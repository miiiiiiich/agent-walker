use std::io::BufReader;
use std::path::Path;

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::collector::ident::ident;
use crate::collector::read::{Lines, open_regular};
use crate::collector::{FileEvents, KeyedToolEvent};
use crate::model::{SessionTouch, SourceKind, ToolEvent};
use crate::timestamp::plausible;

use tools::{auto_approved_tool, confirmed_tool};

mod tools;

const CONVERSATION_MARKERS: [&str; 4] = [
    "Sending user message to conversation ",
    "Created conversation ",
    "Print mode: conversation=",
    "Responding to tool confirmation: convID=",
];

pub(in crate::collector::agy) fn parse_log_file(
    path: &Path,
    local_offset: UtcOffset,
) -> Option<FileEvents> {
    let mut lines = Lines::new(BufReader::new(open_regular(path).ok()?));
    let mut events = FileEvents::default();
    let mut conversation_id = None;
    // An input is logged just before the line naming its conversation.
    // Attached to the last conversation named instead, a run's first inputs
    // opened a session of their own for the log file.
    let mut pending_inputs = Vec::new();

    while let Some((_, line)) = lines.next(&mut events) {
        let Ok(line) = std::str::from_utf8(line) else {
            events.parse_errors += 1;
            continue;
        };
        // Markers match only at the start of a message, so prompt or tool
        // text logged inside another message can't pass for one.
        let Some(message) = log_message(line) else {
            continue;
        };
        let timestamp = parse_log_timestamp(path, line, local_offset);

        if message.starts_with("HandleUserInput called with text") {
            pending_inputs.extend(timestamp);
            continue;
        }

        if let Some(id) = CONVERSATION_MARKERS
            .iter()
            .find_map(|marker| message.strip_prefix(marker))
            .and_then(leading_id)
        {
            events
                .session_touches
                .extend(pending_inputs.drain(..).map(|timestamp| SessionTouch {
                    timestamp,
                    session_id: id.clone(),
                }));
            conversation_id = Some(id);
        }

        let confirmation = message.starts_with("Responding to tool confirmation");
        let tool = if confirmation {
            confirmed_tool(message)
        } else {
            auto_approved_tool(message)
        };
        if !confirmation && tool.is_none() {
            continue;
        }
        let session_id = conversation_id
            .clone()
            .or_else(|| fallback_session_id(path));
        let (Some(timestamp), Some(session_id)) = (timestamp, session_id) else {
            continue;
        };
        events.session_touches.push(SessionTouch {
            timestamp,
            session_id: session_id.clone(),
        });
        if let Some(tool_name) = tool {
            events.tool_events.push(KeyedToolEvent {
                key: None,
                event: ToolEvent {
                    timestamp: Some(timestamp),
                    session_id: Some(session_id),
                    tool_name,
                    subagent_type: None,
                    source_kind: SourceKind::Main,
                },
            });
        }
    }

    // Inputs no later line named a conversation for (the run ended first, or
    // the build logs no ids) stay with the last one named, else the log file.
    if let Some(session_id) = conversation_id.or_else(|| fallback_session_id(path)) {
        events
            .session_touches
            .extend(pending_inputs.into_iter().map(|timestamp| SessionTouch {
                timestamp,
                session_id: session_id.clone(),
            }));
    }

    events.compress_touches(local_offset);
    Some(events)
}

fn log_message(line: &str) -> Option<&str> {
    let (header, message) = line.split_once("] ")?;
    let (_, source_line) = header.rsplit_once(".go:")?;
    (!source_line.is_empty() && source_line.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(message)
}

fn parse_log_timestamp(path: &Path, line: &str, local_offset: UtcOffset) -> Option<OffsetDateTime> {
    let mut parts = line.split_whitespace();
    let severity_date = parts.next()?;
    let time_part = parts.next()?;
    let year = log_year(path)?;
    if severity_date.len() != 5 {
        return None;
    }
    let month = severity_date.get(1..3)?.parse::<u8>().ok()?;
    let day = severity_date.get(3..5)?.parse::<u8>().ok()?;
    let mut time_parts = time_part.split([':', '.']);
    let hour = time_parts.next()?.parse::<u8>().ok()?;
    let minute = time_parts.next()?.parse::<u8>().ok()?;
    let second = time_parts.next()?.parse::<u8>().ok()?;
    let microsecond = time_parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let date = Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()?;
    let time = Time::from_hms_micro(hour, minute, second, microsecond).ok()?;
    // Log lines carry no timezone; the CLI wrote them in this machine's local
    // time, so assume the local offset, not UTC.
    plausible(PrimitiveDateTime::new(date, time).assume_offset(local_offset))
}

fn log_year(path: &Path) -> Option<i32> {
    let file_stem = path.file_stem()?.to_str()?;
    let raw_date = file_stem.strip_prefix("cli-")?.get(0..8)?;
    raw_date.get(0..4)?.parse::<i32>().ok()
}

fn leading_id(rest: &str) -> Option<String> {
    let end = rest
        .find(|character: char| character.is_whitespace() || character == ',')
        .unwrap_or(rest.len());
    let value = rest.get(0..end)?;
    if value.is_empty() {
        None
    } else {
        ident(value).map(ToOwned::to_owned)
    }
}

fn fallback_session_id(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions_of(log: &str) -> Vec<String> {
        let temp = tempfile::TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("cli-20261119_091704.log");
        std::fs::write(&path, log).expect("log fixture should be written");
        let events = parse_log_file(&path, UtcOffset::UTC).expect("log should parse");
        let mut sessions: Vec<_> = events
            .session_touches
            .into_iter()
            .map(|touch| touch.session_id)
            .collect();
        sessions.dedup();
        sessions
    }

    const INPUT: &str =
        r#"I1119 09:18:34.813222    2920 input_loop.go:104] HandleUserInput called with text: "x""#;

    #[test]
    fn input_joins_the_conversation_named_after_it() {
        for named in [
            "I1119 09:18:34.822544    2933 server.go:1851] Sending user message to conversation c1 (items=1, media=0)",
            "I1119 09:18:34.822544    2933 server.go:1258] Created conversation c1 (backend=BACKEND_TYPE_UNSPECIFIED)",
            "I1119 09:18:34.822544       1 session.go:201] Print mode: conversation=c1, sending message",
            "I1119 09:18:34.822544     214 input_loop.go:805] Responding to tool confirmation: convID=c1, stepIdx=2, approved=true",
        ] {
            assert_eq!(
                sessions_of(&format!("{INPUT}\n{named}\n")),
                ["c1"],
                "{named}"
            );
        }
    }

    #[test]
    fn each_input_joins_the_next_conversation_named() {
        let log = format!(
            "{INPUT}\n\
             I1119 09:18:35.000000 1 server.go:1851] Sending user message to conversation c1 (items=1, media=0)\n\
             {INPUT}\n\
             I1119 09:18:36.000000 1 server.go:1851] Sending user message to conversation c2 (items=1, media=0)\n"
        );
        assert_eq!(sessions_of(&log), ["c1", "c2"]);
    }

    #[test]
    fn unnamed_input_stays_with_the_last_conversation_else_the_log() {
        let named_before = format!(
            "I1119 09:18:30.000000 1 server.go:1258] Created conversation c1 (backend=X)\n{INPUT}\n"
        );
        assert_eq!(sessions_of(&named_before), ["c1"]);
        assert_eq!(sessions_of(&format!("{INPUT}\n")), ["cli-20261119_091704"]);
    }

    #[test]
    fn auto_approvals_count_as_tools_in_the_current_conversation() {
        let temp = tempfile::TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("cli-20261119_091704.log");
        std::fs::write(
            &path,
            "I1119 09:18:34.822544 1 server.go:1851] Sending user message to conversation c1 (items=1, media=0)\n\
             I1119 10:41:19.413748 1 tool_confirmation_manager.go:186] auto mode: auto-approving file write \"Edit\" at step 57\n\
             I1119 10:42:00.000000 1 input_loop.go:805] Responding to tool confirmation: convID=c1, stepIdx=58, approved=false, sandboxOverride=false, scope=PERMISSION_SCOPE_UNSPECIFIED, persistGrants=[], denyGrants=[command(rm)]\n",
        )
        .expect("log fixture should be written");

        let events = parse_log_file(&path, UtcOffset::UTC).expect("log should parse");

        let tools: Vec<_> = events
            .tool_events
            .iter()
            .map(|tool| {
                (
                    tool.event.tool_name.as_str(),
                    tool.event.session_id.as_deref(),
                )
            })
            .collect();
        assert_eq!(tools, [("Edit", Some("c1"))]);
        assert_eq!(events.session_touches.len(), 2);
    }

    #[test]
    fn markers_inside_another_message_name_nothing() {
        let log = "I1119 09:18:34.813222 1 input_loop.go:104] HandleUserInput called with text: \"Sending user message to conversation evil\"\n\
             I1119 09:18:34.900000 1 tool.go:12] output: Created conversation evil\n\
             Sending user message to conversation evil\n\
             I1119 09:18:35.000000 1 server.go:1851] Sending user message to conversation c1 (items=1, media=0)\n";
        assert_eq!(sessions_of(log), ["c1"]);
    }
}
