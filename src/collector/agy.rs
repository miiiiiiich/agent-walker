//! Antigravity (`agy`) collector — **auto-detected; shows when local data is
//! present.**
//!
//! Two sources: the text logs (`log/*.log`, `history.jsonl`) give the session /
//! tool activity timeline, and the per-conversation SQLite stores
//! (`conversations/*.db`) give the real token usage, model, and project — see
//! [`super::agy_conv`], which decodes the unlabeled `gen_metadata` protobuf and
//! self-verifies the field map. Tokens used to be unavailable (the store was
//! left unparsed), so this collector was activity-only; it now contributes full
//! usage like the others.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::collector::{FileEvents, KeyedToolEvent, list_files, merge_into, parse_files_cached};
use crate::model::{Collection, Provider, SessionTouch, SourceKind, ToolEvent};

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Agy, root.to_path_buf());
    if !root.exists() {
        return collection;
    }

    let mut files = Vec::new();
    let history = root.join("history.jsonl");
    if history.exists() {
        files.push(history);
    }
    let log_dir = root.join("log");
    if log_dir.exists() {
        files.extend(list_files(
            &log_dir,
            "log",
            mtime_floor,
            &mut collection.stats,
        ));
    }

    let per_file = parse_files_cached(use_cache.then_some("agy"), &files, local_offset, |path| {
        parse_file(path, local_offset)
    });
    merge_into(&mut collection, per_file);

    // Real token usage comes from the per-conversation SQLite stores, not the
    // text logs (which only carry activity). The logs above still provide the
    // session/tool timeline; these add tokens, model, and project.
    let usage =
        super::agy_conv::collect_usage(root, mtime_floor, local_offset, &mut collection.stats);
    collection.usage_events.extend(usage);
    collection.stats.usage_events = collection.usage_events.len();
    collection
}

fn parse_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    if path.file_name().is_some_and(|name| name == "history.jsonl") {
        parse_history_file(path, local_offset)
    } else {
        parse_log_file(path, local_offset)
    }
}

fn parse_history_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    let file = File::open(path).ok()?;
    let mut events = FileEvents::default();
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
        // history.jsonl timestamps are Unix epoch milliseconds. A line without a
        // usable timestamp can't be placed on the activity timeline, so count it
        // as a parse error rather than dropping it silently.
        let Some(timestamp) =
            value
                .get("timestamp")
                .and_then(Value::as_i64)
                .and_then(|timestamp_ms| {
                    OffsetDateTime::from_unix_timestamp(timestamp_ms / 1_000).ok()
                })
        else {
            events.parse_errors += 1;
            continue;
        };
        let session_id = string_field(&value, "conversationId")
            .unwrap_or_else(|| format!("history:{}", line_index + 1));
        events.session_touches.push(SessionTouch {
            timestamp,
            session_id,
        });
    }

    events.compress_touches(local_offset);
    Some(events)
}

fn parse_log_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    let file = File::open(path).ok()?;
    let mut events = FileEvents::default();
    let log_session_id = fallback_session_id(path);
    let mut current_conversation_id = None;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        events.lines_seen += 1;
        let Ok(line) = line else {
            events.parse_errors += 1;
            continue;
        };
        let timestamp = parse_log_timestamp(path, &line, local_offset);

        if line.contains("HandleUserInput called with text") {
            let session_id = current_conversation_id
                .clone()
                .or_else(|| log_session_id.clone());
            // Activity only — real tokens/model come from conversations/*.db
            // (see `agy_conv`), so no zero-token usage event is emitted here.
            if let (Some(timestamp), Some(session_id)) = (timestamp, session_id) {
                events.session_touches.push(SessionTouch {
                    timestamp,
                    session_id,
                });
            }
            continue;
        }

        if line.contains("Responding to tool confirmation") {
            current_conversation_id = value_after(&line, "convID=").or(current_conversation_id);
            let session_id = current_conversation_id
                .clone()
                .or_else(|| log_session_id.clone());
            if let (Some(timestamp), Some(session_id)) = (timestamp, session_id) {
                events.session_touches.push(SessionTouch {
                    timestamp,
                    session_id: session_id.clone(),
                });
                events.tool_events.push(KeyedToolEvent {
                    key: None,
                    event: ToolEvent {
                        timestamp: Some(timestamp),
                        session_id: Some(session_id),
                        tool_name: command_grant(&line)
                            .unwrap_or_else(|| "ToolConfirmation".to_owned()),
                        subagent_type: None,
                        source_kind: SourceKind::Main,
                    },
                });
            }
        }
    }

    events.compress_touches(local_offset);
    Some(events)
}

fn parse_log_timestamp(path: &Path, line: &str, local_offset: UtcOffset) -> Option<OffsetDateTime> {
    let mut parts = line.split_whitespace();
    let severity_date = parts.next()?;
    let time_part = parts.next()?;
    let year = log_year(path)?;
    if severity_date.len() != 5 {
        return None;
    }
    let month = severity_date.get(1..3)?.parse::<u8>().ok()?;
    let day = severity_date.get(3..5)?.parse::<u8>().ok()?;
    let mut time_parts = time_part.split([':', '.']);
    let hour = time_parts.next()?.parse::<u8>().ok()?;
    let minute = time_parts.next()?.parse::<u8>().ok()?;
    let second = time_parts.next()?.parse::<u8>().ok()?;
    let microsecond = time_parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let date = Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()?;
    let time = Time::from_hms_micro(hour, minute, second, microsecond).ok()?;
    // Antigravity log lines carry no timezone; interpret them in the local
    // offset, matching how the CLI writes them on the same machine. Cached
    // parses embed this interpretation (rebuild with --no-cache after moves).
    Some(PrimitiveDateTime::new(date, time).assume_offset(local_offset))
}

fn log_year(path: &Path) -> Option<i32> {
    let file_stem = path.file_stem()?.to_str()?;
    let raw_date = file_stem.strip_prefix("cli-")?.get(0..8)?;
    raw_date.get(0..4)?.parse::<i32>().ok()
}

fn value_after(line: &str, marker: &str) -> Option<String> {
    let start = line.find(marker)? + marker.len();
    let rest = line.get(start..)?;
    let end = rest
        .find(|character: char| character.is_whitespace() || character == ',')
        .unwrap_or(rest.len());
    let value = rest.get(0..end)?;
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn command_grant(line: &str) -> Option<String> {
    let command_start = line.find("command(")? + "command(".len();
    let rest = line.get(command_start..)?;
    let command = rest.get(0..rest.find(')')?)?.trim();
    let executable = command.split_whitespace().next()?;
    if executable.is_empty() {
        None
    } else {
        Some(format!("command:{executable}"))
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
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
    fn collects_antigravity_history_model_events_and_tool_confirmations() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let log_dir = temp.path().join("log");
        fs::create_dir_all(&log_dir).expect("test dirs should be created");
        fs::write(
            temp.path().join("history.jsonl"),
            r#"{"timestamp":1780726748000,"workspace":"/tmp","conversationId":"c1"}"#,
        )
        .expect("history fixture should be written");
        fs::write(
            log_dir.join("cli-20260609_112719.log"),
            concat!(
                r#"I0609 11:27:21.569672  8577 model_config_manager.go:157] Propagating selected model override to backend: label="Gemini 3.5 Flash (High)""#,
                "\n",
                r#"I0609 11:27:22.646388  8577 input_loop.go:34] HandleUserInput called with text: "redacted""#,
                "\n",
                r#"I0609 11:38:30.959991  8577 input_loop.go:451] Responding to tool confirmation: convID=c1, stepIdx=38, approved=true, sandboxOverride=false, turnGrants=[command(bun run)]"#,
                "\n"
            ),
        )
        .expect("log fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 2);
        // The text logs are activity/tools only — token usage now comes from
        // conversations/*.db (none in this fixture), so no usage events here.
        assert!(collection.usage_events.is_empty());
        assert_eq!(collection.tool_events[0].tool_name, "command:bun");
        assert!(collection.session_touches.len() >= 2);
    }
}
