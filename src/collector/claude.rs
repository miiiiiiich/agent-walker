use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::collector::{
    FileEvents, KeyedToolEvent, KeyedUsageEvent, list_files, merge_into, parse_files_cached,
    project_from_cwd,
};
use crate::model::{
    Collection, DurationEvent, Provider, SessionTouch, SourceKind, TokenUsage, ToolEvent,
    UsageEvent,
};

/// Background/observer harnesses (e.g. the claude-mem observer) keep
/// always-on sessions that would inflate session and active-day stats.
const NOISE_DIR_MARKERS: [&str; 1] = ["claude-mem-observer"];

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
) -> Result<Collection> {
    let mut collection = Collection::new(Provider::Claude, root.to_path_buf());
    if !root.exists() {
        return Ok(collection);
    }

    let files: Vec<_> = list_files(root, "jsonl", mtime_floor, &mut collection.stats)
        .into_iter()
        .filter(|path| !is_noise_path(path))
        .collect();
    let per_file = parse_files_cached(use_cache.then_some("claude"), &files, |path| {
        parse_file(path, root)
    });
    merge_into(&mut collection, per_file);
    Ok(collection)
}

fn is_noise_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| NOISE_DIR_MARKERS.iter().any(|marker| name.contains(marker)))
    })
}

/// Fallback project label from the sanitized directory name. Claude Code
/// flattens "/" and "." to "-" in directory names (irreversibly), so this is
/// only used for files whose lines carry no raw `cwd` field.
fn project_label(path: &Path, root: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let directory = relative.components().next()?.as_os_str().to_str()?;
    Some(normalize_project_name(directory))
}

fn normalize_project_name(directory: &str) -> String {
    let home_prefix = std::env::var("HOME")
        .map(|home| format!("{}-", home.replace('/', "-")))
        .unwrap_or_default();
    let trimmed = directory
        .strip_prefix(&home_prefix)
        .unwrap_or(directory)
        .trim_start_matches('-');
    trimmed.to_owned()
}

fn parse_file(path: &Path, root: &Path) -> Option<FileEvents> {
    let file = File::open(path).ok()?;
    let mut events = FileEvents::default();
    let source_kind = path_source_kind(path);
    let file_agent_id = file_agent_id(path);
    // Prefer the raw `cwd` carried on log lines (real slashes); the sanitized
    // directory name is a lossy fallback.
    let mut project = None;
    let fallback_project = project_label(path, root);
    // Claude logs carry no explicit completion event; derive turn durations
    // as "human prompt -> last activity before the next human prompt".
    let mut turn_start: Option<OffsetDateTime> = None;
    let mut last_activity: Option<OffsetDateTime> = None;
    let reader = BufReader::new(file);

    for (line_index, line) in reader.lines().enumerate() {
        events.lines_seen += 1;
        let Ok(line) = line else {
            events.parse_errors += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            events.parse_errors += 1;
            continue;
        };
        if project.is_none()
            && let Some(cwd) = string_field(&value, "cwd")
        {
            project = Some(project_from_cwd(&cwd));
        }
        if source_kind == SourceKind::Main
            && let Some(timestamp) = parse_timestamp(value.get("timestamp"))
        {
            if is_human_turn(&value) {
                push_turn_duration(turn_start, last_activity, &mut events);
                turn_start = Some(timestamp);
                last_activity = Some(timestamp);
            } else if let Some(previous) = last_activity {
                // A long silence means the turn ended and the session was
                // resumed later (compaction, scheduled appends); close the
                // turn at the last real activity instead of spanning days.
                if timestamp - previous > Duration::minutes(30) {
                    push_turn_duration(turn_start, last_activity, &mut events);
                    turn_start = None;
                    last_activity = None;
                } else {
                    last_activity = Some(previous.max(timestamp));
                }
            }
        }
        parse_line(
            &value,
            path,
            line_index,
            source_kind,
            file_agent_id.as_deref(),
            project.as_deref().or(fallback_project.as_deref()),
            &mut events,
        );
    }

    push_turn_duration(turn_start, last_activity, &mut events);
    events.compress_touches();
    Some(events)
}

/// A line that starts a human turn: a user message that is an actual prompt,
/// not a `tool_result` carrier or meta record.
fn is_human_turn(value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if value
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .get("isMeta")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return false;
    }
    match value
        .get("message")
        .and_then(|message| message.get("content"))
    {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => {
            let has_text = blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("text"));
            let has_tool_result = blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"));
            has_text && !has_tool_result
        }
        _ => false,
    }
}

fn push_turn_duration(
    turn_start: Option<OffsetDateTime>,
    last_activity: Option<OffsetDateTime>,
    events: &mut FileEvents,
) {
    let (Some(start), Some(end)) = (turn_start, last_activity) else {
        return;
    };
    let duration_ms = u64::try_from((end - start).whole_milliseconds()).unwrap_or(0);
    if duration_ms == 0 {
        return;
    }
    events.duration_events.push(DurationEvent {
        timestamp: Some(start),
        session_id: None,
        duration_ms,
        status: Some("turn".to_owned()),
    });
}

#[allow(
    clippy::too_many_arguments,
    reason = "Per-line parse context; bundling into a struct adds noise for one caller."
)]
fn parse_line(
    value: &Value,
    path: &Path,
    line_index: usize,
    source_kind: SourceKind,
    file_agent_id: Option<&str>,
    project: Option<&str>,
    events: &mut FileEvents,
) {
    let timestamp = parse_timestamp(value.get("timestamp"));
    let session_id = string_field(value, "sessionId")
        .or_else(|| string_field(value, "session_id"))
        .or_else(|| string_field(value, "session_id_v2"));
    if let (Some(timestamp), Some(session_id)) = (timestamp, session_id.as_ref()) {
        events.session_touches.push(SessionTouch {
            timestamp,
            session_id: session_id.clone(),
        });
    }

    let is_sidechain = value
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let event_source_kind = if is_sidechain {
        SourceKind::Subagent
    } else {
        source_kind
    };
    let attribution_agent = string_field(value, "attributionAgent")
        .or_else(|| string_field(value, "attribution_agent"))
        .or_else(|| {
            if event_source_kind == SourceKind::Subagent {
                file_agent_id.map(ToOwned::to_owned)
            } else {
                None
            }
        });

    let Some(message) = value.get("message") else {
        return;
    };

    if let Some(usage) = parse_usage(message.get("usage").or_else(|| value.get("usage"))) {
        let model = string_field(message, "model").or_else(|| string_field(value, "model"));
        events.usage_events.push(KeyedUsageEvent {
            key: string_field(message, "id").map(|message_id| format!("message:{message_id}")),
            event: UsageEvent {
                timestamp,
                session_id: session_id.clone(),
                model,
                source_kind: event_source_kind,
                attribution_agent: attribution_agent.clone(),
                project: project.map(ToOwned::to_owned),
                usage,
            },
        });
    }

    collect_tool_events(
        message,
        timestamp,
        session_id.as_ref(),
        event_source_kind,
        path,
        line_index,
        events,
    );
}

fn collect_tool_events(
    message: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    source_kind: SourceKind,
    path: &Path,
    line_index: usize,
    events: &mut FileEvents,
) {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return;
    };

    for (block_index, block) in blocks.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(tool_name) = string_field(block, "name") else {
            continue;
        };
        let key = string_field(block, "id").map_or_else(
            || format!("{}:{}:{block_index}", path.display(), line_index + 1),
            |tool_id| format!("tool:{tool_id}"),
        );
        let subagent_type = block
            .get("input")
            .and_then(|input| string_field(input, "subagent_type"));
        events.tool_events.push(KeyedToolEvent {
            key: Some(key),
            event: ToolEvent {
                timestamp,
                session_id: session_id.cloned(),
                tool_name,
                subagent_type,
                source_kind,
            },
        });
    }
}

fn parse_usage(value: Option<&Value>) -> Option<TokenUsage> {
    let value = value?;
    let input_tokens = u64_field(value, "input_tokens");
    let output_tokens = u64_field(value, "output_tokens");
    let cache_creation_input_tokens = u64_field(value, "cache_creation_input_tokens");
    let cache_read_input_tokens = u64_field(value, "cache_read_input_tokens");
    if input_tokens == 0
        && output_tokens == 0
        && cache_creation_input_tokens == 0
        && cache_read_input_tokens == 0
    {
        return None;
    }

    let mut usage = TokenUsage {
        input_tokens,
        output_tokens,
        cache_creation_input_tokens,
        cache_read_input_tokens,
        ..TokenUsage::default()
    };

    if let Some(cache_creation) = value.get("cache_creation") {
        usage.cache_creation_ephemeral_1h_input_tokens =
            u64_field(cache_creation, "ephemeral_1h_input_tokens");
        usage.cache_creation_ephemeral_5m_input_tokens =
            u64_field(cache_creation, "ephemeral_5m_input_tokens");
    }

    if let Some(server_tool_use) = value.get("server_tool_use").and_then(Value::as_object) {
        for (key, child) in server_tool_use {
            if let Some(count) = child.as_u64() {
                usage.server_tool_use.insert(key.clone(), count);
            }
        }
    }

    Some(usage)
}

fn parse_timestamp(value: Option<&Value>) -> Option<OffsetDateTime> {
    let raw = value?.as_str()?;
    OffsetDateTime::parse(raw, &Rfc3339).ok()
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Token counts are untrusted log data; clamp to a generous sanity bound so
/// downstream sums of a handful of fields can never overflow u64.
fn u64_field(value: &Value, key: &str) -> u64 {
    const MAX_SANE_TOKENS: u64 = 1 << 50;
    value
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_SANE_TOKENS)
}

fn path_source_kind(path: &Path) -> SourceKind {
    if path
        .components()
        .any(|component| component.as_os_str() == "subagents")
    {
        SourceKind::Subagent
    } else {
        SourceKind::Main
    }
}

fn file_agent_id(path: &Path) -> Option<String> {
    let file_name = path.file_stem()?.to_str()?;
    file_name
        .strip_prefix("agent-")
        .map(|id| format!("agent-{id}"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn collects_usage_tools_and_subagent_files() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let project_dir = temp.path().join("project");
        let subagent_dir = project_dir.join("subagents");
        fs::create_dir_all(&subagent_dir).expect("test dirs should be created");

        fs::write(
            project_dir.join("session.jsonl"),
            r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30},"content":[{"type":"tool_use","name":"Agent","input":{"subagent_type":"Explore"}},{"type":"tool_use","name":"Read","input":{"file_path":"/secret"}}]}}"#,
        )
        .expect("main fixture should be written");
        fs::write(
            subagent_dir.join("agent-abc.jsonl"),
            r#"{"timestamp":"2026-06-01T00:01:00Z","sessionId":"s1","isSidechain":true,"attributionAgent":"Explore","message":{"id":"m2","model":"claude-haiku-4-5","usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":10},"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo hidden"}}]}}"#,
        )
        .expect("subagent fixture should be written");

        let collection = collect(temp.path(), None, false).expect("collection should succeed");

        assert_eq!(collection.stats.files_seen, 2);
        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(collection.tool_events.len(), 3);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 63);
        assert_eq!(collection.usage_events[1].source_kind, SourceKind::Subagent);
        assert_eq!(
            collection.tool_events[0].subagent_type.as_deref(),
            Some("Explore")
        );
    }

    #[test]
    fn skips_malformed_lines_without_aborting() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            "not-json\n{\"timestamp\":\"2026-06-01T00:00:00Z\",\"sessionId\":\"s1\",\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-4-8\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n",
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false).expect("collection should succeed");

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(collection.usage_events.len(), 1);
    }

    #[test]
    fn deduplicates_usage_and_tool_use_ids() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let line = r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30},"content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/secret"}}]}}"#;
        fs::write(
            temp.path().join("session.jsonl"),
            format!("{line}\n{line}\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false).expect("collection should succeed");

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 63);
    }

    #[test]
    fn skips_files_older_than_mtime_floor() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("old.jsonl"),
            r#"{"timestamp":"2026-01-01T00:00:00Z","sessionId":"old","message":{"id":"m0","usage":{"input_tokens":1,"output_tokens":1}}}"#,
        )
        .expect("fixture should be written");

        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        let collection =
            collect(temp.path(), Some(future), false).expect("collection should succeed");

        assert_eq!(collection.stats.files_seen, 0);
        assert!(collection.usage_events.is_empty());
    }
}
