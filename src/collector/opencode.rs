//! OpenCode collector — reads OpenCode's local SQLite store.
//!
//! Unlike the JSONL collectors (Claude / Codex / Antigravity), OpenCode keeps
//! everything in `<root>/opencode.db` (SQLite). Per-assistant-message token
//! usage lives in the `message` table's `data` JSON
//! (`tokens.{input,output,reasoning,cache.{read,write}}`, `modelID`,
//! `path.cwd`, `time.{created,completed}`); tool calls live in `part`
//! (`type:"tool"`, `tool`). Storage layout is documented at
//! <https://opencode.ai/docs/troubleshooting/> and the OSS repo (sst/opencode).
//!
//! We read a **private snapshot copy** of the DB (copied to a temp dir, WAL and
//! all) and never let SQLite open the user's live store, so we can't lock,
//! checkpoint, or corrupt it. Cost is left to the shared LiteLLM pricing path
//! like every other provider — the per-message `cost` OpenCode records (and
//! local models such as Ollama, which report no priced usage) is not used here.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;
use time::{OffsetDateTime, UtcOffset};

use crate::collector::project_from_cwd;
use crate::model::{
    Collection, DurationEvent, Provider, SessionTouch, SourceKind, TokenUsage, ToolEvent,
    UsageEvent,
};

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    _use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::OpenCode, root.to_path_buf());
    let db_path = root.join("opencode.db");
    if !db_path.exists() {
        return collection;
    }
    // `_snapshot` must outlive `conn` — it owns the temp copy `conn` reads.
    let Some((conn, _snapshot)) = open_snapshot(&db_path) else {
        collection.stats.unreadable_files += 1;
        return collection;
    };
    collection.stats.files_seen += 1;

    // Events older than the history window can't be relevant; the timestamps are
    // epoch milliseconds (`time.created`), so compare in the same unit.
    let floor_ms = mtime_floor
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0);

    parse_messages(&conn, floor_ms, local_offset, &mut collection);
    parse_tool_parts(&conn, floor_ms, local_offset, &mut collection);

    collection.stats.usage_events = collection.usage_events.len();
    collection.stats.tool_events = collection.tool_events.len();
    collection.stats.duration_events = collection.duration_events.len();
    collection
}

/// Copy the DB (plus any `-wal` / `-shm` sidecars) into a throwaway temp dir and
/// open the *copy*. The user's store is only ever read via `fs::copy` — never
/// opened by SQLite — so a live OpenCode session can't be locked, checkpointed,
/// or corrupted, and copying the WAL keeps the snapshot consistent (no
/// `immutable=1` foot-gun). The `TempDir` is returned so it outlives the
/// connection and is removed on drop.
fn open_snapshot(db: &Path) -> Option<(Connection, TempDir)> {
    let snapshot = TempDir::new().ok()?;
    let dest = snapshot.path().join("opencode.db");
    std::fs::copy(db, &dest).ok()?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = with_suffix(db, suffix);
        if sidecar.exists() {
            let _ = std::fs::copy(&sidecar, with_suffix(&dest, suffix));
        }
    }
    Connection::open(&dest).ok().map(|conn| (conn, snapshot))
}

/// SQLite sidecar path: the DB filename with a suffix appended (`opencode.db`
/// → `opencode.db-wal`). Appends to the whole path so non-UTF-8 names survive.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// One usage + duration event per assistant message (the per-turn totals live on
/// the message), plus a session touch per message for concurrency / active days.
fn parse_messages(
    conn: &Connection,
    floor_ms: i64,
    local_offset: UtcOffset,
    collection: &mut Collection,
) {
    // Filter on the indexed `time_created` column so SQLite skips out-of-window
    // rows before we ever parse their JSON in Rust.
    let Ok(mut stmt) =
        conn.prepare("SELECT session_id, data FROM message WHERE time_created >= ?1")
    else {
        return;
    };
    let Ok(rows) = stmt.query_map(params![floor_ms], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return;
    };

    for row in rows {
        collection.stats.lines_seen += 1;
        let Ok((session_id, data)) = row else {
            // A row-level SQLite error (type mismatch, mid-scan corruption)
            // should surface in the stats, not vanish like `flatten()` would.
            collection.stats.parse_errors += 1;
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            collection.stats.parse_errors += 1;
            continue;
        };
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

        let usage = TokenUsage {
            input_tokens: token(&value, "/tokens/input"),
            output_tokens: token(&value, "/tokens/output"),
            reasoning_output_tokens: token(&value, "/tokens/reasoning"),
            cache_creation_input_tokens: token(&value, "/tokens/cache/write"),
            cache_read_input_tokens: token(&value, "/tokens/cache/read"),
            ..TokenUsage::default()
        };
        collection.usage_events.push(UsageEvent {
            timestamp: Some(timestamp),
            session_id: Some(session_id.clone()),
            model: value
                .get("modelID")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            project: value
                .pointer("/path/cwd")
                .and_then(Value::as_str)
                .map(project_from_cwd),
            usage,
        });

        if let Some(completed_ms) = value.pointer("/time/completed").and_then(Value::as_i64) {
            collection.duration_events.push(DurationEvent {
                timestamp: Some(timestamp),
                session_id: Some(session_id),
                // `time.completed` is untrusted; saturating_sub avoids an
                // overflow panic, and a clock that ran backwards (completed <
                // created) falls to 0 rather than a garbage duration.
                duration_ms: u64::try_from(completed_ms.saturating_sub(created_ms)).unwrap_or(0),
                status: value
                    .get("finish")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            });
        }
    }
}

/// Tool calls are `part` rows of `type:"tool"`; the real tool name is the `tool`
/// field (`glob` / `read` / `edit` / `bash` / an MCP name).
fn parse_tool_parts(
    conn: &Connection,
    floor_ms: i64,
    local_offset: UtcOffset,
    collection: &mut Collection,
) {
    // `time_created >= ?1` first lets SQLite drop out-of-window rows on the cheap
    // integer column before the per-row `json_extract` ever runs.
    let Ok(mut stmt) = conn.prepare(
        "SELECT session_id, data FROM part WHERE time_created >= ?1 AND json_extract(data, '$.type') = 'tool'",
    ) else {
        return;
    };
    let Ok(rows) = stmt.query_map(params![floor_ms], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return;
    };

    for row in rows {
        collection.stats.lines_seen += 1;
        let Ok((session_id, data)) = row else {
            collection.stats.parse_errors += 1;
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            collection.stats.parse_errors += 1;
            continue;
        };
        let Some(tool_name) = value.get("tool").and_then(Value::as_str) else {
            continue;
        };
        let start_ms = value.pointer("/state/time/start").and_then(Value::as_i64);
        collection.tool_events.push(ToolEvent {
            timestamp: start_ms.and_then(|ms| ms_to_offset(ms, local_offset)),
            session_id: Some(session_id),
            tool_name: tool_name.to_owned(),
            subagent_type: None,
            source_kind: SourceKind::Main,
        });
    }
}

fn token(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

/// Epoch-milliseconds → local-offset `OffsetDateTime`.
fn ms_to_offset(ms: i64, local_offset: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .map(|time| time.to_offset(local_offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::time::Duration;
    use tempfile::TempDir;

    const ASSISTANT_MS: i64 = 1_781_542_363_256;
    const ASSISTANT_DONE_MS: i64 = 1_781_542_436_778;

    fn write_db(dir: &Path) {
        let conn = Connection::open(dir.join("opencode.db")).expect("open temp db");
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .expect("create schema");
        let assistant = r#"{"role":"assistant","modelID":"qwen3:8b","path":{"cwd":"/somewhere/proj"},"tokens":{"input":100,"output":20,"reasoning":5,"cache":{"read":40,"write":10}},"time":{"created":1781542363256,"completed":1781542436778},"finish":"stop"}"#;
        conn.execute(
            "INSERT INTO message VALUES ('m1','s1',1781542363256,1781542436778,?1)",
            [assistant],
        )
        .expect("insert assistant");
        let user = r#"{"role":"user","time":{"created":1781542300000}}"#;
        conn.execute(
            "INSERT INTO message VALUES ('m0','s1',1781542300000,1781542300000,?1)",
            [user],
        )
        .expect("insert user");
        let tool = r#"{"type":"tool","tool":"glob","state":{"time":{"start":1781542400000,"end":1781542400500}}}"#;
        conn.execute(
            "INSERT INTO part VALUES ('p1','m1','s1',1781542400000,1781542400500,?1)",
            [tool],
        )
        .expect("insert tool part");
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
        assert_eq!(event.usage.output_tokens, 20);
        assert_eq!(event.usage.reasoning_output_tokens, 5);
        assert_eq!(event.usage.cache_read_input_tokens, 40);
        assert_eq!(event.usage.cache_creation_input_tokens, 10);
        // token_volume = input + output + cache_create + cache_read (reasoning is
        // a subset of output, not added on top).
        assert_eq!(event.usage.token_volume(), 170);
        assert_eq!(event.model.as_deref(), Some("qwen3:8b"));
        assert_eq!(event.project.as_deref(), Some("somewhere/proj"));

        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.tool_events[0].tool_name, "glob");

        assert_eq!(collection.duration_events.len(), 1);
        let expected = u64::try_from(ASSISTANT_DONE_MS - ASSISTANT_MS).unwrap();
        assert_eq!(collection.duration_events[0].duration_ms, expected);

        // A touch per message (user + assistant), for concurrency / active days.
        assert_eq!(collection.session_touches.len(), 2);
    }

    #[test]
    fn missing_db_is_empty() {
        let dir = TempDir::new().expect("tempdir");
        let collection = collect(dir.path(), None, false, UtcOffset::UTC);
        assert!(collection.usage_events.is_empty());
        assert!(collection.tool_events.is_empty());
        assert!(collection.session_touches.is_empty());
    }

    #[test]
    fn mtime_floor_drops_events_before_the_window() {
        let dir = TempDir::new().expect("tempdir");
        write_db(dir.path());
        // Floor sits after every event in the fixture.
        let floor = std::time::UNIX_EPOCH + Duration::from_millis(1_781_542_500_000);
        let collection = collect(dir.path(), Some(floor), false, UtcOffset::UTC);
        assert!(collection.usage_events.is_empty());
        assert!(collection.tool_events.is_empty());
        assert!(collection.session_touches.is_empty());
    }

    #[test]
    fn backwards_clock_duration_is_zero() {
        // A corrupt / skewed `time.completed` earlier than `time.created` must
        // not overflow or produce a garbage duration — it falls to 0.
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
}
