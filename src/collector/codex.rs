use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::collector::{
    FileEvents, list_files_matching, merge_into, parse_files_cached, project_from_cwd,
};
use crate::model::{Collection, Provider, SessionTouch};

mod fork;
mod rollout;
mod tools;
mod turns;
mod usage;

use fork::{Origin, in_fork_burst, is_copied_fork};
use rollout::{
    Line, MAX_LINE_BYTES, is_rollout_file, next_line, open_rollout, plain_rollout_path, supersedes,
};
use tools::collect_tool_event;
use turns::{
    PaceState, TurnItems, collect_duration_event, collect_effort_event, collect_interrupt_event,
    collect_pace_event, collect_permission_event,
};
use usage::{UsageContext, collect_usage_event, collect_usage_record, session_model};

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Codex, root.to_path_buf());

    // Codex *moves* (not copies) a session's JSONL from `sessions/` to the
    // sibling `archived_sessions/` when the desktop app archives it, so a
    // sessions-only scan silently drops archived history. Scan both. Resolve the
    // sibling from the canonical path so a relative root (e.g. `.`) still finds
    // `../archived_sessions`, falling back to the raw parent when the path can't
    // be canonicalized (root missing).
    let archived = root
        .canonicalize()
        .ok()
        .as_deref()
        .unwrap_or(root)
        .parent()
        .map(|parent| parent.join("archived_sessions"));

    // Parse one physical file per relative plain path (docs/codex.md) so
    // Codex durations (emitted without keys) and session touches are not
    // duplicated during merging.
    let mut chosen: HashMap<PathBuf, PathBuf> = HashMap::new();
    for dir in std::iter::once(root).chain(archived.as_deref()) {
        if !dir.exists() {
            continue;
        }
        for path in list_files_matching(dir, &is_rollout_file, mtime_floor, &mut collection.stats) {
            let rel = plain_rollout_path(path.strip_prefix(dir).unwrap_or(&path));
            match chosen.get_mut(&rel) {
                Some(existing) if supersedes(&path, existing) => *existing = path,
                Some(_) => {}
                None => {
                    chosen.insert(rel, path);
                }
            }
        }
    }
    if chosen.is_empty() {
        return collection;
    }
    let mut files: Vec<PathBuf> = chosen.into_values().collect();
    files.sort();

    let per_file = parse_files_cached(use_cache.then_some("codex"), &files, local_offset, |path| {
        parse_file(path, local_offset)
    });
    merge_into(&mut collection, per_file);
    collection
}

#[allow(
    clippy::too_many_lines,
    reason = "One pass over the rollout feeding every collector; splitting adds indirection without logic."
)]
fn parse_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    let mut reader = open_rollout(path)?;
    // Keys and fallback ids come from the plain name, so a rollout parses the
    // same whichever representation is on disk this run.
    let logical_path = plain_rollout_path(path);
    let mut events = FileEvents::default();
    let mut current_session_id = fallback_session_id(&logical_path);
    let mut pace_state = PaceState::default();
    let mut turn_items = TurnItems::default();
    let mut current_model = None;
    let mut current_project = None;
    let mut session_meta_count = 0usize;
    let mut fork_instant: Option<OffsetDateTime> = None;
    let mut own_thread_id: Option<String> = None;
    let mut origin = Origin::default();
    let mut recorded_usage: HashSet<String> = HashSet::new();
    let mut line = Vec::new();

    for line_index in 0.. {
        let read = next_line(reader.as_mut(), &mut line, MAX_LINE_BYTES);
        if matches!(read, Ok(None)) {
            break;
        }
        events.lines_seen += 1;
        // A broken zstd frame returns the same error on every read; skipping
        // it like a bad line would loop forever.
        let Ok(Some(read)) = read else {
            events.parse_errors += 1;
            break;
        };
        if matches!(read, Line::Oversized) {
            events.parse_errors += 1;
            continue;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            events.parse_errors += 1;
            continue;
        };

        let timestamp = parse_timestamp(value.get("timestamp"));
        let is_session_meta = value.get("type").and_then(Value::as_str) == Some("session_meta");
        if is_session_meta {
            session_meta_count += 1;
            if session_meta_count == 1 {
                own_thread_id = string_path(&value, &["payload", "id"]);
                origin = Origin::from_session_meta(&value);
                if is_copied_fork(&value) {
                    fork_instant = timestamp;
                }
            } else if session_meta_count == 2 {
                fork_instant = timestamp;
            }
            current_session_id = string_path(&value, &["payload", "id"]).or(current_session_id);
            current_model = session_model(&value).or(current_model);
            current_project =
                string_path(&value, &["payload", "cwd"]).map(|cwd| project_from_cwd(&cwd));
        }
        // Fork/spawn copies rewrite timestamps to the fork instant and shift
        // line positions (GH-36). Skip the replay burst even without a parent
        // in the scan; content keys deduplicate remaining replay when present.
        // Rows naming this thread as their owner (its own `session_meta`, its
        // own usage records) are never copies, whatever their timestamp.
        let own_session_meta = is_session_meta && session_meta_count == 1;
        let own_record = value.get("type").and_then(Value::as_str) == Some("token_usage_record")
            && own_thread_id.is_some()
            && string_path(&value, &["payload", "thread_id"]) == own_thread_id;
        let in_replay_burst = !own_session_meta
            && !own_record
            && fork_instant
                .zip(timestamp)
                .is_some_and(|(fork, ts)| in_fork_burst(fork, ts));
        if value.get("type").and_then(Value::as_str) == Some("turn_context") {
            current_model = string_path(&value, &["payload", "model"]).or(current_model);
            if !in_replay_burst {
                collect_effort_event(
                    &value,
                    timestamp,
                    current_session_id.as_ref(),
                    line_index,
                    &mut events,
                );
                collect_permission_event(
                    &value,
                    timestamp,
                    current_session_id.as_ref(),
                    line_index,
                    &mut events,
                );
            }
        }
        if in_replay_burst {
            continue;
        }

        if let (Some(timestamp), Some(session_id)) = (timestamp, current_session_id.as_ref()) {
            events.session_touches.push(SessionTouch {
                timestamp,
                session_id: session_id.clone(),
            });
        }

        let usage_context = UsageContext {
            timestamp,
            session_id: current_session_id.as_ref(),
            model: current_model.as_ref(),
            project: current_project.as_deref(),
            origin: &origin,
            line_index,
        };
        collect_usage_record(
            &value,
            &usage_context,
            own_thread_id.as_deref(),
            &mut recorded_usage,
            &mut events,
        );
        collect_usage_event(&value, &usage_context, &recorded_usage, &mut events);
        turn_items.note(&value, timestamp);
        collect_duration_event(
            &value,
            timestamp,
            current_session_id.as_ref(),
            &mut turn_items,
            &mut events,
        );
        collect_interrupt_event(&value, timestamp, current_session_id.as_ref(), &mut events);
        collect_pace_event(
            &value,
            timestamp,
            current_session_id.as_ref(),
            &mut pace_state,
            &mut events,
        );
        collect_tool_event(
            &value,
            timestamp,
            current_session_id.as_ref(),
            &origin,
            &logical_path,
            line_index,
            &mut events,
        );
    }

    events.compress_touches(local_offset);
    Some(events)
}

fn parse_timestamp(value: Option<&Value>) -> Option<OffsetDateTime> {
    let raw = value?.as_str()?;
    OffsetDateTime::parse(raw, &Rfc3339).ok()
}

fn string_path(value: &Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    cursor
        .as_str()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn u64_path(value: &Value, path: &[&str]) -> Option<u64> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    cursor.as_u64()
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

fn fallback_session_id(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn collects_codex_token_count_tools_and_duration() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:02Z","type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"exec_command","arguments":"{}"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"reasoning_output_tokens":3,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:04Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":12345}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"turn_aborted","duration_ms":99999}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].model.as_deref(), Some("gpt-5.5"));
        assert_eq!(collection.usage_events[0].usage.input_tokens, 60);
        assert_eq!(collection.usage_events[0].usage.cache_read_input_tokens, 40);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 110);
        assert_eq!(collection.tool_events[0].tool_name, "exec_command");
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 12_345);
    }

    #[test]
    fn scans_sibling_archived_sessions() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let sessions_day = temp.path().join("sessions/2026/06/01");
        let archived_day = temp.path().join("archived_sessions/2026/06/01");
        fs::create_dir_all(&sessions_day).expect("test dirs should be created");
        fs::create_dir_all(&archived_day).expect("test dirs should be created");
        fs::write(
            sessions_day.join("rollout-active.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");
        fs::write(
            archived_day.join("rollout-archived.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s2","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200,"cached_input_tokens":0,"output_tokens":20,"total_tokens":220}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(&temp.path().join("sessions"), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 2);
        assert_eq!(collection.usage_events.len(), 2);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 330); // 110 (active) + 220 (archived)
    }

    /// A session present in *both* dirs (a stale `sessions/` copy left after an
    /// archive) must not double-count — the keyed events dedupe it to one turn.
    #[test]
    fn dedups_session_present_in_sessions_and_archive() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let sessions_day = temp.path().join("sessions/2026/06/01");
        let archived_day = temp.path().join("archived_sessions/2026/06/01");
        fs::create_dir_all(&sessions_day).expect("test dirs should be created");
        fs::create_dir_all(&archived_day).expect("test dirs should be created");
        let lines = concat!(
            r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
            "\n"
        );
        fs::write(sessions_day.join("rollout-s1.jsonl"), lines).expect("fixture should be written");
        fs::write(archived_day.join("rollout-s1.jsonl"), lines).expect("fixture should be written");

        let collection = collect(&temp.path().join("sessions"), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 110);
        assert_eq!(collection.session_touches.len(), 2);
    }

    pub(super) fn total_volume(collection: &Collection) -> u64 {
        collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum()
    }

    pub(super) fn usage_json(input: u64, cached: u64, output: u64) -> String {
        format!(
            r#"{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{total}}}"#,
            total = input + output,
        )
    }

    pub(super) fn meta_line(ts: &str, id: &str, source: &str, extra: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"session_meta","payload":{{"id":"{id}","cwd":"/tmp/p","source":{source},"model_provider":"openai"{extra}}}}}"#
        )
    }

    pub(super) fn record_line(ts: &str, thread: &str, response: &str, usage: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"token_usage_record","payload":{{"thread_id":"{thread}","turn_id":"t1","session_id":"{thread}","root_turn_id":"t1","response_id":"{response}","usage":{usage},"turn_token_usage":{usage},"thread_token_usage":{usage}}}}}"#
        )
    }

    pub(super) fn token_count_line(ts: &str, last: &str, cumulative_total: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":0,"output_tokens":0,"total_tokens":{cumulative_total}}},"last_token_usage":{last},"model_context_window":258400}}}}}}"#
        )
    }

    pub(super) fn write_lines(path: &Path, lines: &[String]) {
        fs::create_dir_all(path.parent().expect("fixture path has a parent"))
            .expect("test dirs should be created");
        let mut body = lines.join("\n");
        body.push('\n');
        fs::write(path, body).expect("fixture should be written");
    }
}
