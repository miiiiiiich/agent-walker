use std::path::Path;

use serde_json::Value;
use time::OffsetDateTime;

use crate::collector::{
    FileEvents, KeyedEffortEvent, KeyedInterruptEvent, KeyedModeEvent, KeyedPermissionEvent,
    KeyedToolEvent, KeyedUsageEvent,
};
use crate::model::{
    EffortEvent, InterruptEvent, ModeEvent, PermissionEvent, SessionTouch, SourceKind, TokenUsage,
    ToolEvent, UsageEvent,
};

use super::turns::is_human_turn;
use super::{parse_timestamp, session_id_field, string_field, u64_field};

#[allow(
    clippy::too_many_arguments,
    reason = "Per-line parse context; bundling into a struct adds noise for one caller."
)]
pub(in crate::collector::claude) fn parse_line(
    value: &Value,
    path: &Path,
    line_index: usize,
    source_kind: SourceKind,
    file_agent_id: Option<&str>,
    project: Option<&str>,
    events: &mut FileEvents,
) {
    let timestamp = parse_timestamp(value.get("timestamp"));
    let session_id = session_id_field(value);
    if let (Some(timestamp), Some(session_id)) = (timestamp, session_id.as_ref()) {
        events.session_touches.push(SessionTouch {
            timestamp,
            session_id: session_id.clone(),
        });
    }

    let is_sidechain = value
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let event_source_kind = if is_sidechain {
        SourceKind::Subagent
    } else {
        source_kind
    };
    let attribution_agent = string_field(value, "attributionAgent")
        .or_else(|| string_field(value, "attribution_agent"))
        .or_else(|| {
            if event_source_kind == SourceKind::Subagent {
                file_agent_id.map(ToOwned::to_owned)
            } else {
                None
            }
        });
    let attribution_skill = string_field(value, "attributionSkill")
        .or_else(|| string_field(value, "attribution_skill"));

    let Some(message) = value.get("message") else {
        return;
    };
    // Request-log rows (`api-request-blob`) replay earlier tool calls without
    // a timestamp; only transcript rows are a call's own record.
    if !matches!(
        value.get("type").and_then(Value::as_str),
        Some("user" | "assistant")
    ) {
        return;
    }

    collect_mode_event(value, message, timestamp, events);
    collect_effort_event(value, message, timestamp, events);
    collect_permission_event(value, timestamp, events);
    collect_interrupt_event(value, timestamp, source_kind, events);

    let usage_value = message.get("usage").or_else(|| value.get("usage"));
    let message_id = string_field(message, "id");
    let top_model = string_field(message, "model").or_else(|| string_field(value, "model"));
    if let Some(usage) = parse_usage(usage_value) {
        events.usage_events.push(KeyedUsageEvent {
            key: message_id.as_ref().map(|id| format!("message:{id}")),
            event: UsageEvent {
                timestamp,
                session_id: session_id.clone(),
                model: top_model.clone(),
                source_kind: event_source_kind,
                attribution_agent: attribution_agent.clone(),
                attribution_skill: attribution_skill.clone(),
                project: project.map(ToOwned::to_owned),
                usage,
                granularity: crate::model::UsageGranularity::Call,
            },
        });
    }

    // Top-level usage includes main-model iterations; emit only separately billed
    // advisor iterations, keyed by message and iteration index (ccusage#1115).
    if let Some(iterations) = usage_value
        .and_then(|usage| usage.get("iterations"))
        .and_then(Value::as_array)
    {
        for (index, iteration) in iterations.iter().enumerate() {
            if string_field(iteration, "type").as_deref() != Some("advisor_message") {
                continue;
            }
            let Some(usage) = parse_usage(Some(iteration)) else {
                continue;
            };
            events.usage_events.push(KeyedUsageEvent {
                key: message_id
                    .as_ref()
                    .map(|id| format!("message:{id}:iter:{index}")),
                event: UsageEvent {
                    timestamp,
                    session_id: session_id.clone(),
                    model: string_field(iteration, "model").or_else(|| top_model.clone()),
                    source_kind: event_source_kind,
                    attribution_agent: attribution_agent.clone(),
                    attribution_skill: attribution_skill.clone(),
                    project: project.map(ToOwned::to_owned),
                    usage,
                    granularity: crate::model::UsageGranularity::Call,
                },
            });
        }
    }

    collect_tool_events(
        message,
        timestamp,
        session_id.as_ref(),
        event_source_kind,
        path,
        line_index,
        events,
    );
}

fn collect_mode_event(
    value: &Value,
    message: &Value,
    timestamp: Option<OffsetDateTime>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(message_id) = string_field(message, "id") else {
        return;
    };
    let has_thinking = message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("thinking"))
        });
    let fast = message
        .get("usage")
        .and_then(|usage| usage.get("speed"))
        .and_then(Value::as_str)
        == Some("fast");
    events.mode_events.push(KeyedModeEvent {
        key: Some(format!("mode:{message_id}")),
        event: ModeEvent {
            timestamp,
            has_thinking,
            fast,
        },
    });
}

/// One permission event per human turn (keyed by the row uuid, which resume /
/// fork copies share), from the top-level `permissionMode` field. Gated on
/// `is_human_turn`: cross-session agent-message rows (`isMeta`) also carry
/// the field and would let orchestration-heavy windows swamp the mix. The
/// `type:"permission-mode"` change-stream rows are deliberately not used —
/// per-turn values give the distribution, not just the switch points.
fn collect_permission_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    events: &mut FileEvents,
) {
    if !is_human_turn(value) {
        return;
    }
    let Some(mode) = string_field(value, "permissionMode") else {
        return;
    };
    let Some(uuid) = string_field(value, "uuid") else {
        return;
    };
    events.permission_events.push(KeyedPermissionEvent {
        key: Some(format!("claude-permission:{uuid}")),
        event: PermissionEvent { timestamp, mode },
    });
}

/// Count only exact main-thread interruption markers; exclude `isMeta` rows
/// (they can quote the marker) and sidechain echoes, accepting that
/// sidechain-only interruptions are omitted.
pub(in crate::collector::claude) fn is_interrupt_marker(value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let flagged = |field: &str| value.get(field).and_then(Value::as_bool).unwrap_or(false);
    if flagged("isMeta") || flagged("isSidechain") {
        return false;
    }
    let is_marker = |text: &str| {
        matches!(
            text.trim(),
            "[Request interrupted by user]" | "[Request interrupted by user for tool use]"
        )
    };
    // Array content must be SOLELY the marker block (every real marker row
    // is a single-text array) — a prompt attaching an image alongside a
    // quoted marker is a real message, not an esc.
    match value
        .get("message")
        .and_then(|message| message.get("content"))
    {
        Some(Value::String(text)) => is_marker(text),
        Some(Value::Array(blocks)) => match blocks.as_slice() {
            [block] => block
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(is_marker),
            _ => false,
        },
        _ => false,
    }
}

/// Count marker rows alone because `interruptedMessageId` rows are a strict subset.
fn collect_interrupt_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    source_kind: SourceKind,
    events: &mut FileEvents,
) {
    if source_kind != SourceKind::Main {
        return;
    }
    if !is_interrupt_marker(value) {
        return;
    }
    let Some(uuid) = string_field(value, "uuid") else {
        return;
    };
    events.interrupt_events.push(KeyedInterruptEvent {
        key: Some(format!("claude-interrupt:{uuid}")),
        event: InterruptEvent { timestamp },
    });
}

fn collect_effort_event(
    value: &Value,
    message: &Value,
    timestamp: Option<OffsetDateTime>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(effort) = string_field(value, "effort") else {
        return;
    };
    let Some(message_id) = string_field(message, "id") else {
        return;
    };
    events.effort_events.push(KeyedEffortEvent {
        key: Some(format!("claude-effort:{message_id}")),
        event: EffortEvent { timestamp, effort },
    });
}

fn collect_tool_events(
    message: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    source_kind: SourceKind,
    path: &Path,
    line_index: usize,
    events: &mut FileEvents,
) {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return;
    };

    for (block_index, block) in blocks.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(tool_name) = string_field(block, "name") else {
            continue;
        };
        let key = string_field(block, "id").map_or_else(
            || format!("{}:{}:{block_index}", path.display(), line_index + 1),
            |tool_id| format!("tool:{tool_id}"),
        );
        let subagent_type = block
            .get("input")
            .and_then(|input| string_field(input, "subagent_type"));
        events.tool_events.push(KeyedToolEvent {
            key: Some(key),
            event: ToolEvent {
                timestamp,
                session_id: session_id.cloned(),
                tool_name,
                subagent_type,
                source_kind,
            },
        });
    }
}

fn parse_usage(value: Option<&Value>) -> Option<TokenUsage> {
    let value = value?;
    let input_tokens = u64_field(value, "input_tokens");
    let output_tokens = u64_field(value, "output_tokens");
    let cache_creation_input_tokens = u64_field(value, "cache_creation_input_tokens");
    let cache_read_input_tokens = u64_field(value, "cache_read_input_tokens");
    if input_tokens == 0
        && output_tokens == 0
        && cache_creation_input_tokens == 0
        && cache_read_input_tokens == 0
    {
        return None;
    }

    let mut usage = TokenUsage {
        input_tokens,
        output_tokens,
        cache_creation_input_tokens,
        cache_read_input_tokens,
        ..TokenUsage::default()
    };

    if let Some(cache_creation) = value.get("cache_creation") {
        usage.cache_creation_ephemeral_1h_input_tokens =
            u64_field(cache_creation, "ephemeral_1h_input_tokens");
        usage.cache_creation_ephemeral_5m_input_tokens =
            u64_field(cache_creation, "ephemeral_5m_input_tokens");
    }

    if let Some(server_tool_use) = value.get("server_tool_use").and_then(Value::as_object) {
        for (key, child) in server_tool_use {
            if let Some(count) = child.as_u64() {
                usage.server_tool_use.insert(key.clone(), count);
            }
        }
    }

    Some(usage)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use time::UtcOffset;

    use crate::collector::claude::collect;

    /// A mid-turn model fallback: the failed attempt in `usage.iterations`
    /// is NOT billed (the turn is billed as the serving model, mirrored at
    /// the top level), so exactly one event must come out — the top-level
    /// serving call. A streamed duplicate still dedupes.
    #[test]
    fn fallback_attempt_is_not_counted() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let line = r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":2,"output_tokens":2156,"cache_creation_input_tokens":0,"cache_read_input_tokens":313782,"iterations":[{"type":"message","model":"claude-fable-5","input_tokens":2,"output_tokens":601,"cache_creation_input_tokens":991,"cache_read_input_tokens":495675},{"type":"fallback_message","model":"claude-opus-4-8","input_tokens":2,"output_tokens":2156,"cache_creation_input_tokens":0,"cache_read_input_tokens":313782}]},"content":[{"type":"text","text":"hi"}]}}"#;
        fs::write(
            temp.path().join("session.jsonl"),
            format!("{line}\n{line}\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(
            collection.usage_events[0].model.as_deref(),
            Some("claude-opus-4-8")
        );
        assert_eq!(
            collection.usage_events[0].usage.token_volume(),
            2 + 2156 + 313_782
        );
    }

    /// An advisor turn (ccusage#1115 shape): the top level sums the
    /// main-model iterations, while the `advisor_message` in between is an
    /// additional billed call under its own model, absent from the top-level
    /// counters — it must surface as its own event, and the main-model
    /// iterations must not be re-emitted.
    #[test]
    fn advisor_iteration_is_counted_under_its_own_model() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-sonnet-5","usage":{"input_tokens":22,"output_tokens":12,"cache_creation_input_tokens":0,"cache_read_input_tokens":220,"iterations":[{"type":"message","model":"claude-sonnet-5","input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":100},{"type":"advisor_message","model":"claude-opus-4-8","input_tokens":3,"output_tokens":9,"cache_creation_input_tokens":0,"cache_read_input_tokens":50},{"type":"message","model":"claude-sonnet-5","input_tokens":12,"output_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":120}]},"content":[{"type":"text","text":"hi"}]}}"#,
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        let advisor = collection
            .usage_events
            .iter()
            .find(|event| event.model.as_deref() == Some("claude-opus-4-8"))
            .expect("advisor call should be counted");
        assert_eq!(advisor.usage.token_volume(), 3 + 9 + 50);
        let main = collection
            .usage_events
            .iter()
            .find(|event| event.model.as_deref() == Some("claude-sonnet-5"))
            .expect("main turn should be counted once");
        assert_eq!(main.usage.token_volume(), 22 + 12 + 220);
    }

    #[test]
    fn single_mirror_iteration_adds_nothing() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"iterations":[{"type":"message","input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30}]},"content":[{"type":"text","text":"hi"}]}}"#,
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 63);
    }

    #[test]
    fn collects_skill_attribution_and_mode_events() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","attributionSkill":"sk:review","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":10,"output_tokens":3,"speed":"fast"},"content":[{"type":"thinking","thinking":"…"},{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"m2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"plain"}]}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(
            collection.usage_events[0].attribution_skill.as_deref(),
            Some("sk:review")
        );
        assert_eq!(collection.usage_events[1].attribution_skill, None);
        assert_eq!(collection.mode_events.len(), 2);
        assert!(collection.mode_events[0].has_thinking);
        assert!(collection.mode_events[0].fast);
        assert!(!collection.mode_events[1].has_thinking);
        assert!(!collection.mode_events[1].fast);
    }

    #[test]
    fn collects_interrupt_events_from_marker_rows() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"i1","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"i1","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"user","uuid":"i2","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:02:00Z","sessionId":"s1","type":"user","uuid":"i3","interruptedMessageId":"m9","message":{"role":"user","content":"a plain follow-up"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:03:00Z","sessionId":"s1","type":"user","uuid":"i4","isMeta":true,"message":{"role":"user","content":"[Request interrupted by user] quoted in an agent report"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:04:00Z","sessionId":"s1","type":"user","uuid":"i5","message":{"role":"user","content":"the log said [Request interrupted by user] mid-sentence"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"i6","isSidechain":true,"message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:06:00Z","sessionId":"s1","type":"user","uuid":"i7","message":{"role":"user","content":"[Request interrupted by user] what does this marker mean?"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:07:00Z","sessionId":"s1","type":"user","uuid":"i9","message":{"role":"user","content":[{"type":"image","source":{"type":"base64"}},{"type":"text","text":"[Request interrupted by user]"}]}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");
        let subagent_dir = temp.path().join("project").join("subagents");
        fs::create_dir_all(&subagent_dir).expect("test dirs should be created");
        fs::write(
            subagent_dir.join("agent-abc.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:07:00Z","sessionId":"s1","type":"user","uuid":"i8","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n"
            ),
        )
        .expect("subagent fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.interrupt_events.len(), 2);
    }

    /// A request-log row replays tool calls from the request history with
    /// no timestamp; a call whose transcript row is gone must not reappear
    /// from the replay.
    #[test]
    fn request_log_rows_add_no_tool_calls() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            [
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"api-request","id":"r1","querySource":"repl_main_thread","params":{"model":"claude-fable-5"}}"#,
                r#"{"type":"api-request-blob","hash":"b1","sessionId":"s1","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_gone","name":"Read","input":{}},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]}}"#,
                r#"{"timestamp":"2026-07-20T00:00:05Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]}}"#,
                "",
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.tool_events[0].tool_name, "Bash");
        assert!(collection.tool_events[0].timestamp.is_some());
    }

    #[test]
    fn collects_permission_events_deduped_by_uuid() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"u1","permissionMode":"dontAsk","message":{"role":"user","content":"do it"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"u1","permissionMode":"dontAsk","message":{"role":"user","content":"do it"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"user","uuid":"u2","permissionMode":"auto","message":{"role":"user","content":"next"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:02:00Z","sessionId":"s1","type":"user","uuid":"u3","message":{"role":"user","content":"no mode field"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:03:00Z","sessionId":"s1","type":"user","uuid":"u4","isMeta":true,"permissionMode":"bypassPermissions","message":{"role":"user","content":"agent-message injection"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let mut modes: Vec<&str> = collection
            .permission_events
            .iter()
            .map(|event| event.mode.as_str())
            .collect();
        modes.sort_unstable();
        assert_eq!(modes, ["auto", "dontAsk"]);
    }

    #[test]
    fn collects_effort_events_deduped_by_message_id() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"assistant","effort":"xhigh","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":10,"output_tokens":3},"content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:00:01Z","sessionId":"s1","type":"assistant","effort":"xhigh","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":10,"output_tokens":3},"content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","effort":"max","message":{"id":"m2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"deep"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m3","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"old CLI"}]}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let mut efforts: Vec<&str> = collection
            .effort_events
            .iter()
            .map(|event| event.effort.as_str())
            .collect();
        efforts.sort_unstable();
        assert_eq!(efforts, ["max", "xhigh"]);
    }

    /// Streaming duplicates of one message can disagree: the larger-volume
    /// line may lack attribution while a smaller fragment carries it. The
    /// winner keeps its tokens but absorbs the loser's metadata (fill), and
    /// mode flags merge with OR.
    #[test]
    fn duplicate_lines_fill_metadata_instead_of_dropping_it() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":100,"output_tokens":50},"content":[{"type":"text","text":"big"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:01Z","sessionId":"s1","type":"assistant","attributionSkill":"sk:review","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":1,"output_tokens":1},"content":[{"type":"thinking","thinking":"…"}]}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.input_tokens, 100);
        assert_eq!(
            collection.usage_events[0].attribution_skill.as_deref(),
            Some("sk:review")
        );
        assert_eq!(collection.mode_events.len(), 1);
        assert!(collection.mode_events[0].has_thinking);
    }

    #[test]
    fn deduplicates_usage_and_tool_use_ids() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let line = r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30},"content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/secret"}}]}}"#;
        fs::write(
            temp.path().join("session.jsonl"),
            format!("{line}\n{line}\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 63);
    }
}
