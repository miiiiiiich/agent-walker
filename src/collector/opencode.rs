//! We open the live DB **read-only** and read it directly. The read-only flag
//! means SQLite can never write the user's store (no checkpoint, no recovery
//! write); in WAL mode our reads don't block OpenCode's writes, and a brief
//! `busy_timeout` rides out the rare exclusive moments. Reading directly — rather
//! than copying the whole DB into memory first — keeps memory proportional to
//! the recent-window rows we actually parse, not the entire history.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use crate::model::{Collection, Provider, TokenUsage};

mod legacy;
mod v2;

#[cfg(test)]
mod fixtures;

use legacy::{parse_messages, parse_tool_parts};
use v2::parse_session_messages;

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    _use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::OpenCode, root.to_path_buf());

    let floor_ms = mtime_floor
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0);

    // Across multiple DB files (e.g. a default store plus a channel store, or a
    // copy left over from a migration) the same row can appear twice; dedupe by
    // its primary-key id so a turn isn't counted once per file.
    let mut seen_messages = HashSet::new();
    let mut seen_parts = HashSet::new();
    // Legacy `part` rows and a migrated copy's `content` are the same tool
    // calls under different keys, so part-id dedupe alone counted them twice
    // whenever the copy was read first (an earlier-sorted DB file, or a
    // corrupt legacy message). A message's tools come from one generation.
    let mut tools_from = HashMap::new();
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
            &mut tools_from,
        );
        // After the legacy tables, so a row already counted there (same id)
        // is skipped together with its tools.
        parse_session_messages(
            &conn,
            floor_ms,
            local_offset,
            &mut collection,
            &mut seen_messages,
            &mut tools_from,
        );
    }

    collection.stats.usage_events = collection.usage_events.len();
    collection.stats.tool_events = collection.tool_events.len();
    collection.stats.duration_events = collection.duration_events.len();
    collection
}

/// Keep path resolution env-free so it stays testable.
fn db_paths(root: &Path) -> Vec<PathBuf> {
    resolve_db_paths(root, std::env::var_os("OPENCODE_DB"))
}

/// Skip `:memory:` because another process cannot read it.
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Generation {
    Legacy,
    V2,
}

/// Reasoning is folded into output, the inclusive convention the other
/// providers use.
fn token_usage(value: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: token(value, "/tokens/input"),
        output_tokens: token(value, "/tokens/output")
            .saturating_add(token(value, "/tokens/reasoning")),
        reasoning_output_tokens: token(value, "/tokens/reasoning"),
        cache_creation_input_tokens: token(value, "/tokens/cache/write"),
        cache_read_input_tokens: token(value, "/tokens/cache/read"),
        ..TokenUsage::default()
    }
}

fn token(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

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

    use super::fixtures::{
        ASSISTANT_MS, V2_SCHEMA, insert_v2, v2_assistant, volumes, write_db, write_db_at,
    };

    /// The OpenCode 2 copy of `write_db_at`'s rows: same ids, same tool.
    fn write_migrated_copy(db: &Path) -> Connection {
        let conn = Connection::open(db).expect("open temp db");
        conn.execute_batch(V2_SCHEMA).expect("create schema");
        conn.execute(
            "INSERT INTO session_v2 VALUES ('s1', '/somewhere/proj', NULL, NULL, 1781542000000)",
            [],
        )
        .expect("insert session");
        insert_v2(
            &conn,
            "m0",
            "s1",
            "user",
            0,
            1_781_542_300_000,
            r#"{"text":"hi","time":{"created":1781542300000}}"#,
        );
        insert_v2(
            &conn,
            "m1",
            "s1",
            "assistant",
            1,
            ASSISTANT_MS,
            &v2_assistant(ASSISTANT_MS, 100, Some("glob")),
        );
        conn
    }

    fn tool_names(collection: &Collection) -> Vec<&str> {
        let mut tools: Vec<&str> = collection
            .tool_events
            .iter()
            .map(|tool| tool.tool_name.as_str())
            .collect();
        tools.sort_unstable();
        tools
    }

    /// A `session_message` row with a legacy message's id counts once.
    #[test]
    fn migrated_rows_are_not_counted_twice() {
        let dir = TempDir::new().expect("tempdir");
        write_db(dir.path());
        let conn = write_migrated_copy(&dir.path().join("opencode.db"));
        insert_v2(
            &conn,
            "msg_new",
            "s1",
            "assistant",
            2,
            1_781_543_000_000,
            &v2_assistant(1_781_543_000_000, 700, Some("bash")),
        );
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(volumes(&collection), vec![175, 700 + 12 + 30 + 4]);
        assert_eq!(tool_names(&collection), vec!["bash", "glob"]);
        assert_eq!(collection.duration_events.len(), 2);
        assert_eq!(collection.session_touches.len(), 3);
    }

    /// `opencode-dev.db` sorts before `opencode.db`, so the migrated copy is
    /// read before the legacy rows it shares ids with.
    #[test]
    fn migrated_copy_in_an_earlier_db_file_counts_tools_once() {
        let dir = TempDir::new().expect("tempdir");
        drop(write_migrated_copy(&dir.path().join("opencode-dev.db")));
        write_db(dir.path());

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 2);
        assert_eq!(volumes(&collection), vec![100 + 12 + 30 + 4]);
        assert_eq!(tool_names(&collection), vec!["glob"]);
        assert_eq!(collection.duration_events.len(), 1);
    }

    /// An unreadable legacy message falls back to its migrated copy for
    /// tokens, while its legacy tool parts are still readable.
    #[test]
    fn unreadable_legacy_message_counts_tools_once() {
        let dir = TempDir::new().expect("tempdir");
        write_db(dir.path());
        let conn = write_migrated_copy(&dir.path().join("opencode.db"));
        conn.execute("UPDATE message SET data = '{not json' WHERE id = 'm1'", [])
            .expect("corrupt legacy message");
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(volumes(&collection), vec![100 + 12 + 30 + 4]);
        assert_eq!(tool_names(&collection), vec!["glob"]);
    }

    #[test]
    fn channel_db_is_collected() {
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
        #[allow(clippy::duration_suboptimal_units)] // millis is a unix timestamp, not a span
        let floor = std::time::UNIX_EPOCH + Duration::from_millis(1_781_542_500_000);
        let collection = collect(dir.path(), Some(floor), false, UtcOffset::UTC);
        assert!(collection.usage_events.is_empty());
        assert!(collection.tool_events.is_empty());
        assert!(collection.session_touches.is_empty());
    }

    #[test]
    fn duplicate_rows_across_db_files_are_counted_once() {
        let dir = TempDir::new().expect("tempdir");
        write_db_at(&dir.path().join("opencode.db"));
        write_db_at(&dir.path().join("opencode-prod.db"));
        let collection = collect(dir.path(), None, false, UtcOffset::UTC);
        assert_eq!(collection.stats.files_seen, 2);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.session_touches.len(), 2);
    }
}
