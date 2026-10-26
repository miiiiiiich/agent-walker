//! Opening agents' SQLite stores. A store is untrusted input like a log
//! line: a queried table replaced by a view of an endless `WITH RECURSIVE`
//! grew to 3.9 GB and never returned.
//!
//! Stores are opened read-only, so SQLite never writes, checkpoints or
//! recovers their pages. Reading a WAL-mode store still needs its `-wal` and
//! `-shm` files: SQLite creates them when they're missing and, unable to
//! checkpoint, leaves them behind. `immutable=1` would avoid that, but it
//! reads torn pages while the agent is writing.
//!
//! A crafted store planted in an agent's directory is outside what these
//! guards defend: whoever can write there already runs code as the user. The
//! checks below are cheap defense in depth against such files (the link check
//! is unix-only and races a concurrent writer), not a sandbox; nothing copies
//! a store to read it safely.

use std::cell::Cell;
use std::fs::Metadata;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};

/// SQLite time one provider's stores may take together, shared across all of
/// them so a directory of crafted stores can't multiply it.
const READ_BUDGET: Duration = Duration::from_secs(30);

/// Largest string, blob or row SQLite will produce: a crafted generated
/// column can otherwise build a gigabyte value per row. Matches the JSONL
/// line cap; real rows are far smaller.
const MAX_VALUE_BYTES: i32 = 64 << 20;

/// Text and blob bytes one store's queries may return, so values a store
/// computes rather than stores (a generated column, an added column's
/// default repeated on every old row) stop somewhere: 128 rows of an 8 KiB
/// store returned 2 GiB. An absolute cap far above any real store, not one
/// relative to the file: a joined column repeated on every row legitimately
/// returns more than the file holds.
const MAX_RETURNED_BYTES: u64 = 1 << 30;

/// VM instructions between deadline checks.
const BUDGET_CHECK_OPS: i32 = 10_000;

pub(in crate::collector) fn read_deadline() -> Instant {
    Instant::now() + READ_BUDGET
}

/// A read-only connection plus what its queries may still return.
pub(in crate::collector) struct Store {
    conn: Connection,
    return_budget: Cell<u64>,
}

impl Deref for Store {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}

/// Opens `path` read-only. SQLite resolves symlinks and works on the files
/// beside the target, so those are checked right before the open: the store
/// and any `-wal`, `-shm` or `-journal` must be regular files, and on unix
/// have one link. Opening a FIFO blocks forever, and SQLite opens `-shm`
/// read-write and truncates it even on a read-only connection, so a `-shm`
/// hard-linked to another file would destroy that file. The schema is not
/// trusted (`trusted_schema = OFF`), nothing can be written (`query_only`),
/// and every query is interrupted once `deadline` passes.
pub(in crate::collector) fn open_store(path: &Path, deadline: Instant) -> Option<Store> {
    open_with_return_cap(path, deadline, MAX_RETURNED_BYTES)
}

fn open_with_return_cap(path: &Path, deadline: Instant, max_returned: u64) -> Option<Store> {
    if Instant::now() >= deadline {
        return None;
    }
    let path = std::fs::canonicalize(path).ok()?;
    if !sole_regular_file(&std::fs::symlink_metadata(&path).ok()?) {
        return None;
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        match std::fs::symlink_metadata(sidecar(&path, suffix)) {
            Ok(meta) if sole_regular_file(&meta) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return None,
        }
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    // Rides out the agent's brief write locks instead of failing at once.
    let _ = conn.busy_timeout(Duration::from_millis(500));
    conn.pragma_update(None, "trusted_schema", false).ok()?;
    conn.pragma_update(None, "query_only", true).ok()?;
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_VALUE_BYTES)
        .ok()?;
    conn.progress_handler(BUDGET_CHECK_OPS, Some(move || Instant::now() >= deadline))
        .ok()?;
    Some(Store {
        conn,
        return_budget: Cell::new(max_returned),
    })
}

#[cfg(unix)]
fn sole_regular_file(meta: &Metadata) -> bool {
    meta.is_file() && std::os::unix::fs::MetadataExt::nlink(meta) == 1
}

/// Windows exposes no stable link count, so only the file type is checked.
#[cfg(not(unix))]
fn sole_regular_file(meta: &Metadata) -> bool {
    meta.is_file()
}

impl Store {
    /// Whether `table` is an ordinary table (by SQLite's own classification,
    /// not its SQL text) and each of `columns` one of its stored columns.
    /// Every table a collector queries is checked first: a view, virtual
    /// table or generated column of the same name would run SQL the store
    /// chose. The PRAGMA statements are used rather than their table-valued
    /// `pragma_*` functions, which a store's own table of that name shadows.
    pub(in crate::collector) fn has_table(&self, table: &str, columns: &[&str]) -> bool {
        let mut kinds = Vec::new();
        let listed = self.conn.pragma(Some("main"), "table_list", table, |row| {
            kinds.push(row.get::<_, String>("type")?);
            Ok(())
        });
        if listed.is_err() || kinds != ["table"] {
            return false;
        }
        let mut stored = Vec::new();
        let described = self.conn.pragma(Some("main"), "table_xinfo", table, |row| {
            if row.get::<_, i64>("hidden")? == 0 {
                stored.push(row.get::<_, String>("name")?);
            }
            Ok(())
        });
        described.is_ok()
            && columns
                .iter()
                .all(|column| stored.iter().any(|name| name.eq_ignore_ascii_case(column)))
    }

    /// Charges the text and blob bytes of `row`'s first `columns` values,
    /// before the caller copies them out; an error once the store's budget is
    /// spent, after which the caller stops reading (`spent`).
    pub(in crate::collector) fn charge(&self, row: &Row, columns: usize) -> rusqlite::Result<()> {
        let mut bytes = 0u64;
        for index in 0..columns {
            if let ValueRef::Text(value) | ValueRef::Blob(value) = row.get_ref(index)? {
                bytes = bytes.saturating_add(value.len() as u64);
            }
        }
        let remaining = self.return_budget.get();
        if bytes > remaining {
            self.return_budget.set(0);
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_TOOBIG),
                Some("store returned more than its read budget".to_owned()),
            ));
        }
        self.return_budget.set(remaining - bytes);
        Ok(())
    }

    pub(in crate::collector) fn spent(&self) -> bool {
        self.return_budget.get() == 0
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::collector::read::tests::finishes_quickly;

    fn write_store(path: &Path, schema: &str) {
        Connection::open(path)
            .expect("fixture store should open")
            .execute_batch(schema)
            .expect("fixture schema should apply");
    }

    #[test]
    fn only_ordinary_tables_with_stored_columns_pass() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("store.db");
        write_store(
            &path,
            "CREATE TABLE real (idx INTEGER PRIMARY KEY, data BLOB);
             CREATE VIEW looped AS WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) SELECT n FROM r;
             CREATE VIRTUAL TABLE search USING fts5(body);
             CREATE VIRTUAL TABLE mirrored USING fts5(data, content='looped');
             CREATE TABLE computed (idx INTEGER PRIMARY KEY, data BLOB GENERATED ALWAYS AS (zeroblob(16)) VIRTUAL);
             CREATE TABLE kept (idx INTEGER PRIMARY KEY, data BLOB GENERATED ALWAYS AS (zeroblob(16)) STORED);
             PRAGMA writable_schema = ON;
             UPDATE sqlite_schema SET sql = replace(sql, 'VIRTUAL TABLE', 'VIRTUAL' || char(10) || 'TABLE')
                 WHERE name = 'search';
             PRAGMA writable_schema = OFF;",
        );
        let store = open_store(&path, read_deadline()).expect("store should open");

        assert!(store.has_table("real", &["idx", "data"]));
        assert!(store.has_table("REAL", &["IDX"]));
        assert!(!store.has_table("real", &["idx", "missing"]));
        for name in ["looped", "search", "mirrored", "search_data", "missing"] {
            assert!(!store.has_table(name, &[]), "{name}");
        }
        assert!(store.has_table("computed", &["idx"]));
        assert!(!store.has_table("computed", &["idx", "data"]));
        assert!(!store.has_table("kept", &["data"]));
    }

    /// A store's own table named after a `pragma_*` function shadows it, so
    /// the classification is read through PRAGMA statements.
    #[test]
    fn a_table_named_like_a_pragma_function_cannot_vouch_for_a_view() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("store.db");
        write_store(
            &path,
            "CREATE VIEW looped AS WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) SELECT n AS x FROM r;
             CREATE VIRTUAL TABLE pragma_table_list USING fts5(schema, name, type);
             INSERT INTO pragma_table_list VALUES ('main', 'looped', 'table');
             CREATE VIRTUAL TABLE pragma_table_xinfo USING fts5(name, hidden);
             INSERT INTO pragma_table_xinfo VALUES ('x', '0');",
        );
        let store = open_store(&path, read_deadline()).expect("store should open");

        assert!(!store.has_table("looped", &["x"]));
    }

    /// An added column's default is returned for every older row without
    /// being stored in it, so a small store can return without bound.
    #[test]
    fn returned_bytes_stop_at_the_store_budget() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("store.db");
        let pad = "00".repeat(8 << 10);
        write_store(
            &path,
            &format!(
                "CREATE TABLE t (x INTEGER);
                 WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 1000)
                 INSERT INTO t SELECT n FROM r;
                 ALTER TABLE t ADD COLUMN pad BLOB DEFAULT x'{pad}';"
            ),
        );
        assert!(std::fs::metadata(&path).expect("store exists").len() < 64 << 10);
        let cap = 1 << 20;
        let store = open_with_return_cap(&path, read_deadline(), cap).expect("store should open");

        let mut stmt = store.prepare("SELECT x, pad FROM t").expect("prepares");
        let mut rows = stmt
            .query_map([], |row| {
                store.charge(row, 2)?;
                row.get::<_, Vec<u8>>(1)
            })
            .expect("queries");
        let accepted = rows.by_ref().take_while(Result::is_ok).count();

        assert!(store.spent());
        assert!(accepted * (8 << 10) <= usize::try_from(cap).expect("fits"));
        assert!(accepted > 0);
    }

    #[test]
    fn a_runaway_query_stops_at_the_deadline() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("store.db");
        write_store(&path, "CREATE TABLE real (x);");
        let store = open_store(&path, Instant::now() + Duration::from_millis(200))
            .expect("store should open");

        let endless = finishes_quickly(move || {
            store
                .query_row(
                    "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) SELECT count(*) FROM r",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .is_ok()
        });

        assert!(!endless);
    }

    #[test]
    fn the_connection_cannot_write_or_build_huge_values() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("store.db");
        write_store(&path, "CREATE TABLE real (x);");
        let store = open_store(&path, read_deadline()).expect("store should open");

        let trusted: bool = store
            .pragma_query_value(None, "trusted_schema", |row| row.get(0))
            .expect("pragma reads");
        assert!(!trusted);
        assert!(store.execute("INSERT INTO real VALUES (1)", []).is_err());
        assert!(
            store
                .query_row("SELECT length(zeroblob(100000000))", [], |row| row
                    .get::<_, i64>(0))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn special_files_and_journals_are_not_opened() {
        use crate::collector::read::tests::make_fifo;

        let temp = TempDir::new().expect("test tempdir should be created");
        let fifo = temp.path().join("fifo.db");
        make_fifo(&fifo);
        let with_fifo_wal = temp.path().join("store.db");
        write_store(&with_fifo_wal, "CREATE TABLE real (x);");
        make_fifo(&sidecar(&with_fifo_wal, "-wal"));

        let opened = finishes_quickly(move || {
            [fifo, with_fifo_wal].map(|path| open_store(&path, read_deadline()).is_some())
        });

        assert_eq!(opened, [false, false]);
    }

    /// SQLite truncates `-shm` on a read-only open of a WAL store, so a
    /// `-shm` hard-linked to another file would destroy it.
    #[cfg(unix)]
    #[test]
    fn hard_linked_stores_and_sidecars_are_not_opened() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let victim = temp.path().join("victim.txt");
        std::fs::write(&victim, "keep me").expect("victim should be written");

        let store_dir = temp.path().join("linked-shm");
        std::fs::create_dir(&store_dir).expect("dir should be created");
        let wal_store = store_dir.join("store.db");
        write_store(
            &wal_store,
            "PRAGMA journal_mode = WAL; CREATE TABLE real (x);",
        );
        std::fs::hard_link(&victim, sidecar(&wal_store, "-shm")).expect("link should be created");
        // Reached through a symlink, the sidecars beside the target count.
        let link = temp.path().join("link.db");
        std::os::unix::fs::symlink(&wal_store, &link).expect("symlink should be created");

        let linked_store = temp.path().join("linked.db");
        write_store(&linked_store, "CREATE TABLE real (x);");
        std::fs::hard_link(&linked_store, temp.path().join("other.db"))
            .expect("link should be created");

        for path in [&wal_store, &link, &linked_store] {
            assert!(
                open_store(path, read_deadline()).is_none(),
                "{}",
                path.display()
            );
        }
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim reads"),
            "keep me"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_a_store_is_followed() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let target = temp.path().join("store.db");
        write_store(&target, "CREATE TABLE real (x);");
        let link = temp.path().join("link.db");
        std::os::unix::fs::symlink(&target, &link).expect("symlink should be created");

        let store = open_store(&link, read_deadline()).expect("linked store should open");

        assert!(store.has_table("real", &["x"]));
    }
}
