use std::collections::{HashMap, HashSet};

use rusqlite::params;
use serde_json::Value;

use crate::collector::ident::ident;
use crate::collector::json::parse_bounded;
use crate::collector::store::Store;
use crate::model::Collection;

pub(super) struct PartTool {
    pub(super) name: String,
    pub(super) start_ms: i64,
}

/// Assistant messages whose content OpenCode moved into `session_part` rows,
/// and the tool parts among them. OpenCode itself reads such a message's
/// parts in place of its `content`, so the two are never combined.
#[derive(Default)]
pub(super) struct SplitParts {
    split: HashSet<String>,
    tools: HashMap<String, Vec<PartTool>>,
}

impl SplitParts {
    /// `None` when the message keeps its content inline.
    pub(super) fn tools_of(&self, message_id: &str) -> Option<&[PartTool]> {
        self.split
            .contains(message_id)
            .then(|| self.tools.get(message_id).map_or(&[][..], Vec::as_slice))
    }
}

pub(super) fn split_parts(conn: &Store, floor_ms: i64, collection: &mut Collection) -> SplitParts {
    let mut parts = SplitParts::default();
    if !conn.has_table(
        "session_part",
        &["message_id", "ordinal", "type", "time_created", "data"],
    ) {
        return parts;
    }
    if let Ok(mut stmt) =
        conn.prepare("SELECT DISTINCT message_id FROM session_part WHERE time_created >= ?1")
        && let Ok(rows) = stmt.query_map(params![floor_ms], |row| {
            conn.charge(row, 1)?;
            row.get::<_, String>(0)
        })
    {
        parts.split.extend(rows.flatten());
    }
    // The `type` column, not `data.type`: OpenCode merges the column into the
    // part when it decodes one, so the stored data need not repeat it.
    let Ok(mut stmt) = conn.prepare(
        "SELECT message_id, time_created, data FROM session_part \
         WHERE type = 'tool' AND time_created >= ?1 ORDER BY message_id, ordinal",
    ) else {
        return parts;
    };
    let Ok(rows) = stmt.query_map(params![floor_ms], |row| {
        conn.charge(row, 3)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    }) else {
        return parts;
    };
    for row in rows {
        collection.stats.lines_seen += 1;
        let Ok((message_id, time_created, data)) = row else {
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
        let Some(name) = value.get("name").and_then(Value::as_str).and_then(ident) else {
            continue;
        };
        parts.tools.entry(message_id).or_default().push(PartTool {
            name: name.to_owned(),
            start_ms: value
                .pointer("/time/created")
                .and_then(Value::as_i64)
                .unwrap_or(time_created),
        });
    }
    parts
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use tempfile::TempDir;
    use time::UtcOffset;

    use super::*;
    use crate::collector::opencode::collect;
    use crate::collector::opencode::fixtures::{V2_SCHEMA, insert_v2, v2_assistant, volumes};

    const PART_SCHEMA: &str = "CREATE TABLE session_part (message_id TEXT NOT NULL, session_id TEXT NOT NULL, ordinal INTEGER NOT NULL, type TEXT NOT NULL, call_id TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY (message_id, ordinal));";

    fn insert_part(conn: &Connection, message: &str, ordinal: i64, kind: &str, data: &str) {
        conn.execute(
            "INSERT INTO session_part VALUES (?1, 's1', ?2, ?3, NULL, 1781542363300, 1781542363300, ?4)",
            params![message, ordinal, kind, data],
        )
        .expect("insert session_part");
    }

    fn tool_names(collection: &Collection) -> Vec<&str> {
        let mut names: Vec<&str> = collection
            .tool_events
            .iter()
            .map(|tool| tool.tool_name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    fn split_store(dir: &std::path::Path) -> Connection {
        let conn = Connection::open(dir.join("opencode.db")).expect("open temp db");
        conn.execute_batch(V2_SCHEMA).expect("create schema");
        conn.execute_batch(PART_SCHEMA).expect("create part schema");
        conn.execute(
            "INSERT INTO session_v2 VALUES ('s1', '/somewhere/proj', NULL, NULL, 1781542000000)",
            [],
        )
        .expect("insert session");
        conn
    }

    /// A split message reads its tools from its part rows, whose data may
    /// leave `type` to the column; its tokens still come from the message.
    #[test]
    fn split_message_reads_tools_from_its_parts() {
        let dir = TempDir::new().expect("tempdir");
        let conn = split_store(dir.path());
        insert_v2(
            &conn,
            "msg_a",
            "s1",
            "assistant",
            1,
            1_781_542_363_256,
            &v2_assistant(1_781_542_363_256, 100, None),
        );
        insert_part(&conn, "msg_a", 0, "text", r#"{"type":"text","text":"hi"}"#);
        insert_part(
            &conn,
            "msg_a",
            1,
            "tool",
            r#"{"type":"tool","id":"call-1","name":"bash","time":{"created":1781542363400}}"#,
        );
        insert_part(
            &conn,
            "msg_a",
            2,
            "tool",
            r#"{"id":"call-2","name":"read"}"#,
        );
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(volumes(&collection), vec![100 + 12 + 30 + 4]);
        assert_eq!(tool_names(&collection), vec!["bash", "read"]);
        assert!(
            collection
                .tool_events
                .iter()
                .all(|tool| tool.timestamp.is_some())
        );
    }

    /// Part rows replace inline content, as OpenCode reads them, so a tool
    /// left in both places counts once.
    #[test]
    fn parts_win_over_inline_content() {
        let dir = TempDir::new().expect("tempdir");
        let conn = split_store(dir.path());
        insert_v2(
            &conn,
            "msg_a",
            "s1",
            "assistant",
            1,
            1_781_542_363_256,
            &v2_assistant(1_781_542_363_256, 100, Some("bash")),
        );
        insert_part(
            &conn,
            "msg_a",
            0,
            "tool",
            r#"{"type":"tool","id":"call-1","name":"bash"}"#,
        );
        insert_v2(
            &conn,
            "msg_b",
            "s1",
            "assistant",
            2,
            1_781_542_500_000,
            &v2_assistant(1_781_542_500_000, 200, Some("glob")),
        );
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(tool_names(&collection), vec!["bash", "glob"]);
    }

    /// A fork copies a split parent message's parts under the fork's message
    /// id; the copy sits at or below the fork cutoff and is skipped with it.
    #[test]
    fn forked_copy_of_split_parts_is_not_recounted() {
        let dir = TempDir::new().expect("tempdir");
        let conn = split_store(dir.path());
        conn.execute(
            "INSERT INTO session_v2 VALUES ('fork', '/somewhere/proj', 's1', ?1, 1781549000000)",
            [r#"{"type":"through","messageID":"msg_a"}"#],
        )
        .expect("insert fork");
        let parent = v2_assistant(1_781_542_363_256, 100, None);
        insert_v2(
            &conn,
            "msg_a",
            "s1",
            "assistant",
            1,
            1_781_542_363_256,
            &parent,
        );
        insert_v2(
            &conn,
            "msg_f1",
            "fork",
            "assistant",
            1,
            1_781_542_363_256,
            &parent,
        );
        let tool = r#"{"type":"tool","id":"call-1","name":"bash"}"#;
        insert_part(&conn, "msg_a", 0, "tool", tool);
        insert_part(&conn, "msg_f1", 0, "tool", tool);
        drop(conn);

        let collection = collect(dir.path(), None, false, UtcOffset::UTC);

        assert_eq!(tool_names(&collection), vec!["bash"]);
        assert_eq!(collection.usage_events.len(), 1);
    }
}
