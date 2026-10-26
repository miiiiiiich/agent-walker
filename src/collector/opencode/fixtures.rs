use std::path::Path;

use rusqlite::{Connection, params};

use crate::model::Collection;

pub(super) const ASSISTANT_MS: i64 = 1_781_542_363_256;
pub(super) const ASSISTANT_DONE_MS: i64 = 1_781_542_436_778;

pub(super) fn write_db(dir: &Path) {
    write_db_at(&dir.join("opencode.db"));
}

pub(super) fn write_db_at(db: &Path) {
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

pub(super) const V2_SCHEMA: &str = "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL, fork_session_id TEXT, fork_boundary TEXT, time_created INTEGER NOT NULL);
         CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);";

pub(super) fn v2_assistant(created_ms: i64, input: u64, tool: Option<&str>) -> String {
    let content = tool.map_or_else(String::new, |name| {
        format!(r#"{{"type":"tool","id":"call-{created_ms}","name":"{name}","state":{{"status":"completed"}},"time":{{"created":{created_ms}}}}}"#)
    });
    format!(
        r#"{{"agent":"build","model":{{"id":"claude-sonnet-4.6","providerID":"anthropic"}},"content":[{content}],"finish":"stop","tokens":{{"input":{input},"output":10,"reasoning":2,"cache":{{"read":30,"write":4}}}},"time":{{"created":{created_ms},"completed":{done}}}}}"#,
        done = created_ms + 1_000
    )
}

pub(super) fn insert_v2(
    conn: &Connection,
    id: &str,
    session: &str,
    kind: &str,
    seq: i64,
    created_ms: i64,
    data: &str,
) {
    conn.execute(
        "INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        params![id, session, kind, seq, created_ms, data],
    )
    .expect("insert session_message");
}

pub(super) fn volumes(collection: &Collection) -> Vec<u64> {
    let mut volumes: Vec<u64> = collection
        .usage_events
        .iter()
        .map(|event| event.usage.token_volume())
        .collect();
    volumes.sort_unstable();
    volumes
}
