use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::collector::{
    FileEvents, KeyedToolEvent, KeyedUsageEvent, list_files, merge_into, parse_files_cached,
    project_from_cwd,
};
use crate::model::{
    Collection, DurationEvent, Provider, SessionTouch, SourceKind, TokenUsage, ToolEvent,
    UsageEvent,
};

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
) -> Result<Collection> {
    let mut collection = Collection::new(Provider::Codex, root.to_path_buf());
    if !root.exists() {
        return Ok(collection);
    }

    let files = list_files(root, "jsonl", mtime_floor, &mut collection.stats);
    let per_file = parse_files_cached(use_cache.then_some("codex"), &files, parse_file);
    merge_into(&mut collection, per_file);
    Ok(collection)
}

fn parse_file(path: &Path) -> Option<FileEvents> {
    let file = File::open(path).ok()?;
    let mut events = FileEvents::default();
    let mut current_session_id = fallback_session_id(path);
    let mut current_model = None;
    let mut current_project = None;
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

        let timestamp = parse_timestamp(value.get("timestamp"));
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            current_session_id = string_path(&value, &["payload", "id"]).or(current_session_id);
            current_model = session_model(&value).or(current_model);
            current_project =
                string_path(&value, &["payload", "cwd"]).map(|cwd| project_from_cwd(&cwd));
        }
        if value.get("type").and_then(Value::as_str) == Some("turn_context") {
            current_model = string_path(&value, &["payload", "model"]).or(current_model);
        }

        if let (Some(timestamp), Some(session_id)) = (timestamp, current_session_id.as_ref()) {
            events.session_touches.push(SessionTouch {
                timestamp,
                session_id: session_id.clone(),
            });
        }

        collect_usage_event(
            &value,
            timestamp,
            current_session_id.as_ref(),
            current_model.as_ref(),
            current_project.as_deref(),
            &mut events,
        );
        collect_duration_event(&value, timestamp, current_session_id.as_ref(), &mut events);
        collect_tool_event(
            &value,
            timestamp,
            current_session_id.as_ref(),
            path,
            line_index,
            &mut events,
        );
    }

    events.compress_touches();
    Some(events)
}

fn collect_usage_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    model: Option<&String>,
    project: Option<&str>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    if string_path(value, &["payload", "type"]).as_deref() != Some("token_count") {
        return;
    }
    let Some(last_usage) = value
        .get("payload")
        .and_then(|payload| payload.get("info"))
        .and_then(|info| info.get("last_token_usage"))
    else {
        return;
    };
    let Some(usage) = parse_token_usage(last_usage) else {
        return;
    };
    events.usage_events.push(KeyedUsageEvent {
        key: None,
        event: UsageEvent {
            timestamp,
            session_id: session_id.cloned(),
            model: model.cloned().or_else(|| Some("codex".to_owned())),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            project: project.map(ToOwned::to_owned),
            usage,
        },
    });
}

fn collect_duration_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    let status = string_path(value, &["payload", "type"]);
    if status.as_deref() != Some("task_complete") {
        return;
    }
    let Some(duration_ms) = u64_path(value, &["payload", "duration_ms"]) else {
        return;
    };
    events.duration_events.push(DurationEvent {
        timestamp,
        session_id: session_id.cloned(),
        duration_ms,
        status,
    });
}

fn collect_tool_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    path: &Path,
    line_index: usize,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return;
    }
    let Some(tool_name) = string_path(value, &["payload", "name"]) else {
        return;
    };
    let key = string_path(value, &["payload", "call_id"]).unwrap_or_else(|| {
        format!(
            "{}:{}:{}",
            path.display(),
            line_index + 1,
            tool_name.as_str()
        )
    });
    events.tool_events.push(KeyedToolEvent {
        key: Some(key),
        event: ToolEvent {
            timestamp,
            session_id: session_id.cloned(),
            tool_name,
            subagent_type: None,
            source_kind: SourceKind::Main,
        },
    });
}

fn parse_token_usage(value: &Value) -> Option<TokenUsage> {
    let input_tokens = u64_field(value, "input_tokens");
    let cached_input_tokens = u64_field(value, "cached_input_tokens");
    let output_tokens = u64_field(value, "output_tokens");
    let reasoning_output_tokens = u64_field(value, "reasoning_output_tokens");
    let total_tokens = u64_field(value, "total_tokens");
    if input_tokens == 0 && output_tokens == 0 && total_tokens == 0 {
        return None;
    }

    // Codex reports input_tokens inclusive of cached_input_tokens; subtract so
    // input_tokens means fresh (uncached) input, matching the Claude schema.
    Some(TokenUsage {
        input_tokens: input_tokens.saturating_sub(cached_input_tokens),
        output_tokens,
        reasoning_output_tokens,
        cache_read_input_tokens: cached_input_tokens,
        ..TokenUsage::default()
    })
}

fn session_model(value: &Value) -> Option<String> {
    string_path(value, &["payload", "model"])
        .or_else(|| {
            string_path(
                value,
                &["payload", "collaboration_mode", "settings", "model"],
            )
        })
        .or_else(|| string_path(value, &["payload", "model_provider"]))
}

fn parse_timestamp(value: Option<&Value>) -> Option<OffsetDateTime> {
    let raw = value?.as_str()?;
    OffsetDateTime::parse(raw, &Rfc3339).ok()
}

fn string_path(value: &Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    cursor
        .as_str()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn u64_path(value: &Value, path: &[&str]) -> Option<u64> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    cursor.as_u64()
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

fn fallback_session_id(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn collects_codex_token_count_tools_and_duration() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:02Z","type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"exec_command","arguments":"{}"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"reasoning_output_tokens":3,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:04Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":12345}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"turn_aborted","duration_ms":99999}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false).expect("collection should succeed");

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].model.as_deref(), Some("gpt-5.5"));
        assert_eq!(collection.usage_events[0].usage.input_tokens, 60);
        assert_eq!(collection.usage_events[0].usage.cache_read_input_tokens, 40);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 110);
        assert_eq!(collection.tool_events[0].tool_name, "exec_command");
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 12_345);
    }
}
