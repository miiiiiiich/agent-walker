mod ledger;

use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use crate::collector::ident::ident;
use crate::collector::json::parse_line;
use crate::collector::read::{Lines, open_regular};
use crate::collector::{
    FileEvents, KeyedCreditSample, KeyedDurationEvent, KeyedToolEvent, KeyedUsageEvent, merge_into,
    parse_files_cached, project_from_cwd,
};
use crate::model::{
    Collection, CreditSample, DurationEvent, Provider, SessionTouch, SourceKind, TokenUsage,
    ToolEvent, UsageEvent,
};
use crate::timestamp::parse_rfc3339;

use ledger::{read_call_ledger, supersede_snapshots};

/// Token counts are untrusted log data; clamp to a generous sanity bound so
/// downstream sums of a handful of fields can never overflow u64.
const MAX_SANE_TOKENS: u64 = 1 << 50;

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Copilot, root.to_path_buf());
    let files = event_files(&root.join("session-state"), mtime_floor, &mut collection);
    let db_path = root.join("session-store.db");
    let ledger = read_call_ledger(&db_path, local_offset, &mut collection);
    if files.is_empty() && ledger.is_none() {
        return collection;
    }

    let mut per_file = if files.is_empty() {
        Vec::new()
    } else {
        parse_files_cached(
            use_cache.then_some("copilot"),
            &files,
            local_offset,
            |path| parse_file(path, local_offset),
        )
    };
    if let Some(mut ledger) = ledger {
        ledger.fall_back_where_incomplete(&per_file);
        supersede_snapshots(&mut per_file, &ledger.first_call);
        per_file.push((db_path, Some(ledger.events)));
    }
    merge_into(&mut collection, per_file);
    collection
}

fn event_files(
    sessions: &Path,
    mtime_floor: Option<SystemTime>,
    collection: &mut Collection,
) -> Vec<PathBuf> {
    if !sessions.exists() {
        return Vec::new();
    }
    // Enumerate `<session dir>/events.jsonl` explicitly instead of globbing
    // for the extension: the session directory also holds `files/` snapshots
    // of workspace content, which may themselves be .jsonl.
    let mut files: Vec<PathBuf> = Vec::new();
    match std::fs::read_dir(sessions) {
        Err(_) => {
            // An unopenable session-state must not read as "no Copilot data".
            collection.stats.unreadable_dirs += 1;
        }
        Ok(entries) => {
            for entry in entries {
                let Ok(entry) = entry else {
                    collection.stats.unreadable_files += 1;
                    continue;
                };
                let path = entry.path().join("events.jsonl");
                let meta = match std::fs::metadata(&path) {
                    Ok(meta) => meta,
                    // Absent is the normal case (not every session dir has an
                    // event stream); anything else is a real read failure
                    // worth surfacing in the stats line.
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
    }
    files.sort();
    files
}

fn parse_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    let mut lines = Lines::new(BufReader::new(open_regular(path).ok()?));
    let session_id = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .and_then(ident)
        .map(ToOwned::to_owned);
    let mut events = FileEvents::default();
    let mut project: Option<String> = None;
    let mut cumulative: HashMap<String, RawCounters> = HashMap::new();
    let mut shutdown_index = 0usize;
    let mut last_nano_aiu = 0u64;
    let mut credit_index = 0usize;
    let mut turn_starts: HashMap<String, OffsetDateTime> = HashMap::new();

    while let Some((_, line)) = lines.next(&mut events) {
        let Some(value) = parse_line(line, &mut events) else {
            continue;
        };

        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        let line = Line {
            timestamp,
            session_id: session_id.as_ref(),
            event_id: value.get("id").and_then(Value::as_str).and_then(ident),
        };
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
                    project = project_from_cwd(cwd);
                }
            }
            Some("tool.execution_start") => {
                collect_tool_event(&value, &line, &mut events);
            }
            Some("assistant.turn_start") => {
                if let (Some(turn_id), Some(timestamp)) = (turn_id(&value), timestamp) {
                    turn_starts.insert(turn_id, timestamp);
                }
            }
            Some("assistant.turn_end") => {
                collect_turn_duration(&value, &line, &mut turn_starts, &mut events);
            }
            Some("session.usage_checkpoint") => {
                collect_credit_delta(
                    &value,
                    &line,
                    &mut last_nano_aiu,
                    &mut credit_index,
                    &mut events,
                );
            }
            Some("session.shutdown") => {
                collect_credit_delta(
                    &value,
                    &line,
                    &mut last_nano_aiu,
                    &mut credit_index,
                    &mut events,
                );
                collect_shutdown_usage(
                    &value,
                    &line,
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

/// Keys built from the event `id` collapse a fork's copy of a shutdown,
/// checkpoint, tool call or turn into the parent's original.
struct Line<'a> {
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&'a String>,
    event_id: Option<&'a str>,
}

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

    /// Without subtracting the writes as well as the reads, every cache write
    /// would count twice (as fresh input and as cache creation).
    fn to_usage(self) -> TokenUsage {
        TokenUsage {
            input_tokens: self
                .input
                .saturating_sub(self.cache_read)
                .saturating_sub(self.cache_write),
            output_tokens: self.output,
            reasoning_output_tokens: self.reasoning,
            cache_read_input_tokens: self.cache_read,
            cache_creation_input_tokens: self.cache_write,
            ..TokenUsage::default()
        }
    }
}

fn collect_shutdown_usage(
    value: &Value,
    line: &Line,
    project: Option<&str>,
    cumulative: &mut HashMap<String, RawCounters>,
    shutdown_index: usize,
    events: &mut FileEvents,
) {
    // A shutdown the analyzer cannot place in time must not advance the
    // baseline either — otherwise the usage up to it would be subtracted by
    // the NEXT valid shutdown and lost forever, instead of being recovered
    // there in full.
    if line.timestamp.is_none() {
        return;
    }
    let Some(metrics) = value
        .get("data")
        .and_then(|data| data.get("modelMetrics"))
        .and_then(Value::as_object)
    else {
        return;
    };
    for (model, entry) in metrics {
        let Some(usage) = entry.get("usage").filter(|_| ident(model).is_some()) else {
            continue;
        };
        // A syntactically valid but incomplete usage object would read as
        // all-zero counters, masquerade as an epoch reset, and poison the
        // baseline (1000 → missing → 1100 would count 1000 + 1100). Require
        // the two core counters to be present before trusting the snapshot;
        // a genuine zero (cache-write-only) still carries the fields.
        if usage.get("inputTokens").and_then(Value::as_u64).is_none()
            || usage.get("outputTokens").and_then(Value::as_u64).is_none()
        {
            continue;
        }
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

        let usage = delta.to_usage();
        if usage.token_volume() == 0 {
            continue;
        }
        let key = match (line.event_id, line.session_id) {
            (Some(id), _) => Some(format!("copilot:{id}:{model}")),
            (None, Some(sid)) => Some(format!("copilot:{sid}:{model}:{shutdown_index}")),
            (None, None) => None,
        };
        events.usage_events.push(KeyedUsageEvent {
            key,
            event: UsageEvent {
                timestamp: line.timestamp,
                session_id: line.session_id.cloned(),
                model: Some(model.clone()),
                source_kind: SourceKind::Main,
                attribution_agent: None,
                attribution_skill: None,
                project: project.map(ToOwned::to_owned),
                usage,
                granularity: crate::model::UsageGranularity::Aggregate,
            },
        });
    }
}

fn collect_turn_duration(
    value: &Value,
    line: &Line,
    turn_starts: &mut HashMap<String, OffsetDateTime>,
    events: &mut FileEvents,
) {
    if let (Some(turn_id), Some(end)) = (turn_id(value), line.timestamp)
        // Validate before consuming: a clock-skewed (end < start) record must
        // not eat the start entry, or a later valid end could never pair.
        && turn_starts.get(&turn_id).is_some_and(|start| end >= *start)
        && let Some(start) = turn_starts.remove(&turn_id)
    {
        events.duration_events.push(KeyedDurationEvent {
            key: line.event_id.map(|id| format!("copilot-turn:{id}")),
            event: DurationEvent {
                timestamp: Some(end),
                session_id: line.session_id.cloned(),
                duration_ms: u64::try_from((end - start).whole_milliseconds()).unwrap_or(0),
                human_wait_ms: 0,
                model_ms: None,
                status: Some("turn".to_owned()),
            },
        });
    }
}

fn turn_id(value: &Value) -> Option<String> {
    value
        .get("data")
        .and_then(|data| data.get("turnId"))
        .and_then(Value::as_str)
        .and_then(ident)
        .map(ToOwned::to_owned)
}

fn collect_credit_delta(
    value: &Value,
    line: &Line,
    last_nano_aiu: &mut u64,
    credit_index: &mut usize,
    events: &mut FileEvents,
) {
    let Some(timestamp) = line.timestamp else {
        return;
    };
    // Untrusted log data: clamp like token counts so downstream daily sums
    // can never overflow.
    let Some(current) = value
        .get("data")
        .and_then(|data| data.get("totalNanoAiu"))
        .and_then(Value::as_u64)
        .map(|nano| nano.min(MAX_SANE_TOKENS))
    else {
        return;
    };
    let delta = if current < *last_nano_aiu {
        current
    } else {
        current - *last_nano_aiu
    };
    *last_nano_aiu = current;
    if delta == 0 {
        return;
    }
    let key = match (line.event_id, line.session_id) {
        (Some(id), _) => Some(format!("copilot-credit:{id}")),
        (None, Some(sid)) => Some(format!(
            "copilot-credit:{sid}:{index}",
            index = *credit_index
        )),
        (None, None) => None,
    };
    events.credit_samples.push(KeyedCreditSample {
        key,
        event: CreditSample {
            timestamp,
            nano_aiu: delta,
        },
    });
    *credit_index += 1;
}

fn collect_tool_event(value: &Value, line: &Line, events: &mut FileEvents) {
    let Some(data) = value.get("data") else {
        return;
    };
    let Some(tool_name) = data.get("toolName").and_then(Value::as_str).and_then(ident) else {
        return;
    };
    // Without an event id, namespace tool-call IDs by session because the
    // merge deduplication set is global.
    let key = match (
        line.event_id,
        line.session_id,
        data.get("toolCallId")
            .and_then(Value::as_str)
            .and_then(ident),
    ) {
        (Some(id), _, _) => Some(format!("copilot-tool:{id}")),
        (None, Some(sid), Some(call)) => Some(format!("copilot-tool:{sid}:{call}")),
        _ => None,
    };
    events.tool_events.push(KeyedToolEvent {
        key,
        event: ToolEvent {
            timestamp: line.timestamp,
            session_id: line.session_id.cloned(),
            tool_name: tool_name.to_owned(),
            subagent_type: None,
            source_kind: SourceKind::Main,
        },
    });
}

fn u64_field(value: &Value, key: &str) -> u64 {
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
    use time::format_description::well_known::Rfc3339;

    use super::*;
    use crate::collector::ident::tests::{overlong_ident, overlong_path};
    use crate::collector::read::tests::oversized_json;

    pub(super) fn write_session(root: &Path, session: &str, lines: &str) {
        let dir = root.join("session-state").join(session);
        fs::create_dir_all(&dir).expect("test dirs should be created");
        fs::write(dir.join("events.jsonl"), lines).expect("fixture should be written");
    }

    pub(super) fn total_volume(collection: &Collection) -> u64 {
        collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum()
    }

    pub(super) fn shutdown(id: &str, at: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"type":"session.shutdown","id":"{id}","timestamp":"{at}","data":{{"modelMetrics":{{"gpt-5-mini":{{"usage":{{"inputTokens":{input},"outputTokens":{output},"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}}}}}}"#
        )
    }

    /// Cache writes come out of fresh input, so each counts once.
    #[test]
    fn cache_writes_are_not_counted_twice() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:00:00.000Z","data":{"modelMetrics":{"claude-haiku-4.5":{"usage":{"inputTokens":64359,"outputTokens":209,"cacheReadTokens":63048,"cacheWriteTokens":1306,"reasoningTokens":209}}}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let usage = &collection.usage_events[0].usage;
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.cache_read_input_tokens, 63_048);
        assert_eq!(usage.cache_creation_input_tokens, 1_306);
        assert_eq!(usage.token_volume(), 64_359 + 209);
    }

    /// Without a store, a fork's copies of the parent's events (same event
    /// ids) collapse into the originals.
    #[test]
    fn fork_copies_collapse_by_event_id() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let history = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            r#"{"type":"tool.execution_start","id":"t-1","timestamp":"2026-07-27T11:00:00.000Z","data":{"toolCallId":"call_1","toolName":"bash"}}"#,
            r#"{"type":"assistant.turn_start","id":"ts-1","timestamp":"2026-07-27T11:00:00.000Z","data":{"turnId":"0"}}"#,
            r#"{"type":"assistant.turn_end","id":"te-1","timestamp":"2026-07-27T11:00:05.000Z","data":{"turnId":"0"}}"#,
            r#"{"type":"session.usage_checkpoint","id":"c-1","timestamp":"2026-07-27T11:00:06.000Z","data":{"totalNanoAiu":1000}}"#,
            shutdown("e-1", "2026-07-27T12:00:00.000Z", 1_000, 100),
        );
        write_session(temp.path(), "parent", &history);
        let own_turn = shutdown("e-2", "2026-07-28T12:00:00.000Z", 1_400, 140);
        write_session(temp.path(), "fork", &format!("{history}{own_turn}\n"));

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 1_100 + 440);
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.credit_samples.len(), 1);
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
        assert_eq!(event.usage.input_tokens, 12_630);
        assert_eq!(event.usage.cache_read_input_tokens, 1_536);
        assert_eq!(event.usage.token_volume(), 14_166 + 150);
        assert_eq!(event.usage.reasoning_output_tokens, 128);
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
        assert_eq!(second.usage.token_volume(), 14_240 + 136);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 28_406 + 286);
        let t1 = OffsetDateTime::parse("2026-07-27T12:41:08.275Z", &Rfc3339)
            .expect("test timestamp should parse");
        let t2 = OffsetDateTime::parse("2026-07-27T12:42:15.207Z", &Rfc3339)
            .expect("test timestamp should parse");
        assert_eq!(first.timestamp, Some(t1));
        assert_eq!(second.timestamp, Some(t2));
    }

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

    #[test]
    fn identical_snapshot_reemission_adds_nothing() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let shutdown = r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:00:00.000Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":1000,"outputTokens":100,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#;
        write_session(temp.path(), "s1", &format!("{shutdown}\n{shutdown}\n"));

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 1_100);
    }

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

    /// A malformed line must not abort the file and lose a later shutdown.
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
        assert_eq!(collection.credit_samples.len(), 1);
        assert_eq!(collection.credit_samples[0].nano_aiu, 1_958_555_000);
    }

    #[test]
    fn credit_deltas_accrue_across_checkpoints_and_shutdown() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.usage_checkpoint","timestamp":"2026-07-27T10:00:00.000Z","data":{"totalNanoAiu":1000000000,"totalPremiumRequests":0}}"#,
                "\n",
                r#"{"type":"session.usage_checkpoint","timestamp":"2026-07-27T11:00:00.000Z","data":{"totalNanoAiu":3500000000,"totalPremiumRequests":0}}"#,
                "\n",
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:00:00.000Z","data":{"totalNanoAiu":4000000000,"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":100,"outputTokens":10,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let deltas: Vec<u64> = collection
            .credit_samples
            .iter()
            .map(|sample| sample.nano_aiu)
            .collect();
        assert_eq!(deltas, vec![1_000_000_000, 2_500_000_000, 500_000_000]);
    }

    /// A shutdown without a timestamp cannot be placed in a period — it must
    /// not advance the cumulative baseline, so the next valid exit recovers
    /// the full amount instead of losing everything before the bad record.
    #[test]
    fn shutdown_without_timestamp_does_not_advance_baseline() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.shutdown","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":1000,"outputTokens":100,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#,
                "\n",
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T13:00:00.000Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":1000,"outputTokens":100,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 1_100);
        assert!(collection.usage_events[0].timestamp.is_some());
    }

    /// An incomplete usage object (missing counters) must not masquerade as
    /// an epoch reset and poison the baseline: 1000 → missing → 1100 counts
    /// 1000 + 100, never 1000 + 1100.
    #[test]
    fn incomplete_snapshot_does_not_poison_the_baseline() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:00:00.000Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":1000,"outputTokens":100,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#,
                "\n",
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T12:30:00.000Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{}}}}}"#,
                "\n",
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T13:00:00.000Z","data":{"modelMetrics":{"gpt-5-mini":{"usage":{"inputTokens":1100,"outputTokens":110,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 1_100 + 110);
    }

    #[test]
    fn credit_resets_and_equal_snapshots() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.usage_checkpoint","timestamp":"2026-07-27T10:00:00.000Z","data":{"totalNanoAiu":100}}"#,
                "\n",
                r#"{"type":"session.usage_checkpoint","timestamp":"2026-07-27T10:10:00.000Z","data":{"totalNanoAiu":40}}"#,
                "\n",
                r#"{"type":"session.usage_checkpoint","timestamp":"2026-07-27T10:20:00.000Z","data":{"totalNanoAiu":70}}"#,
                "\n",
                r#"{"type":"session.shutdown","timestamp":"2026-07-27T10:30:00.000Z","data":{"totalNanoAiu":70,"modelMetrics":{}}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let total: u64 = collection
            .credit_samples
            .iter()
            .map(|sample| sample.nano_aiu)
            .sum();
        assert_eq!(total, 170);
        assert_eq!(collection.credit_samples.len(), 3);
    }

    /// A clock-skewed `turn_end` (before its start) must not consume the
    /// start entry — the later valid end still pairs.
    #[test]
    fn skewed_turn_end_does_not_eat_the_start() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"assistant.turn_start","timestamp":"2026-07-27T12:00:10.000Z","data":{"turnId":"0"}}"#,
                "\n",
                r#"{"type":"assistant.turn_end","timestamp":"2026-07-27T12:00:05.000Z","data":{"turnId":"0"}}"#,
                "\n",
                r#"{"type":"assistant.turn_end","timestamp":"2026-07-27T12:00:20.000Z","data":{"turnId":"0"}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 10_000);
    }

    #[test]
    fn turn_durations_pair_start_and_end_by_turn_id() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"assistant.turn_start","timestamp":"2026-07-27T12:00:00.000Z","data":{"turnId":"0"}}"#,
                "\n",
                r#"{"type":"assistant.turn_end","timestamp":"2026-07-27T12:00:08.000Z","data":{"turnId":"0"}}"#,
                "\n",
                r#"{"type":"assistant.turn_end","timestamp":"2026-07-27T12:00:09.000Z","data":{"turnId":"unmatched"}}"#,
                "\n"
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 8_000);
        assert_eq!(
            collection.duration_events[0].status.as_deref(),
            Some("turn")
        );
    }

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

    #[test]
    fn shutdown_dated_past_the_plausible_range_reads_as_undated() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n{}\n",
                shutdown("e-1", "2026-07-27T10:00:00.000Z", 1_000, 100),
                shutdown("e-2", "9999-12-31T23:00:00.000Z", 1_500, 150)
            ),
        );

        let tokyo = UtcOffset::from_hms(9, 0, 0).expect("offset");
        let collection = collect(temp.path(), None, false, tokyo);

        assert_eq!(total_volume(&collection), 1_100);
        assert_eq!(collection.session_touches.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn event_stream_that_is_a_fifo_is_not_opened() {
        use crate::collector::read::tests::{finishes_quickly, make_fifo};

        let temp = TempDir::new().expect("test tempdir should be created");
        let session = temp.path().join("session-state").join("s1");
        fs::create_dir_all(&session).expect("test dirs should be created");
        make_fifo(&session.join("events.jsonl"));

        let root = temp.path().to_path_buf();
        let collection = finishes_quickly(move || collect(&root, None, false, UtcOffset::UTC));

        assert_eq!(collection.stats.unreadable_files, 1);
    }

    #[test]
    fn event_past_the_line_cap_is_skipped_as_a_parse_error() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n{}\n",
                oversized_json(&shutdown("e-1", "2026-07-27T10:00:00.000Z", 1_000, 100)),
                shutdown("e-2", "2026-07-27T11:00:00.000Z", 1_500, 150)
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(total_volume(&collection), 1_650);
    }

    #[test]
    fn overlong_cwd_and_tool_name_are_missing() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let long = overlong_ident();
        let deep = overlong_path();
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n{}\n{}\n",
                format_args!(
                    r#"{{"type":"session.start","id":"e-0","timestamp":"2026-07-27T09:00:00.000Z","data":{{"context":{{"cwd":"{deep}"}}}}}}"#
                ),
                format_args!(
                    r#"{{"type":"tool.execution_start","id":"e-1","timestamp":"2026-07-27T09:30:00.000Z","data":{{"toolName":"{long}","toolCallId":"c1"}}}}"#
                ),
                shutdown("e-2", "2026-07-27T10:00:00.000Z", 1_000, 100)
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert!(collection.tool_events.is_empty());
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].project, None);
    }
}
