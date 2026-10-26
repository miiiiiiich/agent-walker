mod line;
mod turns;

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::collector::{
    FileEvents, KeyedPaceEvent, list_files, merge_into, parse_files_cached, project_from_cwd,
};
use crate::model::{Collection, PaceEvent, Provider, SourceKind};

use line::{is_interrupt_marker, parse_line};
use turns::{TurnState, is_human_turn};

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
    // Trim and normalize the home prefix so a trailing separator cannot produce a double hyphen.
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
    let mut turn = TurnState::default();
    // Resumes and fork prefixes replay earlier rows verbatim, sometimes
    // in the middle of a later turn. A prompt uuid seen before must not
    // flush and restart the turn, and a question id seen before must not
    // reopen (its replayed answer then finds nothing pending and is inert).
    let mut seen_prompts: HashSet<String> = HashSet::new();
    let mut seen_questions: HashSet<String> = HashSet::new();
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
        if source_kind == SourceKind::Main {
            if is_interrupt_marker(&value) {
                // Clear an interrupted turn before timestamp validation; an undated
                // marker must also prevent a completed-duration event.
                turn = TurnState::default();
            } else if let Some(timestamp) = parse_timestamp(value.get("timestamp")) {
                if is_human_turn(&value) {
                    let key = string_field(&value, "uuid");
                    let replayed = key
                        .as_ref()
                        .is_some_and(|uuid| !seen_prompts.insert(uuid.clone()));
                    if !replayed {
                        // The gap from the previous turn's last activity to
                        // this prompt is the human's pace — only while the
                        // turn is still open (a cutoff means they were away).
                        if let (Some(_), Some(end)) = (turn.start, turn.last_activity)
                            && timestamp > end
                            && timestamp - end <= Duration::minutes(30)
                        {
                            let gap_ms =
                                u64::try_from((timestamp - end).whole_milliseconds()).unwrap_or(0);
                            events.pace_events.push(KeyedPaceEvent {
                                key: key.as_ref().map(|uuid| format!("claude-pace:{uuid}")),
                                event: PaceEvent {
                                    timestamp: Some(timestamp),
                                    gap_ms,
                                },
                            });
                        }
                        turn.flush(&mut events);
                        turn = TurnState::opened_at(&value, timestamp, key);
                    }
                } else if let Some(previous) = turn.last_activity {
                    if timestamp - previous > Duration::minutes(30) {
                        // A long silence means the turn ended and the session
                        // was resumed later (compaction, scheduled appends);
                        // close the turn at the last real activity instead of
                        // spanning days. A question left open that long ended
                        // the turn when it was asked (`flush` charges the
                        // tail as waiting), and its late answer is a fresh
                        // human input: the next turn starts there.
                        turn.flush(&mut events);
                        turn = turn.after_cutoff();
                        turn.open_from(&value, timestamp, &mut seen_prompts);
                    } else if turn.charge_answer(&value, timestamp) {
                        turn.last_activity = Some(previous.max(timestamp));
                    } else {
                        let is_assistant =
                            value.get("type").and_then(Value::as_str) == Some("assistant");
                        turn.responded |= is_assistant;
                        // An assistant row while a question is still open
                        // (a replay, or a parallel tool call answered first)
                        // is inside the human's wait, not the model's time.
                        if is_assistant && timestamp > previous && turn.pending_questions.is_empty()
                        {
                            let gap = u64::try_from((timestamp - previous).whole_milliseconds())
                                .unwrap_or(0);
                            turn.model_ms = turn.model_ms.saturating_add(gap);
                        }
                        turn.note_questions(&value, timestamp, &mut seen_questions);
                        turn.open_if_generated(&value, timestamp, &mut seen_prompts);
                        turn.last_activity = Some(previous.max(timestamp));
                    }
                } else {
                    turn.open_from(&value, timestamp, &mut seen_prompts);
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

    turn.flush(&mut events);
    events.compress_touches(local_offset);
    Some(events)
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

fn session_id_field(value: &Value) -> Option<String> {
    string_field(value, "sessionId")
        .or_else(|| string_field(value, "session_id"))
        .or_else(|| string_field(value, "session_id_v2"))
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
            r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30},"content":[{"type":"tool_use","name":"Agent","input":{"subagent_type":"Explore"}},{"type":"tool_use","name":"Read","input":{"file_path":"/secret"}}]}}"#,
        )
        .expect("main fixture should be written");
        fs::write(
            subagent_dir.join("agent-abc.jsonl"),
            r#"{"timestamp":"2026-06-01T00:01:00Z","sessionId":"s1","isSidechain":true,"attributionAgent":"Explore","type":"assistant","message":{"id":"m2","model":"claude-haiku-4-5","usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":10},"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo hidden"}}]}}"#,
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

    #[test]
    fn skips_malformed_lines_without_aborting() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            "not-json\n{\"timestamp\":\"2026-06-01T00:00:00Z\",\"sessionId\":\"s1\",\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-4-8\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n",
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(collection.usage_events.len(), 1);
    }

    #[test]
    fn skips_files_older_than_mtime_floor() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("old.jsonl"),
            r#"{"timestamp":"2026-01-01T00:00:00Z","sessionId":"old","type":"assistant","message":{"id":"m0","usage":{"input_tokens":1,"output_tokens":1}}}"#,
        )
        .expect("fixture should be written");

        let future = SystemTime::now() + std::time::Duration::from_hours(1);
        let collection = collect(temp.path(), Some(future), false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 0);
        assert!(collection.usage_events.is_empty());
    }
}
