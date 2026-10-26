use std::collections::{HashMap, HashSet};

use rusqlite::params;
use serde_json::Value;
use time::UtcOffset;

use crate::collector::ident::{ident, ident_string};
use crate::collector::json::parse_bounded;
use crate::collector::project_from_cwd;
use crate::collector::store::Store;
use crate::model::{Collection, DurationEvent, SessionTouch, SourceKind, ToolEvent, UsageEvent};

use super::{Generation, ms_to_offset, token_usage};

pub(super) fn parse_messages(
    conn: &Store,
    floor_ms: i64,
    local_offset: UtcOffset,
    collection: &mut Collection,
    seen: &mut HashSet<String>,
) {
    if !conn.has_table("message", &["id", "session_id", "time_created", "data"]) {
        return;
    }
    // Filter on the indexed `time_created` column so SQLite skips out-of-window
    // rows before we ever parse their JSON in Rust.
    let Ok(mut stmt) =
        conn.prepare("SELECT id, session_id, data FROM message WHERE time_created >= ?1")
    else {
        return;
    };
    let Ok(rows) = stmt.query_map(params![floor_ms], |row| {
        // `session_id` is NOT NULL in OpenCode's own schema, but a corrupt DB
        // could still carry a NULL — default it rather than dropping the row
        // (and its tokens) as a type-mismatch parse error.
        conn.charge(row, 3)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?
                .and_then(ident_string)
                .unwrap_or_default(),
            row.get::<_, String>(2)?,
        ))
    }) else {
        return;
    };

    for row in rows {
        collection.stats.lines_seen += 1;
        let Ok((id, session_id, data)) = row else {
            // A row-level SQLite error (type mismatch, mid-scan corruption)
            // should surface in the stats, not vanish like `flatten()` would.
            collection.stats.parse_errors += 1;
            if conn.spent() {
                break;
            }
            continue;
        };
        if seen.contains(&id) {
            continue;
        }
        // Claim the id only once the row parses, so a corrupt legacy row
        // doesn't hide its intact migrated copy.
        let Some(value) = parse_bounded(&data) else {
            collection.stats.parse_errors += 1;
            continue;
        };
        seen.insert(id);
        let Some(created_ms) = value.pointer("/time/created").and_then(Value::as_i64) else {
            continue;
        };
        let Some(timestamp) = ms_to_offset(created_ms, local_offset) else {
            continue;
        };

        collection.session_touches.push(SessionTouch {
            timestamp,
            session_id: session_id.clone(),
        });

        if value.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }

        let usage = token_usage(&value);
        collection.usage_events.push(UsageEvent {
            timestamp: Some(timestamp),
            session_id: Some(session_id.clone()),
            model: value
                .get("modelID")
                .and_then(Value::as_str)
                .and_then(ident)
                .map(ToOwned::to_owned),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            attribution_skill: None,
            project: value
                .pointer("/path/cwd")
                .and_then(Value::as_str)
                .and_then(project_from_cwd),
            // OpenCode records a per-message cost, but like every provider we
            // price from LiteLLM for consistency (see docs/opencode).
            usage,
            granularity: crate::model::UsageGranularity::Call,
        });

        if let Some(completed_ms) = value.pointer("/time/completed").and_then(Value::as_i64) {
            collection.duration_events.push(DurationEvent {
                timestamp: Some(timestamp),
                session_id: Some(session_id),
                // `time.completed` is untrusted; saturating_sub avoids an
                // overflow panic, and a clock that ran backwards (completed <
                // created) falls to 0 rather than a garbage duration.
                duration_ms: u64::try_from(completed_ms.saturating_sub(created_ms)).unwrap_or(0),
                human_wait_ms: 0,
                model_ms: None,
                status: value
                    .get("finish")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            });
        }
    }
}

pub(super) fn parse_tool_parts(
    conn: &Store,
    floor_ms: i64,
    local_offset: UtcOffset,
    collection: &mut Collection,
    seen: &mut HashSet<String>,
    tools_from: &mut HashMap<String, Generation>,
) {
    if !conn.has_table(
        "part",
        &["id", "message_id", "session_id", "time_created", "data"],
    ) {
        return;
    }
    // Filter only on the indexed `time_created` integer column in SQL, and do the
    // `type == "tool"` check in Rust. Putting `json_extract` in the WHERE clause
    // makes a single malformed `data` row raise "malformed JSON" and abort the
    // whole result set, losing every later tool call in this DB.
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, message_id, session_id, time_created, data FROM part WHERE time_created >= ?1",
    ) else {
        return;
    };
    let Ok(rows) = stmt.query_map(params![floor_ms], |row| {
        // `session_id` is NOT NULL in OpenCode's own schema, but a corrupt DB
        // could still carry a NULL — default it rather than dropping the row as
        // a type-mismatch parse error.
        conn.charge(row, 5)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?
                .and_then(ident_string)
                .unwrap_or_default(),
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
        ))
    }) else {
        return;
    };

    for row in rows {
        collection.stats.lines_seen += 1;
        let Ok((id, message_id, session_id, time_created, data)) = row else {
            collection.stats.parse_errors += 1;
            if conn.spent() {
                break;
            }
            continue;
        };
        let Some(value) = parse_bounded(&data) else {
            collection.stats.parse_errors += 1;
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(tool_name) = value.get("tool").and_then(Value::as_str).and_then(ident) else {
            continue;
        };
        if let Some(message_id) = message_id
            && *tools_from.entry(message_id).or_insert(Generation::Legacy) == Generation::V2
        {
            continue;
        }
        if !seen.insert(id) {
            continue;
        }
        // Prefer the tool's own start time; fall back to the row's `time_created`
        // so a tool without a recorded start isn't dropped by the analyzer for
        // lacking a timestamp.
        let start_ms = value
            .pointer("/state/time/start")
            .and_then(Value::as_i64)
            .unwrap_or(time_created);
        collection.tool_events.push(ToolEvent {
            timestamp: ms_to_offset(start_ms, local_offset),
            session_id: Some(session_id),
            tool_name: tool_name.to_owned(),
            subagent_type: None,
            source_kind: SourceKind::Main,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use crate::collector::ident::tests::{overlong_ident, overlong_path};
    use crate::collector::json::tests::over_structured_line;
    use crate::collector::opencode::collect;
    use crate::collector::opencode::fixtures::{ASSISTANT_DONE_MS, ASSISTANT_MS, write_db};
    use crate::model::Provider;

    fn insert_message(dir: &std::path::Path, id: &str, created_ms: i64, data: &str) {
        Connection::open(dir.join("opencode.db"))
            .expect("open temp db")
            .execute(
                "INSERT INTO message VALUES (?1, 's1', ?2, ?2, ?3)",
                params![id, created_ms, data],
            )
            .expect("insert message");
    }

    #[test]
    fn parses_tokens_tools_durations() {
        let dir = TempDir::new().expect("tempdir");
        write_db(dir.path());
        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.provider, Provider::OpenCode);
        assert_eq!(collection.usage_events.len(), 1);
        let event = &collection.usage_events[0];
        assert_eq!(event.usage.input_tokens, 100);
        assert_eq!(event.usage.output_tokens, 25);
        assert_eq!(event.usage.reasoning_output_tokens, 5);
        assert_eq!(event.usage.cache_read_input_tokens, 40);
        assert_eq!(event.usage.cache_creation_input_tokens, 10);
        assert_eq!(event.usage.token_volume(), 175);
        assert_eq!(event.model.as_deref(), Some("qwen3:8b"));
        assert_eq!(event.project.as_deref(), Some("somewhere/proj"));

        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.tool_events[0].tool_name, "glob");

        assert_eq!(collection.duration_events.len(), 1);
        let expected = u64::try_from(ASSISTANT_DONE_MS - ASSISTANT_MS).unwrap();
        assert_eq!(collection.duration_events[0].duration_ms, expected);

        assert_eq!(collection.session_touches.len(), 2);
    }

    #[test]
    fn malformed_part_json_does_not_drop_later_tools() {
        // A single malformed part row must not abort the whole result set; later
        // valid tool calls in the same DB still come through.
        let dir = TempDir::new().expect("tempdir");
        let conn = Connection::open(dir.path().join("opencode.db")).expect("open temp db");
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .expect("create schema");
        conn.execute(
            "INSERT INTO part VALUES ('p0','m1','s1',1781542400000,1781542400000,'{not valid json')",
            [],
        )
        .expect("insert bad");
        let good = r#"{"type":"tool","tool":"bash","state":{"time":{"start":1781542400500}}}"#;
        conn.execute(
            "INSERT INTO part VALUES ('p1','m1','s1',1781542400600,1781542400600,?1)",
            [good],
        )
        .expect("insert good");
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.tool_events[0].tool_name, "bash");
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn tool_without_start_time_falls_back_to_time_created() {
        let dir = TempDir::new().expect("tempdir");
        let conn = Connection::open(dir.path().join("opencode.db")).expect("open temp db");
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .expect("create schema");
        let tool = r#"{"type":"tool","tool":"read"}"#;
        conn.execute(
            "INSERT INTO part VALUES ('p1','m1','s1',1781542400000,1781542400000,?1)",
            [tool],
        )
        .expect("insert");
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);
        assert_eq!(collection.tool_events.len(), 1);
        assert!(collection.tool_events[0].timestamp.is_some());
    }

    #[test]
    fn null_session_id_keeps_the_row() {
        let dir = TempDir::new().expect("tempdir");
        let conn = Connection::open(dir.path().join("opencode.db")).expect("open temp db");
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .expect("create schema");
        let assistant = r#"{"role":"assistant","tokens":{"input":7,"output":3},"time":{"created":1781542363256}}"#;
        conn.execute(
            "INSERT INTO message VALUES ('m1',NULL,1781542363256,1781542363256,?1)",
            [assistant],
        )
        .expect("insert");
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 10);
        assert_eq!(collection.stats.parse_errors, 0);
    }

    #[test]
    fn backwards_clock_duration_is_zero() {
        let dir = TempDir::new().expect("tempdir");
        let conn = Connection::open(dir.path().join("opencode.db")).expect("open temp db");
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .expect("create schema");
        let backwards = r#"{"role":"assistant","tokens":{"input":1,"output":1},"time":{"created":2000,"completed":1000}}"#;
        conn.execute(
            "INSERT INTO message VALUES ('m1','s1',2000,2000,?1)",
            [backwards],
        )
        .expect("insert");
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 0);
    }

    #[test]
    fn message_dated_past_the_plausible_range_is_skipped() {
        let dir = TempDir::new().expect("tempdir");
        write_db(dir.path());
        insert_message(
            dir.path(),
            "m9",
            0,
            r#"{"role":"assistant","modelID":"x","tokens":{"input":1,"output":1},"time":{"created":253402290000000}}"#,
        );

        let tokyo = UtcOffset::from_hms(9, 0, 0).expect("offset");
        let collection = collect(dir.path(), None, false, tokyo);

        assert_eq!(collection.usage_events.len(), 1);
        assert!(collection.usage_events[0].timestamp.is_some());
    }

    #[test]
    fn over_structured_message_is_a_parse_error() {
        let dir = TempDir::new().expect("tempdir");
        write_db(dir.path());
        insert_message(dir.path(), "m9", ASSISTANT_MS, &over_structured_line());

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(collection.usage_events.len(), 1);
    }

    #[test]
    fn message_view_is_not_read() {
        let dir = TempDir::new().expect("tempdir");
        let conn = Connection::open(dir.path().join("opencode.db")).expect("open temp db");
        conn.execute_batch(
            "CREATE TABLE hidden (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE VIEW message AS SELECT * FROM hidden;",
        )
        .expect("create schema");
        drop(conn);
        Connection::open(dir.path().join("opencode.db"))
            .expect("open temp db")
            .execute(
                "INSERT INTO hidden VALUES ('m1', 's1', ?1, ?1, ?2)",
                params![
                    ASSISTANT_MS,
                    r#"{"role":"assistant","tokens":{"input":1,"output":1},"time":{"created":1781542363256}}"#
                ],
            )
            .expect("insert");

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert!(collection.usage_events.is_empty());
    }

    #[test]
    fn overlong_session_model_and_cwd_are_missing() {
        let dir = TempDir::new().expect("tempdir");
        write_db(dir.path());
        let long = overlong_ident();
        let deep = overlong_path();
        Connection::open(dir.path().join("opencode.db"))
            .expect("open temp db")
            .execute(
                "INSERT INTO message VALUES ('m9', ?1, ?2, ?2, ?3)",
                params![
                    long,
                    ASSISTANT_MS,
                    format!(
                        r#"{{"role":"assistant","modelID":"{long}","path":{{"cwd":"{deep}"}},"tokens":{{"input":1,"output":1}},"time":{{"created":{ASSISTANT_MS}}}}}"#
                    )
                ],
            )
            .expect("insert message");

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        let event = collection
            .usage_events
            .iter()
            .find(|e| e.usage.token_volume() == 2)
            .expect("the row still counts");
        assert_eq!(event.session_id.as_deref(), Some(""));
        assert_eq!(event.model, None);
        assert_eq!(event.project, None);
    }

    /// The table-type check once matched SQL text, which a line break
    /// inside `CREATE VIRTUAL TABLE` defeated; FTS5 then read the view
    /// named as its external content. SQLite writes that prefix itself, so
    /// only a crafted file carries the line break.
    #[test]
    fn virtual_message_table_is_not_read() {
        let dir = TempDir::new().expect("tempdir");
        let conn = Connection::open(dir.path().join("opencode.db")).expect("open temp db");
        conn.execute_batch(
            "CREATE TABLE hidden (id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE VIEW shown AS SELECT rowid, * FROM hidden;
             CREATE VIRTUAL TABLE message USING fts5(id, session_id, time_created, data, content='shown');
             PRAGMA writable_schema = ON;
             UPDATE sqlite_schema SET sql = replace(sql, 'VIRTUAL TABLE', 'VIRTUAL' || char(10) || 'TABLE')
                 WHERE name = 'message';
             PRAGMA writable_schema = OFF;",
        )
        .expect("create schema");
        conn.execute(
            "INSERT INTO hidden VALUES ('m1', 's1', ?1, ?2)",
            params![
                ASSISTANT_MS,
                r#"{"role":"assistant","tokens":{"input":1,"output":1},"time":{"created":1781542363256}}"#
            ],
        )
        .expect("insert");
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert!(collection.usage_events.is_empty());
    }
}
