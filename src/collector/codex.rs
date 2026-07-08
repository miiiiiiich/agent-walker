use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::collector::{
    FileEvents, KeyedEffortEvent, KeyedRateLimitSample, KeyedToolEvent, KeyedUsageEvent,
    list_files, merge_into, parse_files_cached, project_from_cwd,
};
use crate::model::{
    Collection, DurationEvent, EffortEvent, Provider, RateLimitSample, SessionTouch, SourceKind,
    TokenUsage, ToolEvent, UsageEvent,
};

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Codex, root.to_path_buf());

    // Codex *moves* (not copies) a session's JSONL from `sessions/` to the
    // sibling `archived_sessions/` when the desktop app archives it, so a
    // sessions-only scan silently drops archived history. Scan both. Resolve the
    // sibling from the canonical path so a relative root (e.g. `.`) still finds
    // `../archived_sessions`, falling back to the raw parent when the path can't
    // be canonicalized (root missing).
    let archived = root
        .canonicalize()
        .ok()
        .as_deref()
        .unwrap_or(root)
        .parent()
        .map(|parent| parent.join("archived_sessions"));

    // A session can briefly exist in both dirs (a stale `sessions/` copy left
    // after archiving). Dedupe by relative path before parsing — keeping the
    // larger, more-complete file — so duration events and session touches (which
    // merge_into does not key-dedupe) can't double-count.
    let file_len = |path: &Path| std::fs::metadata(path).map_or(0, |meta| meta.len());
    let mut chosen: HashMap<PathBuf, PathBuf> = HashMap::new();
    for dir in std::iter::once(root).chain(archived.as_deref()) {
        if !dir.exists() {
            continue;
        }
        for path in list_files(dir, "jsonl", mtime_floor, &mut collection.stats) {
            let rel = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
            match chosen.get_mut(&rel) {
                Some(existing) if file_len(&path) > file_len(existing) => *existing = path,
                Some(_) => {}
                None => {
                    chosen.insert(rel, path);
                }
            }
        }
    }
    if chosen.is_empty() {
        return collection;
    }
    let mut files: Vec<PathBuf> = chosen.into_values().collect();
    files.sort();

    let per_file = parse_files_cached(use_cache.then_some("codex"), &files, local_offset, |path| {
        parse_file(path, local_offset)
    });
    merge_into(&mut collection, per_file);
    collection
}

fn parse_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
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
            collect_effort_event(
                &value,
                timestamp,
                current_session_id.as_ref(),
                line_index,
                &mut events,
            );
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
            line_index,
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

    events.compress_touches(local_offset);
    Some(events)
}

#[allow(
    clippy::too_many_arguments,
    reason = "Per-line parse context; bundling into a struct adds noise for one caller."
)]
fn collect_usage_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    model: Option<&String>,
    project: Option<&str>,
    line_index: usize,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    if string_path(value, &["payload", "type"]).as_deref() != Some("token_count") {
        return;
    }
    collect_rate_limit_sample(value, timestamp, session_id, line_index, events);
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
    // `info.last_token_usage` is the delta for the most recent turn, NOT the
    // running `info.total_token_usage` cumulative. Summing one usage event per
    // token_count line therefore yields the session total — do not also add the
    // cumulative field, or every turn would be double-counted.
    // Dedup key for accidental session-file duplicates (e.g. a copied rollout):
    // (session, timestamp, line_index) is a stable per-event identifier. A
    // copied file reproduces all three, so the duplicate is merged away; two
    // distinct turns within one file differ in line_index (and usually
    // timestamp), so both survive even if their usage numbers happen to match.
    // Only keyed when both session_id and timestamp are present; otherwise None
    // (count every event).
    let key = match (session_id, timestamp) {
        (Some(sid), Some(ts)) => Some(format!(
            "codex:{sid}:{ts}:{line_index}",
            ts = ts.unix_timestamp_nanos(),
        )),
        _ => None,
    };
    events.usage_events.push(KeyedUsageEvent {
        key,
        event: UsageEvent {
            timestamp,
            session_id: session_id.cloned(),
            model: model.cloned().or_else(|| Some("codex".to_owned())),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            attribution_skill: None,
            project: project.map(ToOwned::to_owned),
            usage,
            reported_cost_usd: None,
        },
    });
}

/// Rate-limit snapshot riding on a `token_count` event: the plan's primary
/// (5h) window utilization at that moment. Only the primary window is kept —
/// the weekly window was deliberately dropped from the LIMITS history (a
/// 30-day view of a 7-day window nests confusingly). Keyed like usage events
/// so a copied rollout file can't double-sample a day.
fn collect_rate_limit_sample(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    line_index: usize,
    events: &mut FileEvents,
) {
    let Some(timestamp) = timestamp else {
        return;
    };
    let Some(used_percent) = value
        .get("payload")
        .and_then(|payload| payload.get("rate_limits"))
        .and_then(|limits| limits.get("primary"))
        .and_then(|primary| primary.get("used_percent"))
        .and_then(Value::as_f64)
    else {
        return;
    };
    let key = session_id.map(|sid| {
        format!(
            "codex-limit:{sid}:{ts}:{line_index}",
            ts = timestamp.unix_timestamp_nanos(),
        )
    });
    events.rate_limit_samples.push(KeyedRateLimitSample {
        key,
        event: RateLimitSample {
            timestamp,
            used_percent: used_percent.clamp(0.0, 100.0),
        },
    });
}

/// Reasoning-effort setting for one turn (`turn_context.payload.effort`).
fn collect_effort_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    line_index: usize,
    events: &mut FileEvents,
) {
    let Some(effort) = string_path(value, &["payload", "effort"]) else {
        return;
    };
    let key = match (session_id, timestamp) {
        (Some(sid), Some(ts)) => Some(format!(
            "codex-effort:{sid}:{ts}:{line_index}",
            ts = ts.unix_timestamp_nanos(),
        )),
        _ => None,
    };
    events.effort_events.push(KeyedEffortEvent {
        key,
        event: EffortEvent { timestamp, effort },
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
    let Some(raw_name) = string_path(value, &["payload", "name"]) else {
        return;
    };
    // Codex runs most reads and writes through a generic shell wrapper
    // (`exec_command` etc., usually `bash -lc "..."`); resolving the wrapper to
    // the real command basename lets the tool list show `grep`/`cargo` instead of
    // one undifferentiated "exec" bucket.
    let tool_name = if is_shell_wrapper(&raw_name) {
        exec_command_basename(value).unwrap_or(raw_name)
    } else {
        raw_name
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

/// Codex tool names that wrap an arbitrary shell command rather than naming a
/// concrete operation. These are the ones worth decomposing.
fn is_shell_wrapper(name: &str) -> bool {
    matches!(
        name,
        "exec_command" | "shell" | "local_shell" | "unified_exec"
    )
}

/// Resolve a shell-wrapper tool call to the basename of the command it actually
/// ran. Reads `payload.arguments` (a JSON string), pulls `command` (or `cmd` as
/// a fallback; array or string), unwraps a `bash -c "<script>"` shape to the
/// script's first token, skips run-prefixes (`env`/`sudo`/…) and variable
/// assignments, and strips the path. Returns `None` when anything is
/// unrecognized, so the caller keeps the original wrapper name as a fallback.
fn exec_command_basename(value: &Value) -> Option<String> {
    let arguments = string_path(value, &["payload", "arguments"])?;
    let parsed = serde_json::from_str::<Value>(&arguments).ok()?;
    let command = parsed.get("command").or_else(|| parsed.get("cmd"))?;
    let tokens = command_tokens(command)?;
    let effective = effective_command(&tokens)?;
    basename(&effective)
}

/// Normalize `command` into a token vector: a JSON array of strings, or a plain
/// string split on whitespace.
fn command_tokens(command: &Value) -> Option<Vec<String>> {
    match command {
        Value::Array(items) => {
            let tokens: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect();
            (!tokens.is_empty()).then_some(tokens)
        }
        Value::String(text) => {
            let tokens: Vec<String> = text.split_whitespace().map(ToOwned::to_owned).collect();
            (!tokens.is_empty()).then_some(tokens)
        }
        _ => None,
    }
}

/// True for a short-option cluster that ends with `c` semantics — i.e. starts
/// with a single `-`, is not a `--long` flag, and contains `c` (`-c`, `-lc`,
/// `-lic`, `-euc`). Excludes `--norc` and friends, which merely contain `c`.
fn is_shell_command_flag(token: &str) -> bool {
    token.starts_with('-') && !token.starts_with("--") && token.contains('c')
}

/// Run-prefixes that wrap the real command and should be skipped when looking
/// for the effective command (`sudo cargo build` -> cargo).
const RUN_PREFIXES: [&str; 4] = ["env", "sudo", "time", "nice"];

/// A leading variable assignment (`FOO=bar`): an identifier, `=`, then a value.
fn is_var_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Pick the token that names the real command. For a `bash -c "<script>"`
/// wrapper, that is the first token of the script; otherwise it is the first
/// real token after any run-prefix (`env`/`sudo`/`time`/`nice`) and leading
/// variable assignments.
fn effective_command(tokens: &[String]) -> Option<String> {
    let first = tokens.first()?;
    let is_shell = matches!(
        basename(first).as_deref(),
        Some("bash" | "sh" | "zsh" | "dash")
    );
    if is_shell
        && let Some(flag_index) = tokens.iter().skip(1).position(|t| is_shell_command_flag(t))
    {
        // `position` is relative to the skipped slice; +1 realigns to `tokens`,
        // and the script string is the token right after the flag.
        let script = tokens.get(flag_index + 2)?;
        return script
            .split_whitespace()
            .next()
            .map(trim_quotes)
            .map(ToOwned::to_owned);
    }

    // Not a shell wrapper: skip run-prefixes and `FOO=bar` assignments to reach
    // the real command (`sudo cargo build` -> cargo, `env FOO=1 grep` -> grep).
    let effective = tokens.iter().find(|token| {
        let bare = trim_quotes(token);
        !is_var_assignment(bare) && !RUN_PREFIXES.contains(&basename(bare).as_deref().unwrap_or(""))
    })?;
    Some(trim_quotes(effective).to_owned())
}

/// Strip a single matching pair of surrounding quotes (`"grep"` / `'grep'`).
fn trim_quotes(token: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = token
            .strip_prefix(quote)
            .and_then(|t| t.strip_suffix(quote))
        {
            return inner;
        }
    }
    token
}

/// Basename of a command token (`/usr/bin/grep` -> `grep`). Gives up on shapes
/// that aren't a plain command word — a leading `(` subshell or a `FOO=bar`
/// assignment — so the caller falls back to the wrapper name.
fn basename(command: &str) -> Option<String> {
    let command = trim_quotes(command);
    if command.is_empty() || command.starts_with('(') || command.contains('=') {
        return None;
    }
    let base = command.rsplit('/').next()?;
    (!base.is_empty()).then(|| base.to_owned())
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

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

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

    #[test]
    fn collects_effort_and_rate_limit_samples() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"xhigh"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110}},"rate_limits":{"primary":{"used_percent":37.5,"window_minutes":300},"secondary":{"used_percent":12.0,"window_minutes":10080}}}}"#,
                "\n",
                // A turn_context without effort contributes no effort event.
                r#"{"timestamp":"2026-06-01T00:00:04Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.effort_events.len(), 1);
        assert_eq!(collection.effort_events[0].effort, "xhigh");
        // Only the PRIMARY (5h) window is sampled; the weekly window is
        // deliberately dropped from the LIMITS history.
        assert_eq!(collection.rate_limit_samples.len(), 1);
        assert!((collection.rate_limit_samples[0].used_percent - 37.5).abs() < f64::EPSILON);
    }

    /// Each `token_count` line carries a (session, timestamp, `line_index`)
    /// dedup key. A copied session file reproduces all three, so the duplicate
    /// file is merged away — but two distinct turns inside one file differ in
    /// `line_index` and both survive, even though their usage numbers are
    /// identical here. So the two-turn file copied twice yields 2 events (not 4,
    /// and not 1).
    #[test]
    fn deduplicates_copies_but_keeps_distinct_turns() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        // Two turns with identical usage but distinct timestamps and lines.
        let lines = concat!(
            r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:09Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
            "\n"
        );
        fs::write(day.join("rollout-original.jsonl"), lines).expect("fixture should be written");
        fs::write(day.join("rollout-copy.jsonl"), lines).expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 2);
        // 2 distinct turns survive; the copied file is deduplicated away.
        assert_eq!(collection.usage_events.len(), 2);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 220);
    }

    /// The Codex desktop app *moves* a session's JSONL from `sessions/` to the
    /// sibling `archived_sessions/`, so the collector must scan both.
    #[test]
    fn scans_sibling_archived_sessions() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let sessions_day = temp.path().join("sessions/2026/06/01");
        let archived_day = temp.path().join("archived_sessions/2026/06/01");
        fs::create_dir_all(&sessions_day).expect("test dirs should be created");
        fs::create_dir_all(&archived_day).expect("test dirs should be created");
        fs::write(
            sessions_day.join("rollout-active.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");
        fs::write(
            archived_day.join("rollout-archived.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s2","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200,"cached_input_tokens":0,"output_tokens":20,"total_tokens":220}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(&temp.path().join("sessions"), None, false, UtcOffset::UTC);

        // Both the active and the archived session are counted.
        assert_eq!(collection.stats.files_seen, 2);
        assert_eq!(collection.usage_events.len(), 2);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 330); // 110 (active) + 220 (archived)
    }

    /// A session present in *both* dirs (a stale `sessions/` copy left after an
    /// archive) must not double-count — the keyed events dedupe it to one turn.
    #[test]
    fn dedups_session_present_in_sessions_and_archive() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let sessions_day = temp.path().join("sessions/2026/06/01");
        let archived_day = temp.path().join("archived_sessions/2026/06/01");
        fs::create_dir_all(&sessions_day).expect("test dirs should be created");
        fs::create_dir_all(&archived_day).expect("test dirs should be created");
        let lines = concat!(
            r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
            "\n"
        );
        fs::write(sessions_day.join("rollout-s1.jsonl"), lines).expect("fixture should be written");
        fs::write(archived_day.join("rollout-s1.jsonl"), lines).expect("fixture should be written");

        let collection = collect(&temp.path().join("sessions"), None, false, UtcOffset::UTC);

        // The stale duplicate is filtered by relative path before parsing, so
        // only one file is read. Its two timestamped lines yield two session
        // touches — not four — proving the duplicate didn't double-count (the
        // bug this guards: session_touches / duration_events aren't key-deduped).
        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 110);
        assert_eq!(collection.session_touches.len(), 2);
    }

    /// Shell-wrapper tool calls (`exec_command` etc.) are decomposed to the real
    /// command basename so the tool list reflects the real command;
    /// unrecognized arguments fall back to the wrapper name unchanged.
    #[test]
    fn decomposes_exec_command_to_real_command_basename() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        let exec = |call: &str, args: &str| {
            format!(
                r#"{{"timestamp":"2026-06-01T00:00:00Z","type":"response_item","payload":{{"type":"function_call","call_id":"{call}","name":"exec_command","arguments":{args}}}}}"#,
            )
        };
        // `arguments` is a JSON *string*, so the inner JSON is serde-encoded.
        let arg = |inner: &str| serde_json::Value::String(inner.to_owned()).to_string();
        let mut body = String::from(
            r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
        );
        body.push('\n');
        for (call, inner) in [
            ("c1", r#"{"command":["bash","-lc","grep -rn foo src"]}"#),
            ("c2", r#"{"command":["cargo","build"]}"#),
            ("c3", r"not json"),
            ("c4", r#"{"command":["/usr/bin/cat","README.md"]}"#),
            ("c5", r#"{"command":["bash","-c","grep x"]}"#),
            ("c6", r#"{"command":["sudo","cargo","build"]}"#),
            ("c7", r#"{"command":["env","FOO=1","grep","x"]}"#),
            ("c8", r#"{"command":["bash","--norc","-lc","cat y"]}"#),
            ("c9", r#"{"cmd":"ls -la"}"#),
        ] {
            body.push_str(&exec(call, &arg(inner)));
            body.push('\n');
        }
        fs::write(day.join("rollout-session.jsonl"), body).expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let names: Vec<&str> = collection
            .tool_events
            .iter()
            .map(|event| event.tool_name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "grep",         // bash -lc "grep -rn foo src"
                "cargo",        // cargo build
                "exec_command", // non-JSON args -> wrapper fallback
                "cat",          // /usr/bin/cat -> basename
                "grep",         // bash -c "grep x"
                "cargo",        // sudo cargo build -> skip the sudo prefix
                "grep",         // env FOO=1 grep x -> skip env + assignment
                "cat",          // bash --norc -lc "cat y" -> --norc is not the -c flag
                "ls",           // {"cmd":"ls -la"} -> cmd fallback field
            ]
        );
    }
}
