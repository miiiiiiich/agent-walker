use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::collector::{
    FileEvents, KeyedDurationEvent, KeyedEffortEvent, KeyedInterruptEvent, KeyedModeEvent,
    KeyedPermissionEvent, KeyedToolEvent, KeyedUsageEvent, list_files, merge_into,
    parse_files_cached, project_from_cwd,
};
use crate::model::{
    Collection, DurationEvent, EffortEvent, InterruptEvent, ModeEvent, PermissionEvent, Provider,
    SessionTouch, SourceKind, TokenUsage, ToolEvent, UsageEvent,
};

/// Background/observer harnesses (e.g. the claude-mem observer) keep
/// always-on sessions that would inflate session and active-day stats.
const NOISE_DIR_MARKERS: [&str; 1] = ["claude-mem-observer"];

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Claude, root.to_path_buf());
    if !root.exists() {
        return collection;
    }

    let files: Vec<_> = list_files(root, "jsonl", mtime_floor, &mut collection.stats)
        .into_iter()
        .filter(|path| !is_noise_path(path))
        .collect();
    let per_file = parse_files_cached(
        use_cache.then_some("claude"),
        &files,
        local_offset,
        |path| parse_file(path, root, local_offset),
    );
    merge_into(&mut collection, per_file);
    collection
}

fn is_noise_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| NOISE_DIR_MARKERS.iter().any(|marker| name.contains(marker)))
    })
}

/// Fallback project label from the sanitized directory name. Claude Code
/// flattens "/" and "." to "-" in directory names (irreversibly), so this is
/// only used for files whose lines carry no raw `cwd` field.
fn project_label(path: &Path, root: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let directory = relative.components().next()?.as_os_str().to_str()?;
    Some(normalize_project_name(directory))
}

fn normalize_project_name(directory: &str) -> String {
    // Claude Code names its project directories by flattening the cwd with
    // every path separator replaced by `-`. Strip the home prefix so the label
    // is a relative project path. Exact on macOS / Linux. Windows-native
    // Claude Code's flattening rule isn't confirmed (especially whether the
    // drive-letter `:` is dropped or rewritten), so we sanitize the home like
    // the directory name itself — replacing `:` as well as the separators —
    // and trim any trailing separator first so a home such as `/home/me/` or
    // `D:\` doesn't yield a double-hyphen prefix. When the flattened prefix
    // still doesn't match we fall back to the leading-`-` trim; the cwd field
    // in the session is preferred as the project label whenever present.
    let home_prefix = crate::paths::home_dir()
        .ok()
        .and_then(|home| home.to_str().map(ToOwned::to_owned))
        .map(|home| {
            let trimmed = home.trim_end_matches(['/', '\\']);
            format!("{}-", trimmed.replace([':', '/', '\\'], "-"))
        })
        .unwrap_or_default();
    let trimmed = directory
        .strip_prefix(&home_prefix)
        .unwrap_or(directory)
        .trim_start_matches('-');
    trimmed.to_owned()
}

fn parse_file(path: &Path, root: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    let file = File::open(path).ok()?;
    let mut events = FileEvents::default();
    let source_kind = path_source_kind(path);
    let file_agent_id = file_agent_id(path);
    // Prefer the raw `cwd` carried on log lines (real slashes); the sanitized
    // directory name is a lossy fallback.
    let mut project = None;
    let fallback_project = project_label(path, root);
    // Claude logs carry no explicit completion event; derive turn durations
    // as "human prompt -> last activity before the next human prompt".
    let mut turn_start: Option<OffsetDateTime> = None;
    let mut last_activity: Option<OffsetDateTime> = None;
    let reader = BufReader::new(file);

    for (line_index, line) in reader.lines().enumerate() {
        events.lines_seen += 1;
        let Ok(line) = line else {
            events.parse_errors += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            events.parse_errors += 1;
            continue;
        };
        if project.is_none()
            && let Some(cwd) = string_field(&value, "cwd")
        {
            project = Some(project_from_cwd(&cwd));
        }
        if source_kind == SourceKind::Main
            && let Some(timestamp) = parse_timestamp(value.get("timestamp"))
        {
            if is_interrupt_marker(&value) {
                // The active turn was aborted: discard it (completion stats
                // count completed turns only, matching Codex where
                // `turn_aborted` never reaches the durations) and don't
                // start a bogus turn from the marker row itself.
                turn_start = None;
                last_activity = None;
            } else if is_human_turn(&value) {
                push_turn_duration(turn_start, last_activity, &mut events);
                turn_start = Some(timestamp);
                last_activity = Some(timestamp);
            } else if let Some(previous) = last_activity {
                // A long silence means the turn ended and the session was
                // resumed later (compaction, scheduled appends); close the
                // turn at the last real activity instead of spanning days.
                if timestamp - previous > Duration::minutes(30) {
                    push_turn_duration(turn_start, last_activity, &mut events);
                    turn_start = None;
                    last_activity = None;
                } else {
                    last_activity = Some(previous.max(timestamp));
                }
            }
        }
        parse_line(
            &value,
            path,
            line_index,
            source_kind,
            file_agent_id.as_deref(),
            project.as_deref().or(fallback_project.as_deref()),
            &mut events,
        );
    }

    push_turn_duration(turn_start, last_activity, &mut events);
    events.compress_touches(local_offset);
    Some(events)
}

/// A line that starts a human turn: a user message that is an actual prompt,
/// not a `tool_result` carrier or meta record.
fn is_human_turn(value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if value
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .get("isMeta")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return false;
    }
    match value
        .get("message")
        .and_then(|message| message.get("content"))
    {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => {
            let has_text = blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("text"));
            let has_tool_result = blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"));
            has_text && !has_tool_result
        }
        _ => false,
    }
}

fn push_turn_duration(
    turn_start: Option<OffsetDateTime>,
    last_activity: Option<OffsetDateTime>,
    events: &mut FileEvents,
) {
    let (Some(start), Some(end)) = (turn_start, last_activity) else {
        return;
    };
    let duration_ms = u64::try_from((end - start).whole_milliseconds()).unwrap_or(0);
    if duration_ms == 0 {
        return;
    }
    events.duration_events.push(KeyedDurationEvent {
        key: None,
        event: DurationEvent {
            timestamp: Some(start),
            session_id: None,
            duration_ms,
            status: Some("turn".to_owned()),
        },
    });
}

#[allow(
    clippy::too_many_arguments,
    reason = "Per-line parse context; bundling into a struct adds noise for one caller."
)]
fn parse_line(
    value: &Value,
    path: &Path,
    line_index: usize,
    source_kind: SourceKind,
    file_agent_id: Option<&str>,
    project: Option<&str>,
    events: &mut FileEvents,
) {
    let timestamp = parse_timestamp(value.get("timestamp"));
    let session_id = string_field(value, "sessionId")
        .or_else(|| string_field(value, "session_id"))
        .or_else(|| string_field(value, "session_id_v2"));
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
                reported_cost_usd: None,
            },
        });
    }

    // `usage.iterations` (log-schema addition, 2026-04) breaks one turn into
    // its underlying API calls. The top level is the turn's BILLED usage for
    // the serving model: a failed fallback attempt is not billed (fallback
    // credit refunds the switch) and the turn is billed as the serving model
    // alone, and on advisor turns the top level already sums the main-model
    // iterations. So main-model `message` and `fallback_message` entries must
    // never be re-emitted — only `advisor_message` entries are additional
    // billed calls, made under their own model and absent from the top-level
    // counters (ccusage#1115 lost them entirely). Keyed per iteration index
    // so streamed duplicates of the message still dedupe.
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
                    reported_cost_usd: None,
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

/// One mode event per assistant message (keyed by message id): did extended
/// thinking fire (a `thinking` content block exists — presence only, the text
/// is never read), and did fast mode serve it (`usage.speed == "fast"`).
/// Streaming duplicates of the same message merge with OR in `merge_into`.
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

/// A main-thread row the harness writes when the user hits esc: a user row
/// whose content IS one of the two complete marker forms the harness
/// emits (the only variants across real logs) — a prompt that merely
/// quotes or starts with the marker text must not count or clear a turn.
/// `isMeta` rows are excluded because agent messages QUOTING the marker
/// would otherwise count. `isSidechain` rows are excluded because one esc
/// against a parallel team fans out as marker echoes into every subagent
/// transcript (bursts of 10-16 observed — counting them would overstate
/// interruptions ~1.8x, load-dependently). The trade-off: an esc recorded
/// only in sidechain files (~14% of esc moments) is deliberately not
/// counted — the same turn-level ruling as Codex, where `turn_aborted`
/// is used and `sub_agent_activity: interrupted` is discarded.
fn is_interrupt_marker(value: &Value) -> bool {
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
    match value
        .get("message")
        .and_then(|message| message.get("content"))
    {
        Some(Value::String(text)) => is_marker(text),
        Some(Value::Array(blocks)) => blocks.iter().any(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(is_marker)
        }),
        _ => false,
    }
}

/// One interrupt event per main-thread esc (`interruptedMessageId` rows are
/// a strict subset of marker rows, so the marker alone carries the count).
/// Subagent-file rows are excluded by file provenance too, not only by the
/// row's `isSidechain` flag. Keyed by the row uuid, which resume/fork
/// copies share.
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

/// One effort event per assistant message (keyed by message id), from the
/// top-level `effort` field Claude Code writes since v2.1.212 (2026-07-17).
/// Older lines lack the field and contribute nothing; subagent (sidechain)
/// messages carry it too, so the mix covers delegated turns.
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

fn parse_timestamp(value: Option<&Value>) -> Option<OffsetDateTime> {
    let raw = value?.as_str()?;
    OffsetDateTime::parse(raw, &Rfc3339).ok()
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Token counts are untrusted log data; clamp to a generous sanity bound so
/// downstream sums of a handful of fields can never overflow u64.
fn u64_field(value: &Value, key: &str) -> u64 {
    const MAX_SANE_TOKENS: u64 = 1 << 50;
    value
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_SANE_TOKENS)
}

fn path_source_kind(path: &Path) -> SourceKind {
    if path
        .components()
        .any(|component| component.as_os_str() == "subagents")
    {
        SourceKind::Subagent
    } else {
        SourceKind::Main
    }
}

fn file_agent_id(path: &Path) -> Option<String> {
    let file_name = path.file_stem()?.to_str()?;
    file_name
        .strip_prefix("agent-")
        .map(|id| format!("agent-{id}"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn collects_usage_tools_and_subagent_files() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let project_dir = temp.path().join("project");
        let subagent_dir = project_dir.join("subagents");
        fs::create_dir_all(&subagent_dir).expect("test dirs should be created");

        fs::write(
            project_dir.join("session.jsonl"),
            r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30},"content":[{"type":"tool_use","name":"Agent","input":{"subagent_type":"Explore"}},{"type":"tool_use","name":"Read","input":{"file_path":"/secret"}}]}}"#,
        )
        .expect("main fixture should be written");
        fs::write(
            subagent_dir.join("agent-abc.jsonl"),
            r#"{"timestamp":"2026-06-01T00:01:00Z","sessionId":"s1","isSidechain":true,"attributionAgent":"Explore","message":{"id":"m2","model":"claude-haiku-4-5","usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":10},"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo hidden"}}]}}"#,
        )
        .expect("subagent fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 2);
        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(collection.tool_events.len(), 3);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 63);
        assert_eq!(collection.usage_events[1].source_kind, SourceKind::Subagent);
        assert_eq!(
            collection.tool_events[0].subagent_type.as_deref(),
            Some("Explore")
        );
    }

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

    /// The ordinary shape — one main-model `message` iteration mirroring the
    /// top-level numbers — must not create a second event.
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
        // One mode event per assistant message: thinking+fast, then neither.
        assert_eq!(collection.mode_events.len(), 2);
        assert!(collection.mode_events[0].has_thinking);
        assert!(collection.mode_events[0].fast);
        assert!(!collection.mode_events[1].has_thinking);
        assert!(!collection.mode_events[1].fast);
    }

    /// Interrupt markers count once per esc: duplicate uuids (resume/fork
    /// copies) dedupe, block-content markers count, and a row carrying only
    /// `interruptedMessageId` without the marker does not count (real logs
    /// show such rows always carry the marker too).
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
                "\n"
            ),
        )
        .expect("fixture should be written");
        let subagent_dir = temp.path().join("project").join("subagents");
        fs::create_dir_all(&subagent_dir).expect("test dirs should be created");
        // A subagent-file echo that omits the redundant `isSidechain` flag:
        // file provenance alone must exclude it.
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

    /// An interrupted turn is discarded from completion durations (matching
    /// Codex, where `turn_aborted` never reaches the durations), and the
    /// marker row does not start a bogus turn of its own.
    #[test]
    fn interrupted_turns_are_excluded_from_durations() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"do the thing"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"working"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:02:00Z","sessionId":"s1","type":"user","uuid":"i1","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"try again"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:06:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:07:00Z","sessionId":"s1","type":"user","uuid":"h3","message":{"role":"user","content":"thanks"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.interrupt_events.len(), 1);
        // Only the completed turn survives ("try again" 00:05 -> last
        // activity "done" 00:06 = 60s); the aborted first turn and the
        // marker row contribute no durations.
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 60_000);
    }

    /// The top-level `permissionMode` field on user rows becomes one
    /// permission event per turn; resume/fork copies share the row uuid and
    /// dedupe, and rows without the field contribute nothing.
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

    /// The top-level `effort` field (present since CLI v2.1.212) becomes one
    /// effort event per assistant message; duplicate lines for the same
    /// message dedupe by id, and lines without the field contribute nothing.
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

        // One deduped usage event: the big line's tokens, the small line's skill.
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.input_tokens, 100);
        assert_eq!(
            collection.usage_events[0].attribution_skill.as_deref(),
            Some("sk:review")
        );
        // One deduped mode event with OR-merged thinking.
        assert_eq!(collection.mode_events.len(), 1);
        assert!(collection.mode_events[0].has_thinking);
    }

    #[test]
    fn skips_malformed_lines_without_aborting() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            "not-json\n{\"timestamp\":\"2026-06-01T00:00:00Z\",\"sessionId\":\"s1\",\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-4-8\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n",
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(collection.usage_events.len(), 1);
    }

    #[test]
    fn deduplicates_usage_and_tool_use_ids() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let line = r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30},"content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/secret"}}]}}"#;
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

    #[test]
    fn skips_files_older_than_mtime_floor() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("old.jsonl"),
            r#"{"timestamp":"2026-01-01T00:00:00Z","sessionId":"old","message":{"id":"m0","usage":{"input_tokens":1,"output_tokens":1}}}"#,
        )
        .expect("fixture should be written");

        let future = SystemTime::now() + std::time::Duration::from_hours(1);
        let collection = collect(temp.path(), Some(future), false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 0);
        assert!(collection.usage_events.is_empty());
    }
}
