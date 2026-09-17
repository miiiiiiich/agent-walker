use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::collector::{
    FileEvents, KeyedDurationEvent, KeyedEffortEvent, KeyedInterruptEvent, KeyedPaceEvent,
    KeyedPermissionEvent, KeyedRateLimitSample, KeyedToolEvent, KeyedUsageEvent, list_files,
    merge_into, parse_files_cached, project_from_cwd,
};
use crate::model::{
    Collection, DurationEvent, EffortEvent, InterruptEvent, PaceEvent, PermissionEvent, Provider,
    RateLimitSample, SessionTouch, SourceKind, TokenUsage, ToolEvent, UsageEvent,
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
    // larger, more-complete file — so Codex durations (emitted without keys)
    // and session touches are not duplicated during merging.
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

#[allow(
    clippy::too_many_lines,
    reason = "One pass over the rollout feeding every collector; splitting adds indirection without logic."
)]
fn parse_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    let file = File::open(path).ok()?;
    let mut events = FileEvents::default();
    let mut current_session_id = fallback_session_id(path);
    let mut pace_state = PaceState::default();
    let mut turn_items = TurnItems::default();
    let mut current_model = None;
    let mut current_project = None;
    let mut session_meta_count = 0usize;
    let mut replay_second: Option<i64> = None;
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
            session_meta_count += 1;
            if session_meta_count == 2 {
                replay_second = timestamp.map(OffsetDateTime::unix_timestamp);
            }
            current_session_id = string_path(&value, &["payload", "id"]).or(current_session_id);
            current_model = session_model(&value).or(current_model);
            current_project =
                string_path(&value, &["payload", "cwd"]).map(|cwd| project_from_cwd(&cwd));
        }
        // Fork/spawn copies rewrite timestamps to the fork instant and shift
        // line positions (GH-36). Skip the replay burst even without a parent
        // in the scan; content keys deduplicate remaining replay when present.
        let in_replay_burst = session_meta_count >= 2
            && match (replay_second, timestamp) {
                (Some(second), Some(ts)) => ts.unix_timestamp() == second,
                _ => false,
            };
        if value.get("type").and_then(Value::as_str) == Some("turn_context") {
            current_model = string_path(&value, &["payload", "model"]).or(current_model);
            if !in_replay_burst {
                collect_effort_event(
                    &value,
                    timestamp,
                    current_session_id.as_ref(),
                    line_index,
                    &mut events,
                );
                collect_permission_event(
                    &value,
                    timestamp,
                    current_session_id.as_ref(),
                    line_index,
                    &mut events,
                );
            }
        }
        if in_replay_burst {
            continue;
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
        turn_items.note(&value, timestamp);
        collect_duration_event(
            &value,
            timestamp,
            current_session_id.as_ref(),
            &mut turn_items,
            &mut events,
        );
        collect_interrupt_event(&value, timestamp, current_session_id.as_ref(), &mut events);
        collect_pace_event(
            &value,
            timestamp,
            current_session_id.as_ref(),
            &mut pace_state,
            &mut events,
        );
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
    // Emit last_token_usage deltas; do not also add cumulative total_token_usage.
    // Key by session, last vector, and cumulative vector to collapse replay
    // and unchanged re-emissions (GH-36). Without cumulative usage, positional
    // keys deduplicate whole-file copies only.
    let key = match (session_id, total_usage(value)) {
        (Some(sid), Some(cum)) => Some(format!(
            "codex-usage:v2:{sid}:{last}:{cum}",
            last = usage_fingerprint(last_usage),
            cum = usage_fingerprint(cum),
        )),
        (Some(sid), None) => positional_key("codex", sid, timestamp, line_index),
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

/// Key by co-riding usage state and snapshot fields so fork replay collapses
/// (GH-36), while a moved window survives.
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
    let Some(primary) = value
        .get("payload")
        .and_then(|payload| payload.get("rate_limits"))
        .and_then(|limits| limits.get("primary"))
    else {
        return;
    };
    let Some(used_percent) = primary.get("used_percent").and_then(Value::as_f64) else {
        return;
    };
    let last = value
        .get("payload")
        .and_then(|payload| payload.get("info"))
        .and_then(|info| info.get("last_token_usage"))
        .and_then(fingerprintable);
    let key = match (session_id, total_usage(value), last) {
        (Some(sid), Some(cum), Some(last)) => Some(format!(
            "codex-limit:v2:{sid}:{last_fp}:{cum_fp}:{percent}:{window}:{resets}",
            last_fp = usage_fingerprint(last),
            cum_fp = usage_fingerprint(cum),
            percent = used_percent.clamp(0.0, 100.0).to_bits(),
            window = u64_field(primary, "window_minutes"),
            resets = u64_field(primary, "resets_at"),
        )),
        (Some(sid), _, _) => positional_key("codex-limit", sid, Some(timestamp), line_index),
        _ => None,
    };
    events.rate_limit_samples.push(KeyedRateLimitSample {
        key,
        event: RateLimitSample {
            timestamp,
            used_percent: used_percent.clamp(0.0, 100.0),
        },
    });
}

/// Key by `turn_id`, which fork replays copy verbatim — the replayed
/// `turn_context` collapses with its original despite the rewritten timestamp
/// (GH-36). Logs predating `turn_id` fall back to the positional key.
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
    let key = match (session_id, string_path(value, &["payload", "turn_id"])) {
        (Some(sid), Some(turn_id)) => Some(format!("codex-effort:v2:{sid}:{turn_id}")),
        (Some(sid), None) => positional_key("codex-effort", sid, timestamp, line_index),
        _ => None,
    };
    events.effort_events.push(KeyedEffortEvent {
        key,
        event: EffortEvent { timestamp, effort },
    });
}

/// Count `turn_aborted` only with reason `interrupted`, a session id, and a
/// turn id. A positional fallback is not fork-stable and would count copies.
fn collect_interrupt_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    if string_path(value, &["payload", "type"]).as_deref() != Some("turn_aborted") {
        return;
    }
    if string_path(value, &["payload", "reason"]).as_deref() != Some("interrupted") {
        return;
    }
    let (Some(sid), Some(turn_id)) = (session_id, string_path(value, &["payload", "turn_id"]))
    else {
        return;
    };
    events.interrupt_events.push(KeyedInterruptEvent {
        key: Some(format!("codex-interrupt:v2:{sid}:{turn_id}")),
        event: InterruptEvent { timestamp },
    });
}

fn collect_permission_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    line_index: usize,
    events: &mut FileEvents,
) {
    let Some(mode) = string_path(value, &["payload", "approval_policy"]) else {
        return;
    };
    let key = match (session_id, string_path(value, &["payload", "turn_id"])) {
        (Some(sid), Some(turn_id)) => Some(format!("codex-permission:v2:{sid}:{turn_id}")),
        (Some(sid), None) => positional_key("codex-permission", sid, timestamp, line_index),
        _ => None,
    };
    events.permission_events.push(KeyedPermissionEvent {
        key,
        event: PermissionEvent { timestamp, mode },
    });
}

/// Pace bookkeeping per file: the completion the next prompt will pair
/// with, and the newest completion time ever seen — replayed rows (fork
/// bursts that leak, resumes) are older than that high-water mark and
/// must neither re-arm a consumed completion nor rewind a pending one.
#[derive(Default)]
struct PaceState {
    pending_completion: Option<OffsetDateTime>,
    newest_completion: Option<OffsetDateTime>,
}

/// The human's pace: `task_complete` → the next `user_message`, when under
/// the 30-minute cutoff (longer is the human being away). Keyed by session
/// and prompt time so a replayed prompt row doesn't count twice.
fn collect_pace_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    state: &mut PaceState,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    match string_path(value, &["payload", "type"]).as_deref() {
        Some("task_complete") => {
            if let Some(at) = timestamp
                && state.newest_completion.is_none_or(|newest| at > newest)
            {
                state.newest_completion = Some(at);
                state.pending_completion = Some(at);
            }
        }
        Some("user_message") => {
            let Some(prompt_at) = timestamp else {
                return;
            };
            // A replayed (older) prompt row neither records a gap nor
            // consumes the completion the genuine next prompt will pair with.
            if let Some(end) = state.pending_completion
                && prompt_at > end
                && prompt_at - end <= Duration::minutes(30)
            {
                state.pending_completion = None;
                let gap_ms = u64::try_from((prompt_at - end).whole_milliseconds()).unwrap_or(0);
                events.pace_events.push(KeyedPaceEvent {
                    key: session_id.map(|session| {
                        format!("codex-pace:{session}:{}", prompt_at.unix_timestamp_nanos())
                    }),
                    event: PaceEvent {
                        timestamp: Some(prompt_at),
                        gap_ms,
                    },
                });
            }
        }
        _ => {}
    }
}

/// Items dedupe by id — file-
/// wide, so a replay of an earlier turn's item never lands in a later turn
/// — and tool spans are unioned (parallel tools), so the turn minus the
/// tool time is the model's own time. The per-turn part resets only when
/// a turn ends (`task_complete` / `turn_aborted`) or a new one starts
/// (`task_started`) — a steering `user_message` mid-turn must not drop
/// what ran before it.
#[derive(Default)]
struct TurnItems {
    seen: HashSet<String>,
    tool_spans: Vec<(u64, u64)>,
    timed: bool,
    /// Newest lifecycle row (`task_started` / `turn_aborted` /
    /// `task_complete`) seen: a replayed older one must not reset the turn.
    newest_lifecycle: Option<OffsetDateTime>,
}

impl TurnItems {
    fn end_turn(&mut self) {
        self.tool_spans.clear();
        self.timed = false;
    }

    fn lifecycle_is_current(&mut self, timestamp: Option<OffsetDateTime>) -> bool {
        let Some(at) = timestamp else {
            return true;
        };
        if self.newest_lifecycle.is_some_and(|newest| at <= newest) {
            return false;
        }
        self.newest_lifecycle = Some(at);
        true
    }

    fn note(&mut self, value: &Value, timestamp: Option<OffsetDateTime>) {
        if value.get("type").and_then(Value::as_str) != Some("event_msg") {
            return;
        }
        match string_path(value, &["payload", "type"]).as_deref() {
            Some("task_started" | "turn_aborted") if self.lifecycle_is_current(timestamp) => {
                self.end_turn();
            }
            Some("item_completed") => {
                // An item row older than the newest lifecycle row is a
                // replay leaking into the current turn (its own lifecycle
                // rows were rejected; a skipped fork burst never taught
                // `seen` its ids) — it belongs to no turn we are building.
                if timestamp
                    .zip(self.newest_lifecycle)
                    .is_some_and(|(at, newest)| at < newest)
                {
                    return;
                }
                let (Some(started), Some(completed)) = (
                    u64_path(value, &["payload", "started_at_ms"]),
                    u64_path(value, &["payload", "completed_at_ms"]),
                ) else {
                    return;
                };
                let fresh = string_path(value, &["payload", "item", "id"])
                    .is_none_or(|id| self.seen.insert(id));
                if !fresh {
                    return;
                }
                self.timed = true;
                // Allowlist the model's own items; every other timed item type,
                // including ones a newer CLI adds, counts as a tool.
                let is_model = matches!(
                    string_path(value, &["payload", "item", "type"]).as_deref(),
                    Some("Reasoning" | "AgentMessage" | "ContextCompaction" | "UserMessage")
                );
                if !is_model && completed > started {
                    self.tool_spans.push((started, completed));
                }
            }
            _ => {}
        }
    }

    /// Union length of the tool spans — parallel tools count once.
    fn tool_ms(&self) -> u64 {
        let mut spans = self.tool_spans.clone();
        spans.sort_unstable();
        let mut total = 0_u64;
        let mut current: Option<(u64, u64)> = None;
        for (start, end) in spans {
            match current {
                Some((cur_start, cur_end)) if start <= cur_end => {
                    current = Some((cur_start, cur_end.max(end)));
                }
                Some((cur_start, cur_end)) => {
                    total = total.saturating_add(cur_end - cur_start);
                    current = Some((start, end));
                }
                None => current = Some((start, end)),
            }
        }
        if let Some((cur_start, cur_end)) = current {
            total = total.saturating_add(cur_end - cur_start);
        }
        total
    }

    /// The model's share of a `duration_ms` turn; `None` when no item
    /// carried timing (older CLI), rather than pretending zero tool time.
    fn model_ms(&self, duration_ms: u64) -> Option<u64> {
        self.timed
            .then(|| duration_ms.saturating_sub(self.tool_ms()))
    }
}

fn collect_duration_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    turn_items: &mut TurnItems,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    let status = string_path(value, &["payload", "type"]);
    if status.as_deref() != Some("task_complete") {
        return;
    }
    // A replayed older completion must not close the current turn.
    if !turn_items.lifecycle_is_current(timestamp) {
        return;
    }
    // The turn is over either way: a completion without a duration must
    // still close its items, or the next turn inherits them.
    let model_ms = u64_path(value, &["payload", "duration_ms"]).map(|d| turn_items.model_ms(d));
    turn_items.end_turn();
    let Some(duration_ms) = u64_path(value, &["payload", "duration_ms"]) else {
        return;
    };
    events.duration_events.push(KeyedDurationEvent {
        key: None,
        event: DurationEvent {
            timestamp,
            session_id: session_id.cloned(),
            duration_ms,
            human_wait_ms: 0,
            model_ms: model_ms.flatten(),
            status,
        },
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

fn is_shell_wrapper(name: &str) -> bool {
    matches!(
        name,
        "exec_command" | "shell" | "local_shell" | "unified_exec"
    )
}

fn exec_command_basename(value: &Value) -> Option<String> {
    let arguments = string_path(value, &["payload", "arguments"])?;
    let parsed = serde_json::from_str::<Value>(&arguments).ok()?;
    let command = parsed.get("command").or_else(|| parsed.get("cmd"))?;
    let tokens = command_tokens(command)?;
    let effective = effective_command(&tokens)?;
    basename(&effective)
}

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

fn is_shell_command_flag(token: &str) -> bool {
    token.starts_with('-') && !token.starts_with("--") && token.contains('c')
}

const RUN_PREFIXES: [&str; 4] = ["env", "sudo", "time", "nice"];

fn is_var_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

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

    let effective = tokens.iter().find(|token| {
        let bare = trim_quotes(token);
        !is_var_assignment(bare) && !RUN_PREFIXES.contains(&basename(bare).as_deref().unwrap_or(""))
    })?;
    Some(trim_quotes(effective).to_owned())
}

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

fn basename(command: &str) -> Option<String> {
    let command = trim_quotes(command);
    if command.is_empty() || command.starts_with('(') || command.contains('=') {
        return None;
    }
    let base = command.rsplit('/').next()?;
    (!base.is_empty()).then(|| base.to_owned())
}

fn total_usage(value: &Value) -> Option<&Value> {
    value
        .get("payload")
        .and_then(|payload| payload.get("info"))
        .and_then(|info| info.get("total_token_usage"))
        .and_then(fingerprintable)
}

/// A usage object valid enough to key on: `total_tokens` parses as an
/// integer. Malformed payloads (`null`, `{}`, a string) would fingerprint as
/// all-zero and falsely collide distinct events — they must take the
/// positional fallback instead.
fn fingerprintable(usage: &Value) -> Option<&Value> {
    usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .is_some()
        .then_some(usage)
}

fn usage_fingerprint(value: &Value) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        u64_field(value, "input_tokens"),
        u64_field(value, "cached_input_tokens"),
        u64_field(value, "cache_write_input_tokens"),
        u64_field(value, "output_tokens"),
        u64_field(value, "reasoning_output_tokens"),
        u64_field(value, "total_tokens"),
    )
}

/// Positional fallback key for events that predate semantic identifiers.
/// Stable for whole-file copies, but NOT for fork replays — those rewrite
/// timestamps and shift line positions, which is why keyed paths prefer
/// content-based keys and reach for this only when the content key can't be
/// built.
fn positional_key(
    prefix: &str,
    session_id: &str,
    timestamp: Option<OffsetDateTime>,
    line_index: usize,
) -> Option<String> {
    timestamp.map(|ts| {
        format!(
            "{prefix}:{session_id}:{ts}:{line_index}",
            ts = ts.unix_timestamp_nanos(),
        )
    })
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
    fn model_time_is_the_turn_minus_the_union_of_tool_items() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let cmd = r#"{"timestamp":"2026-06-01T00:00:15Z","type":"event_msg","payload":{"type":"item_completed","started_at_ms":5000,"completed_at_ms":15000,"item":{"id":"i2","type":"CommandExecution"}}}"#;
        fs::write(
            temp.path().join("rollout.jsonl"),
            [
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","cwd":"/tmp/p","cli_version":"0.149.0"}}"#,
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"event_msg","payload":{"type":"task_started"}}"#,
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"item_completed","started_at_ms":1000,"completed_at_ms":4000,"item":{"id":"i1","type":"Reasoning"}}}"#,
                cmd,
                cmd,
                r#"{"timestamp":"2026-06-01T00:00:16Z","type":"event_msg","payload":{"type":"user_message","message":"steer"}}"#,
                r#"{"timestamp":"2026-06-01T00:00:18Z","type":"event_msg","payload":{"type":"item_completed","started_at_ms":10000,"completed_at_ms":18000,"item":{"id":"i3","type":"McpToolCall"}}}"#,
                r#"{"timestamp":"2026-06-01T00:00:20Z","type":"event_msg","payload":{"type":"item_completed","started_at_ms":18000,"completed_at_ms":19000,"item":{"id":"i4","type":"AgentMessage"}}}"#,
                r#"{"timestamp":"2026-06-01T00:00:21Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":20000}}"#,
                r#"{"timestamp":"2026-06-01T00:01:00Z","type":"event_msg","payload":{"type":"task_started"}}"#,
                r#"{"timestamp":"2026-06-01T00:01:05Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":5000}}"#,
                r#"{"timestamp":"2026-06-01T00:02:00Z","type":"event_msg","payload":{"type":"item_completed","started_at_ms":120000,"completed_at_ms":123000,"item":{"id":"i5","type":"CommandExecution"}}}"#,
                r#"{"timestamp":"2026-06-01T00:02:04Z","type":"event_msg","payload":{"type":"task_complete"}}"#,
                cmd,
                r#"{"timestamp":"2026-06-01T00:03:00Z","type":"event_msg","payload":{"type":"item_completed","started_at_ms":170000,"completed_at_ms":172000,"item":{"id":"i6","type":"Reasoning"}}}"#,
                r#"{"timestamp":"2026-06-01T00:03:02Z","type":"event_msg","payload":{"type":"item_completed","started_at_ms":172000,"completed_at_ms":174000,"item":{"id":"i7","type":"WebSearch"}}}"#,
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"event_msg","payload":{"type":"task_started"}}"#,
                r#"{"timestamp":"2026-06-01T00:00:09Z","type":"event_msg","payload":{"type":"item_completed","started_at_ms":6000,"completed_at_ms":9000,"item":{"id":"leak","type":"CommandExecution"}}}"#,
                r#"{"timestamp":"2026-06-01T00:03:05Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":9000}}"#,
                r#"{"timestamp":"2026-06-01T00:04:00Z","type":"event_msg","payload":{"type":"task_started"}}"#,
                cmd,
                r#"{"timestamp":"2026-06-01T00:04:05Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":3000}}"#,
                "",
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 4);
        assert_eq!(collection.duration_events[0].model_ms, Some(7_000));
        assert_eq!(collection.duration_events[1].model_ms, None);
        assert_eq!(collection.duration_events[2].model_ms, Some(7_000));
        assert_eq!(collection.duration_events[3].model_ms, None);
    }

    #[test]
    fn pace_is_task_complete_to_next_user_message() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("rollout.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","cwd":"/tmp/p","cli_version":"0.149.0"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:04Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":4000}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:30Z","type":"event_msg","payload":{"type":"user_message","message":"next"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:01:00Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":30000}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T01:30:00Z","type":"event_msg","payload":{"type":"user_message","message":"back"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let gaps: Vec<u64> = collection.pace_events.iter().map(|e| e.gap_ms).collect();
        assert_eq!(gaps, vec![26_000]);
    }

    /// Replayed rows are older than the state they meet: a replayed prompt
    /// must not consume the latest completion, and a replayed completion
    /// must not rewind it — the genuine next prompt still pairs correctly.
    #[test]
    fn replayed_codex_rows_do_not_disturb_pace_state() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("rollout.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","cwd":"/tmp/p","cli_version":"0.149.0"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:01:00Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":1000}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:02:00Z","type":"event_msg","payload":{"type":"user_message","message":"a"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:03:00Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":1000}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:01:00Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":1000}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:02:00Z","type":"event_msg","payload":{"type":"user_message","message":"a"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:04:00Z","type":"event_msg","payload":{"type":"user_message","message":"b"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:03:00Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":1000}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:05:00Z","type":"event_msg","payload":{"type":"user_message","message":"mid-turn"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let gaps: Vec<u64> = collection.pace_events.iter().map(|e| e.gap_ms).collect();
        assert_eq!(gaps, vec![60_000, 60_000]);
    }

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
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"xhigh","approval_policy":"never"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110}},"rate_limits":{"primary":{"used_percent":37.5,"window_minutes":300},"secondary":{"used_percent":12.0,"window_minutes":10080}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:04Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","turn_id":"t7","duration_ms":57000}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:06Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","turn_id":"t7","duration_ms":57000}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:07Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"replaced","turn_id":"t8","duration_ms":100}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:08Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","duration_ms":200}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.effort_events.len(), 1);
        assert_eq!(collection.effort_events[0].effort, "xhigh");
        assert_eq!(collection.permission_events.len(), 1);
        assert_eq!(collection.permission_events[0].mode, "never");
        assert_eq!(collection.interrupt_events.len(), 1);
        assert_eq!(collection.rate_limit_samples.len(), 1);
        assert!((collection.rate_limit_samples[0].used_percent - 37.5).abs() < f64::EPSILON);
    }

    #[test]
    fn deduplicates_copies_but_keeps_distinct_turns() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
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
        assert_eq!(collection.usage_events.len(), 2);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 220);
    }

    fn fork_parent_lines() -> &'static str {
        concat!(
            r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"p1","model_provider":"openai"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","approval_policy":"on-request","turn_id":"t1"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}},"rate_limits":{"primary":{"used_percent":10.0,"window_minutes":300,"resets_at":1750000000}}}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:09Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":120,"cached_input_tokens":0,"output_tokens":20,"total_tokens":140},"total_token_usage":{"input_tokens":220,"cached_input_tokens":40,"output_tokens":30,"total_tokens":250}},"rate_limits":{"primary":{"used_percent":12.5,"window_minutes":300,"resets_at":1750000000}}}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:10Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":111}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:11Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","turn_id":"t2","duration_ms":5000}}"#,
            "\n"
        )
    }

    fn fork_child_lines() -> &'static str {
        concat!(
            r#"{"timestamp":"2026-06-01T01:00:00Z","type":"session_meta","payload":{"id":"c1","forked_from_id":"p1","model_provider":"openai"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T01:00:00Z","type":"session_meta","payload":{"id":"p1","model_provider":"openai"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T01:00:00Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","approval_policy":"on-request","turn_id":"t1"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T01:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}},"rate_limits":{"primary":{"used_percent":10.0,"window_minutes":300,"resets_at":1750000000}}}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T01:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":120,"cached_input_tokens":0,"output_tokens":20,"total_tokens":140},"total_token_usage":{"input_tokens":220,"cached_input_tokens":40,"output_tokens":30,"total_tokens":250}},"rate_limits":{"primary":{"used_percent":12.5,"window_minutes":300,"resets_at":1750000000}}}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T01:00:00Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":111}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T01:00:00Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","turn_id":"t2","duration_ms":5000}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T01:00:05Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"xhigh","approval_policy":"never","turn_id":"t9"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T01:00:07Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":90,"cached_input_tokens":10,"output_tokens":40,"total_tokens":130},"total_token_usage":{"input_tokens":310,"cached_input_tokens":50,"output_tokens":70,"total_tokens":380}},"rate_limits":{"primary":{"used_percent":15.0,"window_minutes":300,"resets_at":1750018000}}}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T01:00:08Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":222}}"#,
            "\n"
        )
    }

    fn assert_fork_replay_counted_once(collection: &Collection) {
        assert_eq!(collection.usage_events.len(), 3);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 380); // 110 + 140 + 130
        assert_eq!(collection.rate_limit_samples.len(), 3);
        assert_eq!(collection.effort_events.len(), 2);
        assert_eq!(collection.duration_events.len(), 2);
        assert_eq!(collection.interrupt_events.len(), 1);

        let original = OffsetDateTime::parse("2026-06-01T00:00:03Z", &Rfc3339)
            .expect("test timestamp should parse");
        let first_turn = collection
            .usage_events
            .iter()
            .find(|event| event.usage.token_volume() == 110)
            .expect("first parent turn should survive");
        assert_eq!(first_turn.timestamp, Some(original));
        let first_sample = collection
            .rate_limit_samples
            .iter()
            .find(|sample| (sample.used_percent - 10.0).abs() < f64::EPSILON)
            .expect("first rate-limit sample should survive");
        assert_eq!(first_sample.timestamp, original);
        let original_turn_context = OffsetDateTime::parse("2026-06-01T00:00:01Z", &Rfc3339)
            .expect("test timestamp should parse");
        let first_effort = collection
            .effort_events
            .iter()
            .find(|event| event.effort == "high")
            .expect("parent turn's effort should survive");
        assert_eq!(first_effort.timestamp, Some(original_turn_context));
    }

    /// Fork/spawn copies parent history into the child rollout with rewritten
    /// timestamps (GH-36). Content-based keys must count the replayed turns
    /// once while keeping the child's own new turn.
    #[test]
    fn fork_replay_of_parent_history_counts_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(day.join("rollout-a-parent.jsonl"), fork_parent_lines())
            .expect("fixture should be written");
        fs::write(day.join("rollout-b-child.jsonl"), fork_child_lines())
            .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_fork_replay_counted_once(&collection);
    }

    /// Same fixture with the CHILD sorting first: path order must not decide
    /// which timestamp survives (`archived_sessions` sorts before `sessions`
    /// wholesale, so originals are not guaranteed to be scanned first).
    #[test]
    fn fork_replay_keeps_original_timestamps_when_child_scans_first() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(day.join("rollout-a-child.jsonl"), fork_child_lines())
            .expect("fixture should be written");
        fs::write(day.join("rollout-b-parent.jsonl"), fork_parent_lines())
            .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_fork_replay_counted_once(&collection);
    }

    /// The replay burst is skipped structurally, so it contributes nothing
    /// even when the parent rollout is NOT in the scan (deleted, or outside
    /// the mtime window) — the case cross-file dedup alone cannot cover.
    #[test]
    fn fork_replay_skipped_without_parent_in_scan() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(day.join("rollout-child.jsonl"), fork_child_lines())
            .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 130);
        assert_eq!(collection.rate_limit_samples.len(), 1);
        assert_eq!(collection.effort_events.len(), 1);
        assert_eq!(collection.effort_events[0].effort, "xhigh");
        assert_eq!(collection.permission_events.len(), 1);
        assert_eq!(collection.permission_events[0].mode, "never");
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 222);
    }

    /// A re-emitted `token_count` whose cumulative did not advance reports no
    /// new consumption — summing it again would overcount, so it collapses.
    #[test]
    fn re_emitted_token_count_with_unchanged_cumulative_counts_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 110);
    }

    #[test]
    fn same_last_usage_with_advanced_cumulative_both_survive() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:09Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":200,"cached_input_tokens":0,"output_tokens":20,"total_tokens":220}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 220);
    }

    #[test]
    fn effort_dedup_by_turn_id() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","approval_policy":"on-request","turn_id":"t1"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:02Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","turn_id":"t2"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:59Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","approval_policy":"on-request","turn_id":"t1"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.effort_events.len(), 2);
    }

    /// A malformed cumulative (`{}` / `null`) must not enter the semantic-key
    /// path — it would fingerprint as all-zero and falsely collide distinct
    /// turns. Both events fall back to the positional key and survive.
    #[test]
    fn malformed_cumulative_falls_back_to_positional_key() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:09Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":null}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
    }

    #[test]
    fn rate_limit_renotification_with_moved_window_survives() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}},"rate_limits":{"primary":{"used_percent":37.5,"window_minutes":300,"resets_at":1750000000}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}},"rate_limits":{"primary":{"used_percent":37.5,"window_minutes":300,"resets_at":1750018000}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.rate_limit_samples.len(), 2);
    }

    /// Re-emission across a turn boundary (`turn_context` advances between the
    /// copies) still collapses — the reason `turn_id` is deliberately NOT part
    /// of the usage key.
    #[test]
    fn re_emission_across_turn_boundary_still_collapses() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","approval_policy":"on-request","turn_id":"t1"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:04Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","turn_id":"t2"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.effort_events.len(), 2);
    }

    #[test]
    fn effort_same_timestamp_distinct_turn_ids_both_survive() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","approval_policy":"on-request","turn_id":"t1"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","turn_id":"t2"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.effort_events.len(), 2);
    }

    #[test]
    fn cache_write_difference_yields_distinct_keys() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"cache_write_input_tokens":5,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
    }

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

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 110);
        assert_eq!(collection.session_touches.len(), 2);
    }

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
