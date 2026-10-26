use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use time::UtcOffset;

use crate::collector::ident::{ident, ident_string};
use crate::collector::json::parse_bounded;
use crate::collector::project_from_cwd;
use crate::collector::store::Store;
use crate::model::{Collection, DurationEvent, SessionTouch, SourceKind, ToolEvent, UsageEvent};

use super::{Generation, ms_to_offset, token_usage};

#[derive(Default)]
struct SessionMeta {
    directory: Option<String>,
    copied_through_seq: Option<i64>,
}

pub(super) fn parse_session_messages(
    conn: &Store,
    floor_ms: i64,
    local_offset: UtcOffset,
    collection: &mut Collection,
    seen: &mut HashSet<String>,
    tools_from: &mut HashMap<String, Generation>,
) {
    if !conn.has_table(
        "session_message",
        &["id", "session_id", "type", "seq", "time_created", "data"],
    ) {
        return;
    }
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, session_id, type, seq, time_created, data FROM session_message \
         WHERE time_created >= ?1",
    ) else {
        return;
    };
    let sessions = session_meta(conn);
    let Ok(rows) = stmt.query_map(params![floor_ms], |row| {
        conn.charge(row, 6)?;
        Ok((
            row.get::<_, String>(0)?,
            ident_string(row.get::<_, String>(1)?),
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
        ))
    }) else {
        return;
    };

    for row in rows {
        collection.stats.lines_seen += 1;
        let Ok((id, Some(session_id), kind, seq, time_created, data)) = row else {
            collection.stats.parse_errors += 1;
            if conn.spent() {
                break;
            }
            continue;
        };
        if seen.contains(&id) {
            continue;
        }
        let meta = sessions.get(&session_id);
        // Rows at or below a fork's copy cutoff are the parent's history;
        // counted, every fork would replay the parent.
        if meta
            .and_then(|meta| meta.copied_through_seq)
            .is_some_and(|cutoff| seq <= cutoff)
        {
            continue;
        }
        let Some(value) = parse_bounded(&data) else {
            collection.stats.parse_errors += 1;
            continue;
        };
        seen.insert(id.clone());
        let created_ms = value
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or(time_created);
        let Some(timestamp) = ms_to_offset(created_ms, local_offset) else {
            continue;
        };
        collection.session_touches.push(SessionTouch {
            timestamp,
            session_id: session_id.clone(),
        });

        let has_usage = match kind.as_str() {
            "assistant" => true,
            "compaction" => value.get("tokens").is_some(),
            _ => false,
        };
        if !has_usage {
            continue;
        }
        collection.usage_events.push(UsageEvent {
            timestamp: Some(timestamp),
            session_id: Some(session_id.clone()),
            model: value
                .pointer("/model/id")
                .and_then(Value::as_str)
                .and_then(ident)
                .map(ToOwned::to_owned),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            attribution_skill: None,
            project: meta
                .and_then(|meta| meta.directory.as_deref())
                .filter(|dir| !dir.is_empty())
                .and_then(project_from_cwd),
            usage: token_usage(&value),
            granularity: crate::model::UsageGranularity::Call,
        });
        if kind == "assistant" {
            push_turn(&value, created_ms, &session_id, local_offset, collection);
            if let Entry::Vacant(slot) = tools_from.entry(id) {
                slot.insert(Generation::V2);
                push_tools(&value, created_ms, &session_id, local_offset, collection);
            }
        }
    }
}

fn push_turn(
    value: &Value,
    created_ms: i64,
    session_id: &str,
    local_offset: UtcOffset,
    collection: &mut Collection,
) {
    if let Some(completed_ms) = value.pointer("/time/completed").and_then(Value::as_i64) {
        collection.duration_events.push(DurationEvent {
            timestamp: ms_to_offset(created_ms, local_offset),
            session_id: Some(session_id.to_owned()),
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

fn push_tools(
    value: &Value,
    created_ms: i64,
    session_id: &str,
    local_offset: UtcOffset,
    collection: &mut Collection,
) {
    let tools = value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool"));
    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str).and_then(ident) else {
            continue;
        };
        let start_ms = tool
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or(created_ms);
        collection.tool_events.push(ToolEvent {
            timestamp: ms_to_offset(start_ms, local_offset),
            session_id: Some(session_id.to_owned()),
            tool_name: name.to_owned(),
            subagent_type: None,
            source_kind: SourceKind::Main,
        });
    }
}

fn session_meta(conn: &Store) -> HashMap<String, SessionMeta> {
    let mut sessions: HashMap<String, SessionMeta> = HashMap::new();
    for table in ["session_v2", "session"] {
        if !conn.has_table(table, &["id", "directory"]) {
            continue;
        }
        let Ok(mut stmt) = conn.prepare(&format!("SELECT id, directory FROM {table}")) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([], |row| {
            conn.charge(row, 2)?;
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        }) else {
            continue;
        };
        for (id, directory) in rows.flatten() {
            let meta = sessions.entry(id).or_default();
            if meta.directory.is_none() {
                meta.directory = directory;
            }
        }
    }
    for (fork, cutoff) in fork_cutoffs(conn) {
        sessions.entry(fork).or_default().copied_through_seq = Some(cutoff);
    }
    sessions
}

/// The highest parent `seq` each fork copied. A fork whose parent or
/// boundary row is gone keeps all its rows: they may be the only surviving
/// copy of that history.
fn fork_cutoffs(conn: &Store) -> Vec<(String, i64)> {
    if !conn.has_table("session_v2", &["id", "fork_session_id", "fork_boundary"]) {
        return Vec::new();
    }
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, fork_session_id, fork_boundary FROM session_v2 \
         WHERE fork_session_id IS NOT NULL AND fork_boundary IS NOT NULL",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| {
        conn.charge(row, 3)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|(fork, parent, boundary)| {
            let boundary = parse_bounded(&boundary)?;
            let inclusive = match boundary.get("type").and_then(Value::as_str)? {
                "through" => true,
                "before" => false,
                _ => return None,
            };
            let message = boundary.get("messageID").and_then(Value::as_str)?;
            let boundary_seq: i64 = conn
                .query_row(
                    "SELECT seq FROM session_message WHERE session_id = ?1 AND id = ?2",
                    params![parent, message],
                    |row| row.get(0),
                )
                .optional()
                .ok()??;
            let copied_sql = if inclusive {
                "SELECT max(seq) FROM session_message WHERE session_id = ?1 AND seq <= ?2"
            } else {
                "SELECT max(seq) FROM session_message WHERE session_id = ?1 AND seq < ?2"
            };
            let copied: Option<i64> = conn
                .query_row(copied_sql, params![parent, boundary_seq], |row| row.get(0))
                .ok()?;
            Some((fork, copied?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::Path;
    use tempfile::TempDir;

    use crate::collector::opencode::collect;
    use crate::collector::opencode::fixtures::{V2_SCHEMA, insert_v2, v2_assistant, volumes};

    #[test]
    fn v2_only_store_reads_assistant_and_compaction_usage() {
        let dir = TempDir::new().expect("tempdir");
        let conn = Connection::open(dir.path().join("opencode.db")).expect("open temp db");
        conn.execute_batch(V2_SCHEMA).expect("create schema");
        conn.execute(
            "INSERT INTO session_v2 VALUES ('s1', '/somewhere/proj', NULL, NULL, 1781542000000)",
            [],
        )
        .expect("insert session");
        insert_v2(
            &conn,
            "msg_u",
            "s1",
            "user",
            0,
            1_781_542_300_000,
            r#"{"text":"hi","time":{"created":1781542300000}}"#,
        );
        insert_v2(
            &conn,
            "msg_a",
            "s1",
            "assistant",
            1,
            1_781_542_363_256,
            &v2_assistant(1_781_542_363_256, 100, Some("bash")),
        );
        insert_v2(
            &conn,
            "msg_c",
            "s1",
            "compaction",
            2,
            1_781_542_400_000,
            r#"{"status":"completed","reason":"auto","summary":"s","model":{"id":"claude-sonnet-4.6","providerID":"anthropic"},"tokens":{"input":5000,"output":200,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1781542400000}}"#,
        );
        insert_v2(
            &conn,
            "msg_r",
            "s1",
            "compaction",
            3,
            1_781_542_500_000,
            r#"{"status":"running","reason":"auto","summary":"","time":{"created":1781542500000}}"#,
        );
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(volumes(&collection), vec![100 + 12 + 30 + 4, 5_200]);
        let assistant = &collection.usage_events[0];
        assert_eq!(assistant.model.as_deref(), Some("claude-sonnet-4.6"));
        assert_eq!(assistant.usage.output_tokens, 12);
        assert_eq!(assistant.project.as_deref(), Some("somewhere/proj"));
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.tool_events[0].tool_name, "bash");
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 1_000);
        assert_eq!(collection.session_touches.len(), 4);
    }

    fn write_fork_db(dir: &Path, boundary: &str) {
        let conn = Connection::open(dir.join("opencode.db")).expect("open temp db");
        conn.execute_batch(V2_SCHEMA).expect("create schema");
        conn.execute(
            "INSERT INTO session_v2 VALUES ('parent', '/somewhere/proj', NULL, NULL, 1781542000000)",
            [],
        )
        .expect("insert parent");
        conn.execute(
            "INSERT INTO session_v2 VALUES ('fork', '/somewhere/proj', 'parent', ?1, 1781549000000)",
            [boundary],
        )
        .expect("insert fork");
        let user = r#"{"text":"hi","time":{"created":1781542300000}}"#;
        let first = v2_assistant(1_781_542_400_000, 100, Some("read"));
        let second = v2_assistant(1_781_542_500_000, 200, None);
        insert_v2(&conn, "msg_u", "parent", "user", 0, 1_781_542_300_000, user);
        insert_v2(
            &conn,
            "msg_a",
            "parent",
            "assistant",
            1,
            1_781_542_400_000,
            &first,
        );
        insert_v2(
            &conn,
            "msg_b",
            "parent",
            "assistant",
            2,
            1_781_542_500_000,
            &second,
        );
        insert_v2(
            &conn,
            "msg_evt_0",
            "fork",
            "user",
            0,
            1_781_542_300_000,
            user,
        );
        insert_v2(
            &conn,
            "msg_evt_1",
            "fork",
            "assistant",
            1,
            1_781_542_400_000,
            &first,
        );
        insert_v2(
            &conn,
            "msg_own",
            "fork",
            "assistant",
            2,
            1_781_549_100_000,
            &v2_assistant(1_781_549_100_000, 400, None),
        );
    }

    /// Fork rows at or below the copy cutoff are not recounted; the fork's
    /// own rows count.
    #[test]
    fn fork_copies_are_not_recounted() {
        for boundary in [
            r#"{"type":"through","messageID":"msg_a"}"#,
            r#"{"type":"before","messageID":"msg_b"}"#,
        ] {
            let dir = TempDir::new().expect("tempdir");
            write_fork_db(dir.path(), boundary);

            let collection = collect(dir.path(), None, false, UtcOffset::UTC);

            assert_eq!(volumes(&collection), vec![146, 246, 446], "{boundary}");
            assert_eq!(collection.tool_events.len(), 1, "{boundary}");
            let fork_touches = collection
                .session_touches
                .iter()
                .filter(|touch| touch.session_id == "fork")
                .count();
            assert_eq!(fork_touches, 1, "{boundary}");
        }
    }

    #[test]
    fn unresolvable_fork_boundary_keeps_every_row() {
        let dir = TempDir::new().expect("tempdir");
        write_fork_db(dir.path(), r#"{"type":"through","messageID":"msg_gone"}"#);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 4);
    }
}
