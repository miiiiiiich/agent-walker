use std::io::BufReader;
use std::path::Path;

use serde_json::Value;
use time::UtcOffset;

use crate::collector::FileEvents;
use crate::collector::ident::ident;
use crate::collector::json::parse_line;
use crate::collector::read::{Lines, open_regular};
use crate::model::SessionTouch;
use crate::timestamp::from_unix_seconds;

pub(in crate::collector::agy) fn parse_history_file(
    path: &Path,
    local_offset: UtcOffset,
) -> Option<FileEvents> {
    let mut lines = Lines::new(BufReader::new(open_regular(path).ok()?));
    let mut events = FileEvents::default();

    while let Some((_, line)) = lines.next(&mut events) {
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
        // A line without a conversationId (a slash command, say) names no
        // conversation; a session per such line counted sessions that never ran.
        let Some(session_id) = string_field(&value, "conversationId") else {
            continue;
        };
        events.session_touches.push(SessionTouch {
            timestamp,
            session_id,
        });
    }

    events.compress_touches(local_offset);
    Some(events)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .and_then(ident)
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_without_a_conversation_opens_no_session() {
        let temp = tempfile::TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("history.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"timestamp":1780726748000,"display":"/model","type":"slash_command","workspace":"/w"}"#,
                "\n",
                r#"{"timestamp":1780726749000,"display":"hi","workspace":"/w","conversationId":"c1"}"#,
            ),
        )
        .expect("history fixture should be written");

        let events = parse_history_file(&path, UtcOffset::UTC).expect("history should parse");

        let sessions: Vec<_> = events
            .session_touches
            .iter()
            .map(|touch| touch.session_id.as_str())
            .collect();
        assert_eq!(sessions, ["c1"]);
        assert_eq!(events.parse_errors, 0);
    }
}
