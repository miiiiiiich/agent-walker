use std::io::BufReader;
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::collector::ident::ident;
use crate::collector::json::parse_line;
use crate::collector::read::{Lines, open_regular};
use crate::collector::{FileEvents, KeyedToolEvent, list_files, merge_into, parse_files_cached};
use crate::model::{Collection, Provider, SessionTouch, SourceKind, ToolEvent};
use crate::timestamp::{from_unix_seconds, plausible};

mod conv;

const CLI_STORE: &str = "antigravity-cli";
const APP_STORE: &str = "antigravity";

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

    let cli = root.join(CLI_STORE);
    let mut files = Vec::new();
    let history = cli.join("history.jsonl");
    if history.exists() {
        files.push(history);
    }
    let log_dir = cli.join("log");
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

    let usage = conv::collect_usage(
        &[cli, root.join(APP_STORE)],
        mtime_floor,
        local_offset,
        &mut collection.stats,
    );
    // App conversations, and CLI conversations with no history or log lines,
    // have nothing but their generations: without a touch per generation their
    // sessions count in the total but never reach daily sessions, spans or
    // concurrency. Every generation touches, not only untouched conversations:
    // the DB name is the history's conversationId, so the analyzer merges both
    // into one session.
    collection
        .session_touches
        .extend(usage.iter().filter_map(|event| {
            Some(SessionTouch {
                timestamp: event.timestamp?,
                session_id: event.session_id.clone()?,
            })
        }));
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
    let mut lines = Lines::new(BufReader::new(open_regular(path).ok()?));
    let mut events = FileEvents::default();

    while let Some((line_index, line)) = lines.next(&mut events) {
        let Some(value) = parse_line(line, &mut events) else {
            continue;
        };
        // An unusable timestamp cannot be placed on the timeline; count it as a
        // parse error rather than dropping it silently.
        let Some(timestamp) = value
            .get("timestamp")
            .and_then(Value::as_i64)
            .and_then(|timestamp_ms| from_unix_seconds(timestamp_ms / 1_000))
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
    let mut lines = Lines::new(BufReader::new(open_regular(path).ok()?));
    let mut events = FileEvents::default();
    let log_session_id = fallback_session_id(path);
    let mut current_conversation_id = None;

    while let Some((_, line)) = lines.next(&mut events) {
        let Ok(line) = std::str::from_utf8(line) else {
            events.parse_errors += 1;
            continue;
        };
        let timestamp = parse_log_timestamp(path, line, local_offset);

        if line.contains("HandleUserInput called with text") {
            let session_id = current_conversation_id
                .clone()
                .or_else(|| log_session_id.clone());
            if let (Some(timestamp), Some(session_id)) = (timestamp, session_id) {
                events.session_touches.push(SessionTouch {
                    timestamp,
                    session_id,
                });
            }
            continue;
        }

        if line.contains("Responding to tool confirmation") {
            current_conversation_id = value_after(line, "convID=").or(current_conversation_id);
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
                        tool_name: command_grant(line)
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
    // Log lines carry no timezone; the CLI wrote them in this machine's local
    // time, so assume the local offset, not UTC.
    plausible(PrimitiveDateTime::new(date, time).assume_offset(local_offset))
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
        ident(value).map(ToOwned::to_owned)
    }
}

fn command_grant(line: &str) -> Option<String> {
    let command_start = line.find("command(")? + "command(".len();
    let rest = line.get(command_start..)?;
    let command = rest.get(0..rest.find(')')?)?.trim();
    let executable = ident(command.split_whitespace().next()?)?;
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
        .and_then(ident)
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
    use time::macros::{date, datetime};

    use super::conv::tests::{served_only_gen_blob, step_metadata, write_dated_conversation};
    use super::*;
    use crate::collector::ident::tests::overlong_ident;
    use crate::collector::read::tests::oversized_json;

    const INPUT_LOG_LINE: &str = r#"I0609 11:27:22.646388  8577 input_loop.go:34] HandleUserInput called with text: "redacted""#;

    #[test]
    fn collects_antigravity_history_model_events_and_tool_confirmations() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        let log_dir = cli.join("log");
        fs::create_dir_all(&log_dir).expect("test dirs should be created");
        let app_conversations = temp.path().join(APP_STORE).join("conversations");
        fs::create_dir_all(&app_conversations).expect("app store should be created");
        rusqlite::Connection::open(app_conversations.join("c2.db"))
            .and_then(|conn| {
                conn.execute_batch("CREATE TABLE gen_metadata (idx INTEGER, data BLOB);")
            })
            .expect("app conversation should be created");
        fs::write(
            cli.join("history.jsonl"),
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

        // history + log + the app store's conversation
        assert_eq!(collection.stats.files_seen, 3);
        assert!(collection.usage_events.is_empty());
        assert_eq!(collection.tool_events[0].tool_name, "command:bun");
        assert!(collection.session_touches.len() >= 2);
    }

    #[test]
    fn conversation_generations_count_as_sessions_once_per_day() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        let app = temp.path().join(APP_STORE);
        for store in [&cli, &app] {
            fs::create_dir_all(store.join("conversations"))
                .expect("conversations dir should be created");
        }
        let started_ms = 1_780_966_800_000; // 2026-06-09 01:00 UTC
        let hour_ms = 3_600_000;
        let row = |rid: &str| served_only_gen_blob("gemini-3.8-flash", rid, None);
        // c1 is in the CLI history and has a generation an hour later.
        fs::write(
            cli.join("history.jsonl"),
            format!(r#"{{"timestamp":{started_ms},"conversationId":"c1"}}"#),
        )
        .expect("history fixture should be written");
        write_dated_conversation(
            &cli.join("conversations").join("c1.db"),
            started_ms,
            &[row("r1")],
            &[step_metadata(started_ms + hour_ms, "r1")],
        );
        // c2 exists only in the app store: no history, no logs.
        write_dated_conversation(
            &app.join("conversations").join("c2.db"),
            started_ms,
            &[row("r2"), row("r3")],
            &[
                step_metadata(started_ms + 2 * hour_ms, "r2"),
                step_metadata(started_ms + 4 * hour_ms, "r3"),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);
        let summary = crate::analyzer::summarize(
            &collection,
            datetime!(2026-06-09 23:00 UTC),
            7,
            UtcOffset::UTC,
        );

        assert_eq!(collection.usage_events.len(), 3);
        assert_eq!(summary.sessions, 2);
        let sessions_per_day: Vec<_> = summary
            .daily_sessions
            .iter()
            .map(|day| (day.date, day.sessions))
            .collect();
        assert_eq!(sessions_per_day, [(date!(2026 - 06 - 09), 2)]);
        assert_eq!(
            summary
                .longest_session
                .expect("the app conversation should have a span")
                .duration_secs(),
            7_200
        );
    }

    #[test]
    fn history_and_log_rows_past_the_plausible_range_are_undated() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        let log_dir = cli.join("log");
        fs::create_dir_all(&log_dir).expect("test dirs should be created");
        fs::write(
            cli.join("history.jsonl"),
            concat!(
                r#"{"timestamp":1780726748000,"conversationId":"c1"}"#,
                "\n",
                r#"{"timestamp":253402290000000,"conversationId":"c2"}"#,
            ),
        )
        .expect("history fixture should be written");
        fs::write(log_dir.join("cli-99991231_235900.log"), INPUT_LOG_LINE)
            .expect("log fixture should be written");

        let tokyo = UtcOffset::from_hms(9, 0, 0).expect("offset");
        let collection = collect(temp.path(), None, false, tokyo);

        assert_eq!(collection.session_touches.len(), 1);
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn history_and_log_lines_past_the_cap_are_parse_errors() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        let log_dir = cli.join("log");
        fs::create_dir_all(&log_dir).expect("test dirs should be created");
        let history = r#"{"timestamp":1780726748000,"conversationId":"c1"}"#;
        fs::write(
            cli.join("history.jsonl"),
            format!("{}\n{history}\n", oversized_json(history)),
        )
        .expect("history fixture should be written");
        let padded_log_line = format!(
            "{INPUT_LOG_LINE}{}",
            " ".repeat(usize::try_from(crate::collector::read::MAX_LINE_BYTES).expect("fits"))
        );
        fs::write(
            log_dir.join("cli-20260609_112719.log"),
            format!("{padded_log_line}\n{INPUT_LOG_LINE}\n"),
        )
        .expect("log fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 2);
        assert_eq!(collection.session_touches.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn history_that_is_a_fifo_is_not_opened() {
        use crate::collector::read::tests::{finishes_quickly, make_fifo};

        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        fs::create_dir_all(&cli).expect("test dirs should be created");
        make_fifo(&cli.join("history.jsonl"));

        let root = temp.path().to_path_buf();
        let collection = finishes_quickly(move || collect(&root, None, false, UtcOffset::UTC));

        assert_eq!(collection.stats.unreadable_files, 1);
    }

    #[test]
    fn generation_view_is_not_read() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let conversations = temp.path().join(CLI_STORE).join("conversations");
        fs::create_dir_all(&conversations).expect("test dirs should be created");
        let conn = rusqlite::Connection::open(conversations.join("c1.db"))
            .expect("fixture db should open");
        conn.execute_batch(
            "CREATE TABLE hidden (idx INTEGER PRIMARY KEY, data BLOB);
             CREATE VIEW gen_metadata AS SELECT idx, data FROM hidden;",
        )
        .expect("fixture schema should be created");
        conn.execute(
            "INSERT INTO hidden VALUES (0, ?1)",
            [served_only_gen_blob("gemini-3.8-flash", "r1", None)],
        )
        .expect("generation row should insert");
        drop(conn);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert!(collection.usage_events.is_empty());
    }

    #[test]
    fn overlong_conversation_id_falls_back_to_the_log() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let log_dir = temp.path().join(CLI_STORE).join("log");
        fs::create_dir_all(&log_dir).expect("test dirs should be created");
        let long = overlong_ident();
        fs::write(
            log_dir.join("cli-20260609_112719.log"),
            format!(
                "I0609 11:38:30.959991  8577 input_loop.go:451] Responding to tool confirmation: convID={long}, stepIdx=38, approved=true, turnGrants=[command(bun run)]\n"
            ),
        )
        .expect("log fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(
            collection.tool_events[0].session_id.as_deref(),
            Some("cli-20260609_112719")
        );
    }

    #[test]
    fn generated_generation_column_is_not_read() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let conversations = temp.path().join(CLI_STORE).join("conversations");
        fs::create_dir_all(&conversations).expect("test dirs should be created");
        let conn = rusqlite::Connection::open(conversations.join("c1.db"))
            .expect("fixture db should open");
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY,
                 data BLOB GENERATED ALWAYS AS (zeroblob(65536)) VIRTUAL);
             WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 128)
             INSERT INTO gen_metadata (idx) SELECT n FROM r;",
        )
        .expect("fixture schema should be created");
        drop(conn);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.stats.lines_seen, 0);
    }
}
