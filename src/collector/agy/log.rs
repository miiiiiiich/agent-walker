use std::io::BufReader;
use std::path::Path;

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::collector::ident::ident;
use crate::collector::read::{Lines, open_regular};
use crate::collector::{FileEvents, KeyedToolEvent};
use crate::model::{SessionTouch, SourceKind, ToolEvent};
use crate::timestamp::plausible;

pub(in crate::collector::agy) fn parse_log_file(
    path: &Path,
    local_offset: UtcOffset,
) -> Option<FileEvents> {
    let mut lines = Lines::new(BufReader::new(open_regular(path).ok()?));
    let mut events = FileEvents::default();
    let log_session_id = fallback_session_id(path);
    let mut current_conversation_id = None;

    while let Some((_, line)) = lines.next(&mut events) {
        let Ok(line) = std::str::from_utf8(line) else {
            events.parse_errors += 1;
            continue;
        };
        let timestamp = parse_log_timestamp(path, line, local_offset);

        if line.contains("HandleUserInput called with text") {
            let session_id = current_conversation_id
                .clone()
                .or_else(|| log_session_id.clone());
            if let (Some(timestamp), Some(session_id)) = (timestamp, session_id) {
                events.session_touches.push(SessionTouch {
                    timestamp,
                    session_id,
                });
            }
            continue;
        }

        if line.contains("Responding to tool confirmation") {
            current_conversation_id = value_after(line, "convID=").or(current_conversation_id);
            let session_id = current_conversation_id
                .clone()
                .or_else(|| log_session_id.clone());
            if let (Some(timestamp), Some(session_id)) = (timestamp, session_id) {
                events.session_touches.push(SessionTouch {
                    timestamp,
                    session_id: session_id.clone(),
                });
                events.tool_events.push(KeyedToolEvent {
                    key: None,
                    event: ToolEvent {
                        timestamp: Some(timestamp),
                        session_id: Some(session_id),
                        tool_name: command_grant(line)
                            .unwrap_or_else(|| "ToolConfirmation".to_owned()),
                        subagent_type: None,
                        source_kind: SourceKind::Main,
                    },
                });
            }
        }
    }

    events.compress_touches(local_offset);
    Some(events)
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

fn value_after(line: &str, marker: &str) -> Option<String> {
    let start = line.find(marker)? + marker.len();
    let rest = line.get(start..)?;
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

fn command_grant(line: &str) -> Option<String> {
    let command_start = line.find("command(")? + "command(".len();
    let rest = line.get(command_start..)?;
    let command = rest.get(0..rest.find(')')?)?.trim();
    let executable = ident(command.split_whitespace().next()?)?;
    if executable.is_empty() {
        None
    } else {
        Some(format!("command:{executable}"))
    }
}

fn fallback_session_id(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}
