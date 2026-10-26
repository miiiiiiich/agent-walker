use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

use rusqlite::types::Type;

use crate::collector::ident::ident_string;
use crate::collector::store::{open_store, read_deadline};
use crate::collector::{FileEvents, KeyedUsageEvent, project_from_cwd};
use crate::model::{Collection, SessionTouch, SourceKind, UsageEvent};
use crate::timestamp::{parse_rfc3339, plausible};

use super::{MAX_SANE_TOKENS, RawCounters};

pub(super) struct CallLedger {
    pub(super) events: FileEvents,
    pub(super) first_call: HashMap<String, OffsetDateTime>,
    /// Sessions with at least one row that couldn't be read.
    incomplete: HashSet<String>,
    /// A row that couldn't even be tied to a session.
    unattributed: bool,
}

impl CallLedger {
    /// Superseding a session's snapshots from its readable rows alone would
    /// lose an unreadable row's usage that a snapshot covers, so such a
    /// session keeps its snapshots. Of its readable rows only those after
    /// its latest snapshot stay: the snapshots cover everything up to it,
    /// and dropping the later rows would lose every call since that exit.
    /// A session without snapshots keeps all its readable rows.
    pub(super) fn fall_back_where_incomplete(
        &mut self,
        per_file: &[(PathBuf, Option<FileEvents>)],
    ) {
        if self.incomplete.is_empty() && !self.unattributed {
            return;
        }
        let mut covered_until: HashMap<&String, OffsetDateTime> = HashMap::new();
        for keyed in per_file
            .iter()
            .filter_map(|(_, events)| events.as_ref())
            .flat_map(|events| &events.usage_events)
        {
            if let (Some(sid), Some(time)) =
                (keyed.event.session_id.as_ref(), keyed.event.timestamp)
            {
                covered_until
                    .entry(sid)
                    .and_modify(|until| *until = (*until).max(time))
                    .or_insert(time);
            }
        }
        let falls_back_until = |sid: &String| {
            if self.unattributed || self.incomplete.contains(sid) {
                covered_until.get(sid).copied()
            } else {
                None
            }
        };
        self.events.usage_events.retain(|keyed| {
            let (Some(sid), Some(time)) = (keyed.event.session_id.as_ref(), keyed.event.timestamp)
            else {
                return true;
            };
            falls_back_until(sid).is_none_or(|until| time > until)
        });
        self.first_call
            .retain(|sid, _| falls_back_until(sid).is_none());
    }
}

const CALL_COLUMNS: [&str; 11] = [
    "id",
    "session_id",
    "model",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "agent_id",
    "initiator",
    "created_at",
];

const CALLS_SQL: &str = "SELECT e.id, e.session_id, e.model, e.input_tokens, e.output_tokens, \
     e.cache_read_tokens, e.cache_write_tokens, e.reasoning_tokens, e.agent_id, e.initiator, \
     e.created_at, s.cwd \
     FROM assistant_usage_events e LEFT JOIN sessions s ON s.id = e.session_id";

struct CallRow {
    id: i64,
    session_id: String,
    model: String,
    counters: RawCounters,
    subagent: bool,
    created_at: Option<String>,
    cwd: Option<String>,
}

/// Without both tables there is no ledger, as with a CLI that predates it.
pub(super) fn read_call_ledger(
    path: &Path,
    local_offset: UtcOffset,
    collection: &mut Collection,
) -> Option<CallLedger> {
    if !path.is_file() {
        return None;
    }
    let Some(store) = open_store(path, read_deadline()) else {
        collection.stats.unreadable_files += 1;
        return None;
    };
    if !store.has_table("assistant_usage_events", &CALL_COLUMNS)
        || !store.has_table("sessions", &["id", "cwd"])
    {
        return None;
    }
    let mut stmt = store.prepare(CALLS_SQL).ok()?;
    // The session id is read on its own so a row that fails elsewhere still
    // names the session it leaves incomplete.
    let rows = stmt
        .query_map([], |row| {
            // Only the call's own columns: the joined `cwd` repeats the one
            // session row on every call, and charging it per call cut a
            // 10,000-call ledger short.
            store.charge(row, CALL_COLUMNS.len())?;
            Ok((ident_column(row, 1).ok(), call_row(row)))
        })
        .ok()?;

    let mut ledger = CallLedger {
        events: FileEvents::default(),
        first_call: HashMap::new(),
        incomplete: HashSet::new(),
        unattributed: false,
    };
    for row in rows {
        ledger.events.lines_seen += 1;
        let Ok((session_id, call)) = row else {
            ledger.events.parse_errors += 1;
            ledger.unattributed = true;
            if store.spent() {
                break;
            }
            continue;
        };
        let Some((row, timestamp)) = call
            .ok()
            .and_then(|row| parse_db_time(row.created_at.as_deref()?).map(|time| (row, time)))
        else {
            ledger.events.parse_errors += 1;
            match session_id {
                Some(sid) => {
                    ledger.incomplete.insert(sid);
                }
                None => ledger.unattributed = true,
            }
            continue;
        };
        ledger
            .first_call
            .entry(row.session_id.clone())
            .and_modify(|first| *first = (*first).min(timestamp))
            .or_insert(timestamp);
        ledger.events.session_touches.push(SessionTouch {
            timestamp,
            session_id: row.session_id.clone(),
        });
        let usage = row.counters.to_usage();
        if usage.token_volume() == 0 {
            continue;
        }
        ledger.events.usage_events.push(KeyedUsageEvent {
            key: Some(format!("copilot-call:{}", row.id)),
            event: UsageEvent {
                timestamp: Some(timestamp),
                session_id: Some(row.session_id),
                model: Some(row.model),
                source_kind: if row.subagent {
                    SourceKind::Subagent
                } else {
                    SourceKind::Main
                },
                attribution_agent: None,
                attribution_skill: None,
                project: row.cwd.as_deref().and_then(project_from_cwd),
                usage,
                granularity: crate::model::UsageGranularity::Call,
            },
        });
    }
    ledger.events.compress_touches(local_offset);
    Some(ledger)
}

fn ident_column(row: &rusqlite::Row, index: usize) -> rusqlite::Result<String> {
    ident_string(row.get(index)?).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(index, "identifier".to_owned(), Type::Text)
    })
}

fn call_row(row: &rusqlite::Row) -> rusqlite::Result<CallRow> {
    Ok(CallRow {
        id: row.get(0)?,
        session_id: ident_column(row, 1)?,
        model: ident_column(row, 2)?,
        counters: RawCounters {
            input: db_count(row.get(3)?),
            output: db_count(row.get(4)?),
            cache_read: db_count(row.get(5)?),
            cache_write: db_count(row.get(6)?),
            reasoning: db_count(row.get(7)?),
        },
        subagent: row.get::<_, Option<String>>(8)?.is_some()
            || row.get::<_, Option<String>>(9)?.as_deref() == Some("sub-agent"),
        created_at: row.get(10)?,
        cwd: row.get(11)?,
    })
}

/// From a session's first recorded call on, its rows replace its snapshots;
/// earlier snapshots still count. A fork's copy of a superseded snapshot
/// (same key) is dropped too, or the parent's calls would count again under
/// the fork.
pub(super) fn supersede_snapshots(
    per_file: &mut [(PathBuf, Option<FileEvents>)],
    first_call: &HashMap<String, OffsetDateTime>,
) {
    let mut superseded: HashSet<String> = HashSet::new();
    for events in per_file
        .iter_mut()
        .filter_map(|(_, events)| events.as_mut())
    {
        events.usage_events.retain(|keyed| {
            let Some(first) = keyed
                .event
                .session_id
                .as_ref()
                .and_then(|sid| first_call.get(sid))
            else {
                return true;
            };
            let before_store = keyed.event.timestamp.is_some_and(|time| time < *first);
            if !before_store && let Some(key) = &keyed.key {
                superseded.insert(key.clone());
            }
            before_store
        });
    }
    if superseded.is_empty() {
        return;
    }
    for events in per_file
        .iter_mut()
        .filter_map(|(_, events)| events.as_mut())
    {
        events.usage_events.retain(|keyed| {
            keyed
                .key
                .as_ref()
                .is_none_or(|key| !superseded.contains(key))
        });
    }
}

fn parse_db_time(raw: &str) -> Option<OffsetDateTime> {
    parse_rfc3339(raw).or_else(|| {
        PrimitiveDateTime::parse(
            raw,
            format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        )
        .ok()
        .map(PrimitiveDateTime::assume_utc)
        .and_then(plausible)
    })
}

fn db_count(value: Option<i64>) -> u64 {
    value
        .and_then(|count| u64::try_from(count).ok())
        .unwrap_or(0)
        .min(MAX_SANE_TOKENS)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;
    use crate::collector::copilot::collect;
    use crate::collector::copilot::tests::{shutdown, total_volume, write_session};

    struct Call<'a> {
        session: &'a str,
        model: &'a str,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
        agent_id: Option<&'a str>,
        created_at: Option<&'a str>,
    }

    fn call<'a>(session: &'a str, created_at: &'a str, input: i64, output: i64) -> Call<'a> {
        Call {
            session,
            model: "gpt-5-mini",
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            agent_id: None,
            created_at: Some(created_at),
        }
    }

    fn write_store(root: &Path, calls: &[Call]) {
        fs::create_dir_all(root).expect("test root should be created");
        let conn = Connection::open(root.join("session-store.db")).expect("store should open");
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT);
             CREATE TABLE assistant_usage_events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, turn_index INTEGER,
                 agent_id TEXT, model TEXT NOT NULL, input_tokens INTEGER, output_tokens INTEGER,
                 cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER,
                 total_nano_aiu INTEGER, initiator TEXT, created_at TEXT DEFAULT (datetime('now')));",
        )
        .expect("store schema should be created");
        for call in calls {
            conn.execute(
                "INSERT OR IGNORE INTO sessions (id, cwd) VALUES (?1, '/Users/me/code/app')",
                [call.session],
            )
            .expect("session row should insert");
            conn.execute(
                "INSERT INTO assistant_usage_events (session_id, agent_id, model, input_tokens, \
                 output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)",
                rusqlite::params![
                    call.session,
                    call.agent_id,
                    call.model,
                    call.input,
                    call.output,
                    call.cache_read,
                    call.cache_write,
                    call.created_at,
                ],
            )
            .expect("usage row should insert");
        }
    }

    #[test]
    fn per_call_rows_replace_the_shutdown_snapshot() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n",
                shutdown("e-1", "2026-07-28T00:00:00.000Z", 1_500, 150)
            ),
        );
        write_store(
            temp.path(),
            &[
                call("s1", "2026-07-26T10:00:00.000Z", 1_000, 100),
                call("s1", "2026-07-27T10:00:00.000Z", 500, 50),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(total_volume(&collection), 1_650);
        let days: Vec<u8> = collection
            .usage_events
            .iter()
            .filter_map(|event| event.timestamp.map(OffsetDateTime::day))
            .collect();
        assert_eq!(days, vec![26, 27]);
        assert_eq!(
            collection.usage_events[0].project.as_deref(),
            Some("Users/me/code/app")
        );
    }

    /// Calls of a session without a shutdown still count.
    #[test]
    fn session_without_shutdown_counts_from_the_store() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            concat!(
                r#"{"type":"session.usage_checkpoint","id":"c-1","timestamp":"2026-07-23T14:59:54.634Z","data":{"totalNanoAiu":1958555000}}"#,
                "\n"
            ),
        );
        write_store(
            temp.path(),
            &[call("s1", "2026-07-23T14:58:42.988Z", 31_027, 286)],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 31_027 + 286);
        assert_eq!(collection.credit_samples.len(), 1);
    }

    /// A snapshot dated before the session's first recorded call still
    /// counts; everything after comes from the rows.
    #[test]
    fn snapshots_before_the_first_call_still_count() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n{}\n",
                shutdown("e-1", "2026-07-20T00:00:00.000Z", 1_000, 100),
                shutdown("e-2", "2026-07-28T00:00:00.000Z", 1_500, 150),
            ),
        );
        write_store(
            temp.path(),
            &[call("s1", "2026-07-27T10:00:00.000Z", 500, 50)],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(total_volume(&collection), 1_100 + 550);
    }

    /// A fork's copy of a snapshot the parent's rows supersede must not count
    /// either.
    #[test]
    fn fork_copy_of_a_superseded_snapshot_is_dropped() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let parent_exit = shutdown("e-1", "2026-07-28T00:00:00.000Z", 1_500, 150);
        write_session(temp.path(), "parent", &format!("{parent_exit}\n"));
        write_session(temp.path(), "fork", &format!("{parent_exit}\n"));
        write_store(
            temp.path(),
            &[
                call("parent", "2026-07-26T10:00:00.000Z", 1_000, 100),
                call("parent", "2026-07-27T10:00:00.000Z", 500, 50),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 1_650);
    }

    #[test]
    fn store_alone_is_collected_with_subagent_calls_marked() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let mut subagent = call("s1", "2026-07-27T10:00:01.000Z", 200, 20);
        subagent.agent_id = Some("agent-1");
        subagent.cache_read = 50;
        write_store(
            temp.path(),
            &[call("s1", "2026-07-27T10:00:00.000Z", 100, 10), subagent],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(collection.usage_events[1].source_kind, SourceKind::Subagent);
        assert_eq!(collection.usage_events[1].usage.input_tokens, 150);
        assert_eq!(collection.stats.files_seen, 1);
        assert!(!collection.session_touches.is_empty());
    }

    #[test]
    fn unplaceable_row_falls_back_to_the_snapshot() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n",
                shutdown("e-1", "2026-07-28T00:00:00.000Z", 1_000, 100)
            ),
        );
        write_store(temp.path(), &[call("s1", "not a time", 1_000, 100)]);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(total_volume(&collection), 1_100);
    }

    fn session_volumes(collection: &Collection, session: &str) -> Vec<u64> {
        collection
            .usage_events
            .iter()
            .filter(|event| event.session_id.as_deref() == Some(session))
            .map(|event| event.usage.token_volume())
            .collect()
    }

    fn insert_raw_row(root: &Path, values: &str) {
        let conn = Connection::open(root.join("session-store.db")).expect("store should open");
        conn.execute(
            &format!(
                "INSERT INTO assistant_usage_events (session_id, model, input_tokens, \
                 output_tokens, created_at) VALUES ({values})"
            ),
            [],
        )
        .expect("raw row should insert");
    }

    /// An unreadable row sends its session back to its snapshot for the
    /// span the snapshot covers: counting the readable rows there and
    /// superseding the snapshot would lose the unreadable row's usage. Calls
    /// after the latest snapshot still count from their rows, and a session
    /// without a snapshot keeps its readable rows.
    #[test]
    fn unreadable_row_falls_back_to_the_snapshot_but_keeps_later_calls() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!("{}\n", shutdown("e-1", "2026-07-28T00:00:00.000Z", 270, 30)),
        );
        write_store(
            temp.path(),
            &[
                call("s1", "2026-07-27T10:00:00.000Z", 90, 10),
                call("s1", "2026-07-28T05:00:00.000Z", 40, 10),
                call("s2", "2026-07-27T10:00:00.000Z", 40, 10),
                call("s2", "not a time", 180, 20),
            ],
        );
        insert_raw_row(
            temp.path(),
            "'s1', 'gpt-5-mini', 'not a number', 20, '2026-07-27T11:00:00.000Z'",
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 2);
        assert_eq!(session_volumes(&collection, "s1"), vec![300, 50]);
        assert_eq!(session_volumes(&collection, "s2"), vec![50]);
    }

    /// A row tied to no session sends every session with a snapshot back to
    /// it, with the same cutoff at its latest snapshot.
    #[test]
    fn unattributed_row_keeps_calls_after_the_latest_snapshot() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!("{}\n", shutdown("e-1", "2026-07-28T00:00:00.000Z", 270, 30)),
        );
        write_store(
            temp.path(),
            &[
                call("s1", "2026-07-27T10:00:00.000Z", 90, 10),
                call("s1", "2026-07-28T05:00:00.000Z", 40, 10),
                call("s2", "2026-07-27T10:00:00.000Z", 40, 10),
            ],
        );
        insert_raw_row(
            temp.path(),
            "X'00', 'gpt-5-mini', 10, 1, '2026-07-27T11:00:00.000Z'",
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(session_volumes(&collection, "s1"), vec![300, 50]);
        assert_eq!(session_volumes(&collection, "s2"), vec![50]);
    }

    #[test]
    fn sqlite_default_stamp_is_read_as_utc() {
        let parsed = parse_db_time("2026-07-27 12:00:00").expect("default stamp should parse");
        assert_eq!(parsed.offset(), UtcOffset::UTC);
        assert_eq!(parsed.hour(), 12);
    }

    #[test]
    fn store_without_usage_table_keeps_snapshots() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n",
                shutdown("e-1", "2026-07-28T00:00:00.000Z", 1_000, 100)
            ),
        );
        let conn =
            Connection::open(temp.path().join("session-store.db")).expect("store should open");
        conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT);")
            .expect("schema should be created");
        drop(conn);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 1_100);
        assert_eq!(collection.stats.unreadable_files, 0);
    }

    /// A view of the same name would run whatever SQL the store chose; the
    /// one that recursed forever is cut down here to a single row.
    #[test]
    fn usage_view_is_not_read_as_the_ledger() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::create_dir_all(temp.path()).expect("test root should be created");
        let conn =
            Connection::open(temp.path().join("session-store.db")).expect("store should open");
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT);
             CREATE VIEW assistant_usage_events AS SELECT 1 AS id, 's1' AS session_id,
                 'gpt-5' AS model, 1000 AS input_tokens, 100 AS output_tokens,
                 0 AS cache_read_tokens, 0 AS cache_write_tokens, 0 AS reasoning_tokens,
                 NULL AS agent_id, 'user' AS initiator, '2026-07-27T10:00:00Z' AS created_at;",
        )
        .expect("schema should be created");
        drop(conn);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert!(collection.usage_events.is_empty());
    }

    #[test]
    fn call_dated_past_the_plausible_range_is_a_parse_error() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_store(
            temp.path(),
            &[
                call("s1", "2026-07-27T10:00:00.000Z", 1_000, 100),
                call("s1", "9999-12-31 23:00:00", 500, 50),
            ],
        );

        let tokyo = UtcOffset::from_hms(9, 0, 0).expect("offset");
        let collection = collect(temp.path(), None, false, tokyo);

        assert_eq!(total_volume(&collection), 1_100);
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn call_with_an_overlong_model_is_a_parse_error() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let long = crate::collector::ident::tests::overlong_ident();
        write_store(
            temp.path(),
            &[
                call("s1", "2026-07-27T10:00:00.000Z", 1_000, 100),
                Call {
                    model: &long,
                    ..call("s1", "2026-07-27T11:00:00.000Z", 500, 50)
                },
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 1_100);
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn a_long_ledger_with_a_long_cwd_is_read_in_full() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::create_dir_all(temp.path()).expect("test root should be created");
        let conn =
            Connection::open(temp.path().join("session-store.db")).expect("store should open");
        conn.execute_batch(&format!(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT);
             CREATE TABLE assistant_usage_events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, turn_index INTEGER,
                 agent_id TEXT, model TEXT NOT NULL, input_tokens INTEGER, output_tokens INTEGER,
                 cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER,
                 total_nano_aiu INTEGER, initiator TEXT, created_at TEXT);
             INSERT INTO sessions VALUES ('s1', '/Users/me/{}');
             WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 10000)
             INSERT INTO assistant_usage_events (session_id, model, input_tokens, output_tokens,
                 cache_read_tokens, cache_write_tokens, reasoning_tokens, created_at)
             SELECT 's1', 'gpt-5-mini', 10, 1, 0, 0, 0, '2026-07-27T10:00:00.000Z' FROM r;",
            "d".repeat(900)
        ))
        .expect("store should be written");
        drop(conn);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 10_000);
        assert_eq!(total_volume(&collection), 110_000);
        assert_eq!(collection.stats.parse_errors, 0);
    }
}
