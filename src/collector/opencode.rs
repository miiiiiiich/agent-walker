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
//! We open the live DB **read-only** and read it directly. The read-only flag
//! means SQLite can never write the user's store (no checkpoint, no recovery
//! write); in WAL mode our reads don't block OpenCode's writes, and a brief
//! `busy_timeout` rides out the rare exclusive moments. Reading directly — rather
//! than copying the whole DB into memory first — keeps memory proportional to
//! the recent-window rows we actually parse, not the entire history. Cost is
//! left to the shared LiteLLM pricing path like every other provider — the
//! per-message `cost` OpenCode records (and local models such as Ollama, which
//! report no priced usage) is not used here.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;
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

    // Events older than the history window can't be relevant; the timestamps are
    // epoch milliseconds (`time.created`), so compare in the same unit.
    let floor_ms = mtime_floor
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0);

    // Across multiple DB files (e.g. a default store plus a channel store, or a
    // copy left over from a migration) the same row can appear twice; dedupe by
    // its primary-key id so a turn isn't counted once per file.
    let mut seen_messages = HashSet::new();
    let mut seen_parts = HashSet::new();
    for db_path in db_paths(root) {
        let Some(conn) = open_readonly(&db_path) else {
            collection.stats.unreadable_files += 1;
            continue;
        };
        collection.stats.files_seen += 1;
        parse_messages(
            &conn,
            floor_ms,
            local_offset,
            &mut collection,
            &mut seen_messages,
        );
        parse_tool_parts(
            &conn,
            floor_ms,
            local_offset,
            &mut collection,
            &mut seen_parts,
        );
    }

    collection.stats.usage_events = collection.usage_events.len();
    collection.stats.tool_events = collection.tool_events.len();
    collection.stats.duration_events = collection.duration_events.len();
    collection
}

/// The OpenCode DB file(s) to read. Reads the `OPENCODE_DB` override from the
/// environment, then defers to [`resolve_db_paths`] (kept env-free so it stays
/// testable, like `paths::resolve_root`).
fn db_paths(root: &Path) -> Vec<PathBuf> {
    resolve_db_paths(root, std::env::var_os("OPENCODE_DB"))
}

/// Mirror OpenCode's own resolver: `OPENCODE_DB` wins (an absolute path used
/// as-is, a relative one joined under the data dir; `:memory:` can't be read
/// from another process, so it's skipped). Otherwise read every `opencode*.db`
/// in the data dir, which covers both the default `opencode.db` and the
/// per-channel `opencode-<channel>.db` a non-stable install writes.
fn resolve_db_paths(root: &Path, override_db: Option<OsString>) -> Vec<PathBuf> {
    if let Some(db) = override_db {
        if db == *OsStr::new(":memory:") {
            return Vec::new();
        }
        let path = PathBuf::from(&db);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        // A missing override is "absent", not "unreadable" — skip it silently.
        return if path.exists() {
            vec![path]
        } else {
            Vec::new()
        };
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let is_db = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("db"));
            let named_opencode = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("opencode"));
            // A directory matching the pattern isn't a DB — don't try to open it.
            is_db && named_opencode && path.is_file()
        })
        .collect();
    // Deterministic order so files_seen / parse order doesn't depend on the FS.
    paths.sort();
    paths
}

/// Open the live DB read-only. The read-only flag guarantees SQLite never writes
/// the user's store; the `busy_timeout` rides out a brief write lock (e.g. a
/// checkpoint restart) instead of failing immediately. A WAL DB that needs
/// recovery with no `-shm` present can't be opened read-only; that DB is skipped
/// rather than touched.
fn open_readonly(db: &Path) -> Option<Connection> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(500));
    Some(conn)
}

/// One usage + duration event per assistant message (the per-turn totals live on
/// the message), plus a session touch per message for concurrency / active days.
fn parse_messages(
    conn: &Connection,
    floor_ms: i64,
    local_offset: UtcOffset,
    collection: &mut Collection,
    seen: &mut HashSet<String>,
) {
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
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
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
            continue;
        };
        // Already counted from another DB file (a copied store) — skip the dup.
        if !seen.insert(id) {
            continue;
        }
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
            // OpenCode stores only the *visible* output here and counts
            // reasoning separately; fold reasoning in so token totals and cost
            // match the inclusive convention the other providers use.
            output_tokens: token(&value, "/tokens/output")
                .saturating_add(token(&value, "/tokens/reasoning")),
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
    seen: &mut HashSet<String>,
) {
    // Filter only on the indexed `time_created` integer column in SQL, and do the
    // `type == "tool"` check in Rust. Putting `json_extract` in the WHERE clause
    // makes a single malformed `data` row raise "malformed JSON" and abort the
    // whole result set, losing every later tool call in this DB.
    let Ok(mut stmt) = conn
        .prepare("SELECT id, session_id, time_created, data FROM part WHERE time_created >= ?1")
    else {
        return;
    };
    let Ok(rows) = stmt.query_map(params![floor_ms], |row| {
        // `session_id` is NOT NULL in OpenCode's own schema, but a corrupt DB
        // could still carry a NULL — default it rather than dropping the row as
        // a type-mismatch parse error.
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
        ))
    }) else {
        return;
    };

    for row in rows {
        collection.stats.lines_seen += 1;
        let Ok((id, session_id, time_created, data)) = row else {
            collection.stats.parse_errors += 1;
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            collection.stats.parse_errors += 1;
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(tool_name) = value.get("tool").and_then(Value::as_str) else {
            continue;
        };
        // Already counted from another DB file (a copied store) — skip the dup.
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
        write_db_at(&dir.join("opencode.db"));
    }

    fn write_db_at(db: &Path) {
        let conn = Connection::open(db).expect("open temp db");
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
        // OpenCode's stored output (20) excludes reasoning (5); we fold reasoning
        // in, so output_tokens is the inclusive 25 and reasoning is also kept.
        assert_eq!(event.usage.output_tokens, 25);
        assert_eq!(event.usage.reasoning_output_tokens, 5);
        assert_eq!(event.usage.cache_read_input_tokens, 40);
        assert_eq!(event.usage.cache_creation_input_tokens, 10);
        // token_volume = input + output(incl. reasoning) + cache_create +
        // cache_read = 100 + 25 + 10 + 40.
        assert_eq!(event.usage.token_volume(), 175);
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
    fn channel_db_is_collected() {
        // A non-stable install writes `opencode-<channel>.db` instead of the
        // default name; the glob must still pick it up.
        let dir = TempDir::new().expect("tempdir");
        write_db_at(&dir.path().join("opencode-dev.db"));
        let collection = collect(dir.path(), None, false, UtcOffset::UTC);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.stats.files_seen, 1);
    }

    #[test]
    fn resolve_db_paths_globs_channel_dbs_sorted() {
        let dir = TempDir::new().expect("tempdir");
        write_db_at(&dir.path().join("opencode.db"));
        write_db_at(&dir.path().join("opencode-beta.db"));
        // A `-wal` sidecar must not be mistaken for a DB file.
        std::fs::write(dir.path().join("opencode.db-wal"), b"").expect("wal");
        let paths = resolve_db_paths(dir.path(), None);
        assert_eq!(
            paths,
            vec![
                dir.path().join("opencode-beta.db"),
                dir.path().join("opencode.db"),
            ]
        );
    }

    #[test]
    fn resolve_db_paths_honors_absolute_override() {
        let dir = TempDir::new().expect("tempdir");
        let custom = dir.path().join("custom.db");
        write_db_at(&custom);
        // Even with a default db present, the absolute override wins exclusively.
        write_db_at(&dir.path().join("opencode.db"));
        let paths = resolve_db_paths(dir.path(), Some(custom.clone().into_os_string()));
        assert_eq!(paths, vec![custom]);
    }

    #[test]
    fn resolve_db_paths_memory_override_is_skipped() {
        let dir = TempDir::new().expect("tempdir");
        write_db_at(&dir.path().join("opencode.db"));
        let paths = resolve_db_paths(dir.path(), Some(OsString::from(":memory:")));
        assert!(paths.is_empty());
    }

    #[test]
    fn resolve_db_paths_missing_override_is_absent() {
        let dir = TempDir::new().expect("tempdir");
        let paths = resolve_db_paths(dir.path(), Some(OsString::from("/no/such/file.db")));
        assert!(paths.is_empty());
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
    fn duplicate_rows_across_db_files_are_counted_once() {
        // A copied store left beside the default (same row ids) must not double
        // count the same turn.
        let dir = TempDir::new().expect("tempdir");
        write_db_at(&dir.path().join("opencode.db"));
        write_db_at(&dir.path().join("opencode-prod.db"));
        let collection = collect(dir.path(), None, false, UtcOffset::UTC);
        assert_eq!(collection.stats.files_seen, 2);
        // One assistant message and one tool part, despite two identical DBs.
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.tool_events.len(), 1);
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
        // A tool part with no state.time.start.
        let tool = r#"{"type":"tool","tool":"read"}"#;
        conn.execute(
            "INSERT INTO part VALUES ('p1','m1','s1',1781542400000,1781542400000,?1)",
            [tool],
        )
        .expect("insert");
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);
        assert_eq!(collection.tool_events.len(), 1);
        // The event keeps a timestamp (from time_created) so the analyzer doesn't
        // drop it.
        assert!(collection.tool_events[0].timestamp.is_some());
    }

    #[test]
    fn null_session_id_keeps_the_row() {
        // A corrupt DB with a NULL session_id must not drop the row (and its
        // tokens) as a parse error — it defaults to an empty session instead.
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
