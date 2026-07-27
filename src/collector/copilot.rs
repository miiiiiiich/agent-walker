use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::collector::{
    FileEvents, KeyedToolEvent, KeyedUsageEvent, merge_into, parse_files_cached, project_from_cwd,
};
use crate::model::{
    Collection, Provider, SessionTouch, SourceKind, TokenUsage, ToolEvent, UsageEvent,
};

/// GitHub Copilot CLI (`@github/copilot`, the agentic CLI — not the retired
/// `gh copilot` extension) writes one directory per session under
/// `<root>/session-state/<uuid>/`, with an `events.jsonl` event stream.
/// Schema verified live against CLI 1.0.73:
///
/// - Token counts exist ONLY on `session.shutdown`, written on clean exit
///   (`/exit` or non-interactive completion) as per-model cumulative totals
///   (`data.modelMetrics`). A crashed or still-open session has no shutdown
///   and therefore no token data — a documented gap, recovered when the
///   session eventually exits (the cumulative shutdown covers its lifetime).
/// - Resuming appends to the SAME session file and a later clean exit appends
///   ANOTHER shutdown whose totals are cumulative. Each shutdown therefore
///   emits the component-wise DELTA since the previous snapshot, dated at its
///   own exit time — never the raw cumulative. Keeping one cumulative event
///   instead would mis-window resumed sessions: the merged event would carry
///   the latest totals at the EARLIEST exit's date, so a session first closed
///   before the analysis window and resumed today would drop today's usage
///   entirely. A counter going backwards (CLI update / metric reset) starts a
///   new epoch and the snapshot counts in full.
pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Copilot, root.to_path_buf());
    let sessions = root.join("session-state");
    if !sessions.exists() {
        return collection;
    }

    // Enumerate `<session dir>/events.jsonl` explicitly instead of globbing
    // for the extension: the session directory also holds `files/` snapshots
    // of workspace content, which may themselves be .jsonl.
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&sessions) {
        for entry in entries.flatten() {
            let path = entry.path().join("events.jsonl");
            let meta = match std::fs::metadata(&path) {
                Ok(meta) => meta,
                // Absent is the normal case (not every session dir has an
                // event stream); anything else is a real read failure worth
                // surfacing in the stats line.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    collection.stats.unreadable_files += 1;
                    continue;
                }
            };
            if let (Some(floor), Ok(mtime)) = (mtime_floor, meta.modified())
                && mtime < floor
            {
                continue;
            }
            files.push(path);
        }
    }
    if files.is_empty() {
        return collection;
    }
    files.sort();

    let per_file = parse_files_cached(
        use_cache.then_some("copilot"),
        &files,
        local_offset,
        |path| parse_file(path, local_offset),
    );
    merge_into(&mut collection, per_file);
    collection
}

fn parse_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    let file = File::open(path).ok()?;
    // The directory name is the session id (also carried in session.start).
    let session_id = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned);
    let mut events = FileEvents::default();
    let mut project: Option<String> = None;
    let mut cumulative: HashMap<String, RawCounters> = HashMap::new();
    let mut shutdown_index = 0usize;
    let reader = BufReader::new(file);

    for line in reader.lines() {
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

        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|raw| OffsetDateTime::parse(raw, &Rfc3339).ok());
        if let (Some(timestamp), Some(session_id)) = (timestamp, session_id.as_ref()) {
            events.session_touches.push(SessionTouch {
                timestamp,
                session_id: session_id.clone(),
            });
        }

        match value.get("type").and_then(Value::as_str) {
            Some("session.start") => {
                if let Some(cwd) = value
                    .get("data")
                    .and_then(|data| data.get("context"))
                    .and_then(|context| context.get("cwd"))
                    .and_then(Value::as_str)
                {
                    project = Some(project_from_cwd(cwd));
                }
            }
            Some("tool.execution_start") => {
                collect_tool_event(&value, timestamp, session_id.as_ref(), &mut events);
            }
            Some("session.shutdown") => {
                collect_shutdown_usage(
                    &value,
                    timestamp,
                    session_id.as_ref(),
                    project.as_deref(),
                    &mut cumulative,
                    shutdown_index,
                    &mut events,
                );
                shutdown_index += 1;
            }
            _ => {}
        }
    }

    events.compress_touches(local_offset);
    Some(events)
}

/// Raw cumulative counters from one `modelMetrics` entry, as logged:
/// `input` includes `cache_read`, `output` includes `reasoning`.
#[derive(Clone, Copy, Default)]
struct RawCounters {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
}

impl RawCounters {
    fn any_less_than(self, other: Self) -> bool {
        self.input < other.input
            || self.output < other.output
            || self.cache_read < other.cache_read
            || self.cache_write < other.cache_write
            || self.reasoning < other.reasoning
    }

    fn minus(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_sub(other.input),
            output: self.output.saturating_sub(other.output),
            cache_read: self.cache_read.saturating_sub(other.cache_read),
            cache_write: self.cache_write.saturating_sub(other.cache_write),
            reasoning: self.reasoning.saturating_sub(other.reasoning),
        }
    }
}

/// Per-model usage from a `session.shutdown`: the component-wise delta since
/// the previous shutdown snapshot of the same model, so every clean exit is
/// counted once at its own time (see the module doc for why the cumulative
/// must not be kept whole). Field semantics verified against the CLI's own
/// on-screen totals: `inputTokens` INCLUDES `cacheReadTokens` (14,166 =
/// 12,630 fresh + 1,536 cached in the probe session), so fresh input is the
/// difference — the same convention as Codex. `reasoningTokens` is a subset
/// of `outputTokens` and is tracked without being added to the volume.
#[allow(
    clippy::too_many_arguments,
    reason = "Per-line parse context; bundling into a struct adds noise for one caller."
)]
fn collect_shutdown_usage(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    project: Option<&str>,
    cumulative: &mut HashMap<String, RawCounters>,
    shutdown_index: usize,
    events: &mut FileEvents,
) {
    let Some(metrics) = value
        .get("data")
        .and_then(|data| data.get("modelMetrics"))
        .and_then(Value::as_object)
    else {
        return;
    };
    for (model, entry) in metrics {
        let Some(usage) = entry.get("usage") else {
            continue;
        };
        let current = RawCounters {
            input: u64_field(usage, "inputTokens"),
            output: u64_field(usage, "outputTokens"),
            cache_read: u64_field(usage, "cacheReadTokens"),
            cache_write: u64_field(usage, "cacheWriteTokens"),
            reasoning: u64_field(usage, "reasoningTokens"),
        };
        let previous = cumulative.get(model).copied().unwrap_or_default();
        // A counter going backwards means the CLI restarted its accounting
        // (update, epoch change): the snapshot is a fresh cumulative, not a
        // continuation, so it counts in full.
        let base = if current.any_less_than(previous) {
            RawCounters::default()
        } else {
            previous
        };
        let delta = current.minus(base);
        cumulative.insert(model.clone(), current);

        let usage = TokenUsage {
            input_tokens: delta.input.saturating_sub(delta.cache_read),
            output_tokens: delta.output,
            reasoning_output_tokens: delta.reasoning,
            cache_read_input_tokens: delta.cache_read,
            cache_creation_input_tokens: delta.cache_write,
            ..TokenUsage::default()
        };
        // A re-emitted identical snapshot (an exit with no new activity)
        // deltas to zero and adds nothing.
        if usage.token_volume() == 0 {
            continue;
        }
        events.usage_events.push(KeyedUsageEvent {
            key: session_id.map(|sid| format!("copilot:{sid}:{model}:{shutdown_index}")),
            event: UsageEvent {
                timestamp,
                session_id: session_id.cloned(),
                model: Some(model.clone()),
                source_kind: SourceKind::Main,
                attribution_agent: None,
                attribution_skill: None,
                project: project.map(ToOwned::to_owned),
                usage,
                reported_cost_usd: None,
            },
        });
    }
}

fn collect_tool_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    events: &mut FileEvents,
) {
    let Some(data) = value.get("data") else {
        return;
    };
    let Some(tool_name) = data.get("toolName").and_then(Value::as_str) else {
        return;
    };
    // Scoped per session: toolCallId uniqueness across sessions is not
    // guaranteed by anything we verified, and the seen-set in merge_into is
    // global.
    let key = match (session_id, data.get("toolCallId").and_then(Value::as_str)) {
        (Some(sid), Some(id)) => Some(format!("copilot-tool:{sid}:{id}")),
        _ => None,
    };
    events.tool_events.push(KeyedToolEvent {
        key,
        event: ToolEvent {
            timestamp,
            session_id: session_id.cloned(),
            tool_name: tool_name.to_owned(),
            subagent_type: None,
            source_kind: SourceKind::Main,
        },
    });
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_session(root: &Path, session: &str, lines: &str) {
        let dir = root.join("session-state").join(session);
        fs::create_dir_all(&dir).expect("test dirs should be created");
        fs::write(dir.join("events.jsonl"), lines).expect("fixture should be written");
    }

    #[test]
    fn collects_shutdown_usage_tools_and_project() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.start","timestamp":"2026-07-27T12:40:52.292Z","data":{"sessionId":"s1","copilotVersion":"1.0.73","context":{"cwd":"/Users/me/code/app"}}}"#,
                "\n",
                r#"{"type":"tool.execution_start","timestamp":"2026-07-27T12:41:00.000Z","data":{"toolCallId":"call_1","toolName":"web_fetch","model":"gpt-5-mini"}}"#,
                "\n",
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:41:08.275Z","data":{"shutdownType":"routine","modelMetrics":{"gpt-5-mini":{"requests":{"count":1,"cost":0},"usage":{"inputTokens":14166,"outputTokens":150,"cacheReadTokens":1536,"cacheWriteTokens":0,"reasoningTokens":128}}}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        let event = &collection.usage_events[0];
        assert_eq!(event.model.as_deref(), Some("gpt-5-mini"));
        // inputTokens includes cacheReadTokens: fresh = 14,166 - 1,536.
        assert_eq!(event.usage.input_tokens, 12_630);
        assert_eq!(event.usage.cache_read_input_tokens, 1_536);
        assert_eq!(event.usage.token_volume(), 14_166 + 150);
        assert_eq!(event.usage.reasoning_output_tokens, 128);
        // `/Users/me` is not this machine's home, so only the leading slash
        // is trimmed — home stripping is covered by project_from_cwd tests.
        assert_eq!(event.project.as_deref(), Some("Users/me/code/app"));
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.tool_events[0].tool_name, "web_fetch");
    }

    /// A resumed session appends a SECOND shutdown with cumulative totals:
    /// each shutdown must emit only its delta, dated at its own exit — so a
    /// segment inside the analysis window survives even when an earlier exit
    /// falls outside it.
    #[test]
    fn resumed_session_counts_each_segment_at_its_own_exit() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.start","timestamp":"2026-07-27T12:40:52.292Z","data":{"sessionId":"s1","context":{"cwd":"/Users/me/code/app"}}}"#,
                "\n",
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:41:08.275Z","data":{"shutdownType":"routine","modelMetrics":{"gpt-5-mini":{"requests":{"count":1,"cost":0},"usage":{"inputTokens":14166,"outputTokens":150,"cacheReadTokens":1536,"cacheWriteTokens":0,"reasoningTokens":128}}}}}"#,
                "\n",
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:42:15.207Z","data":{"shutdownType":"routine","modelMetrics":{"gpt-5-mini":{"requests":{"count":2,"cost":0},"usage":{"inputTokens":28406,"outputTokens":286,"cacheReadTokens":10240,"cacheWriteTokens":0,"reasoningTokens":256}}}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        let first = &collection.usage_events[0];
        let second = &collection.usage_events[1];
        assert_eq!(first.usage.token_volume(), 14_166 + 150);
        // Second segment: 28,406 − 14,166 input and 286 − 150 output.
        assert_eq!(second.usage.token_volume(), 14_240 + 136);
        // Summing raw cumulatives would have reported 42,572 input.
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 28_406 + 286);
        // Each segment keeps its own exit timestamp.
        let t1 = OffsetDateTime::parse("2026-07-27T12:41:08.275Z", &Rfc3339)
            .expect("test timestamp should parse");
        let t2 = OffsetDateTime::parse("2026-07-27T12:42:15.207Z", &Rfc3339)
            .expect("test timestamp should parse");
        assert_eq!(first.timestamp, Some(t1));
        assert_eq!(second.timestamp, Some(t2));
    }

    /// A cumulative counter going backwards (CLI update / metric reset)
    /// starts a new epoch: the snapshot counts in full, never a negative or
    /// zero delta.
    #[test]
    fn cumulative_reset_counts_new_epoch_in_full() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:00:00.000Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":1000,"outputTokens":100,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#,
                "\n",
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T13:00:00.000Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":400,"outputTokens":40,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 1_100 + 440);
    }

    /// An exit with no new activity re-emits the identical snapshot — the
    /// delta is zero and nothing is added.
    #[test]
    fn identical_snapshot_reemission_adds_nothing() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let shutdown = r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:00:00.000Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":1000,"outputTokens":100,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#;
        write_session(temp.path(), "s1", &format!("{shutdown}\n{shutdown}\n"));

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 1_100);
    }

    /// Cache writes are billed volume even when input/output stay zero.
    #[test]
    fn cache_write_only_snapshot_is_counted() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:00:00.000Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":0,"outputTokens":0,"cacheReadTokens":0,"cacheWriteTokens":500,"reasoningTokens":0}}}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 500);
    }

    /// toolCallId uniqueness across sessions is unverified — the dedup key is
    /// scoped per session so two sessions reusing an id both count.
    #[test]
    fn tool_call_ids_are_scoped_per_session() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let line = r#"{"type":"tool.execution_start","timestamp":"2026-07-27T12:00:00.000Z","data":{"toolCallId":"call_1","toolName":"web_fetch"}}"#;
        write_session(temp.path(), "s1", &format!("{line}\n"));
        write_session(temp.path(), "s2", &format!("{line}\n"));

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.tool_events.len(), 2);
    }

    /// A malformed line is skipped without aborting the file — the shutdown
    /// after it still counts.
    #[test]
    fn malformed_line_does_not_abort_the_file() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                "not json at all\n",
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:00:00.000Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":1000,"outputTokens":100,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(collection.usage_events.len(), 1);
    }

    /// A session that never exited cleanly has no shutdown — and no token
    /// data. It must contribute activity only, not usage.
    #[test]
    fn session_without_shutdown_has_no_usage() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.start","timestamp":"2026-07-23T14:58:00.000Z","data":{"sessionId":"s1","context":{"cwd":"/Users/me/code/app"}}}"#,
                "\n",
                r#"{"type":"session.usage_checkpoint","timestamp":"2026-07-23T14:59:54.634Z","data":{"totalNanoAiu":1958555000,"totalPremiumRequests":0}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 0);
        assert!(!collection.session_touches.is_empty());
    }

    /// Multi-model sessions split per model under one session key space.
    #[test]
    fn multi_model_shutdown_splits_per_model() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:41:08.275Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":100,"outputTokens":10,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}},"claude-sonnet-4.6":{"usage":{"inputTokens":200,"outputTokens":20,"cacheReadTokens":50,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 110 + 220);
    }
}
