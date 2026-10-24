use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ruzstd::decoding::StreamingDecoder;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::collector::{
    FileEvents, KeyedDurationEvent, KeyedEffortEvent, KeyedInterruptEvent, KeyedPaceEvent,
    KeyedPermissionEvent, KeyedRateLimitSample, KeyedToolEvent, KeyedUsageEvent,
    list_files_matching, merge_into, parse_files_cached, project_from_cwd,
};
use crate::model::{
    Collection, DurationEvent, EffortEvent, InterruptEvent, PaceEvent, PermissionEvent, Provider,
    RateLimitSample, SessionTouch, SourceKind, TokenUsage, ToolEvent, UsageEvent,
};

const COMPRESSED_SUFFIX: &str = ".zst";

/// Bounds the memory one line can take; a longer line is skipped as a parse
/// error instead of being buffered.
const MAX_LINE_BYTES: u64 = 64 << 20;

/// Decoded bytes a `.zst` may expand to, per on-disk byte: far above what
/// JSONL compresses to, far below what a run-length frame can claim. Past the
/// budget the file stops parsing.
const MAX_DECODE_RATIO: u64 = 64;
/// Floor of the decode budget, so a tiny file's frame overhead never trips it.
const MIN_DECODE_BUDGET: u64 = 64 << 20;

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

    // Parse one physical file per relative plain path (docs/codex.md) so
    // Codex durations (emitted without keys) and session touches are not
    // duplicated during merging.
    let mut chosen: HashMap<PathBuf, PathBuf> = HashMap::new();
    for dir in std::iter::once(root).chain(archived.as_deref()) {
        if !dir.exists() {
            continue;
        }
        for path in list_files_matching(dir, &is_rollout_file, mtime_floor, &mut collection.stats) {
            let rel = plain_rollout_path(path.strip_prefix(dir).unwrap_or(&path));
            match chosen.get_mut(&rel) {
                Some(existing) if supersedes(&path, existing) => *existing = path,
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

fn is_compressed(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

fn is_rollout_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl") || is_compressed(path)
}

fn plain_rollout_path(path: &Path) -> PathBuf {
    if !is_compressed(path) {
        return path.to_path_buf();
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(COMPRESSED_SUFFIX))
        .unwrap_or_default();
    path.with_file_name(name)
}

/// Plain beats compressed, as in Codex. The frame header's content size is
/// not compared: it is whatever the file claims, and a forged one would drop
/// the plain file's usage.
fn supersedes(candidate: &Path, existing: &Path) -> bool {
    let disk_len = |path: &Path| std::fs::metadata(path).map_or(0, |meta| meta.len());
    match (is_compressed(candidate), is_compressed(existing)) {
        (false, true) => true,
        (true, false) => false,
        _ => disk_len(candidate) > disk_len(existing),
    }
}

fn open_rollout(path: &Path) -> Option<Box<dyn BufRead>> {
    let file = File::open(path).ok()?;
    if is_compressed(path) {
        let budget = file
            .metadata()
            .ok()?
            .len()
            .saturating_mul(MAX_DECODE_RATIO)
            .max(MIN_DECODE_BUDGET);
        let decoder = StreamingDecoder::new(BufReader::new(file)).ok()?;
        return Some(Box::new(BufReader::new(DecodeBudget {
            inner: decoder,
            remaining: budget,
        })));
    }
    Some(Box::new(BufReader::new(file)))
}

/// Fails the read once more than `remaining` bytes would come out, so the
/// overrun surfaces as an error rather than a silently truncated file.
struct DecodeBudget<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for DecodeBudget<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            let mut probe = [0_u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::other("decoded rollout exceeds its budget")),
            };
        }
        let allowed = usize::try_from(self.remaining).map_or(buf.len(), |left| left.min(buf.len()));
        let read = self.inner.read(&mut buf[..allowed])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

enum Line {
    Text,
    Oversized,
}

/// Reads the next line into `line`; `Ok(None)` at end of input. A line past
/// `max_bytes` is consumed without being kept.
fn next_line(
    reader: &mut dyn BufRead,
    line: &mut Vec<u8>,
    max_bytes: u64,
) -> io::Result<Option<Line>> {
    line.clear();
    let read = reader.take(max_bytes + 1).read_until(b'\n', line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') && line.len() as u64 > max_bytes {
        line.clear();
        reader.skip_until(b'\n')?;
        return Ok(Some(Line::Oversized));
    }
    Ok(Some(Line::Text))
}

#[allow(
    clippy::too_many_lines,
    reason = "One pass over the rollout feeding every collector; splitting adds indirection without logic."
)]
fn parse_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    let mut reader = open_rollout(path)?;
    // Keys and fallback ids come from the plain name, so a rollout parses the
    // same whichever representation is on disk this run.
    let logical_path = plain_rollout_path(path);
    let mut events = FileEvents::default();
    let mut current_session_id = fallback_session_id(&logical_path);
    let mut pace_state = PaceState::default();
    let mut turn_items = TurnItems::default();
    let mut current_model = None;
    let mut current_project = None;
    let mut session_meta_count = 0usize;
    let mut fork_instant: Option<OffsetDateTime> = None;
    let mut own_thread_id: Option<String> = None;
    let mut origin = Origin::default();
    let mut recorded_usage: HashSet<String> = HashSet::new();
    let mut line = Vec::new();

    for line_index in 0.. {
        let read = next_line(reader.as_mut(), &mut line, MAX_LINE_BYTES);
        if matches!(read, Ok(None)) {
            break;
        }
        events.lines_seen += 1;
        // A broken zstd frame returns the same error on every read; skipping
        // it like a bad line would loop forever.
        let Ok(Some(read)) = read else {
            events.parse_errors += 1;
            break;
        };
        if matches!(read, Line::Oversized) {
            events.parse_errors += 1;
            continue;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            events.parse_errors += 1;
            continue;
        };

        let timestamp = parse_timestamp(value.get("timestamp"));
        let is_session_meta = value.get("type").and_then(Value::as_str) == Some("session_meta");
        if is_session_meta {
            session_meta_count += 1;
            if session_meta_count == 1 {
                own_thread_id = string_path(&value, &["payload", "id"]);
                origin = Origin::from_session_meta(&value);
                if is_copied_fork(&value) {
                    fork_instant = timestamp;
                }
            } else if session_meta_count == 2 {
                fork_instant = timestamp;
            }
            current_session_id = string_path(&value, &["payload", "id"]).or(current_session_id);
            current_model = session_model(&value).or(current_model);
            current_project =
                string_path(&value, &["payload", "cwd"]).map(|cwd| project_from_cwd(&cwd));
        }
        // Fork/spawn copies rewrite timestamps to the fork instant and shift
        // line positions (GH-36). Skip the replay burst even without a parent
        // in the scan; content keys deduplicate remaining replay when present.
        // Rows naming this thread as their owner (its own `session_meta`, its
        // own usage records) are never copies, whatever their timestamp.
        let own_session_meta = is_session_meta && session_meta_count == 1;
        let own_record = value.get("type").and_then(Value::as_str) == Some("token_usage_record")
            && own_thread_id.is_some()
            && string_path(&value, &["payload", "thread_id"]) == own_thread_id;
        let in_replay_burst = !own_session_meta
            && !own_record
            && fork_instant
                .zip(timestamp)
                .is_some_and(|(fork, ts)| in_fork_burst(fork, ts));
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

        let usage_context = UsageContext {
            timestamp,
            session_id: current_session_id.as_ref(),
            model: current_model.as_ref(),
            project: current_project.as_deref(),
            origin: &origin,
            line_index,
        };
        collect_usage_record(
            &value,
            &usage_context,
            own_thread_id.as_deref(),
            &mut recorded_usage,
            &mut events,
        );
        collect_usage_event(&value, &usage_context, &recorded_usage, &mut events);
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
            &origin,
            &logical_path,
            line_index,
            &mut events,
        );
    }

    events.compress_touches(local_offset);
    Some(events)
}

/// A `history_base` fork has no copied burst; skipping its first second would
/// drop its own rows.
fn is_copied_fork(session_meta: &Value) -> bool {
    let payload = session_meta.get("payload");
    payload
        .and_then(|p| p.get("forked_from_id"))
        .is_some_and(|id| !id.is_null())
        && payload
            .and_then(|p| p.get("history_base"))
            .is_none_or(Value::is_null)
}

/// Matching only the fork's own second counts a copy that spills into the
/// next second twice. Subtracting instead of adding a second keeps a fork at
/// the last representable instant from panicking.
fn in_fork_burst(fork: OffsetDateTime, at: OffsetDateTime) -> bool {
    at.unix_timestamp() >= fork.unix_timestamp() && at - fork < Duration::SECOND
}

/// Read from the first `session_meta` only: a fork child's second one is the
/// parent's copy and would make the child a main session.
#[derive(Default)]
struct Origin {
    agent: Option<String>,
}

impl Origin {
    fn from_session_meta(session_meta: &Value) -> Self {
        let Some(payload) = session_meta.get("payload") else {
            return Self::default();
        };
        let Some(Value::Object(source)) = payload.get("source") else {
            return Self::default();
        };
        let agent = if let Some(subagent) = source.get("subagent") {
            Some(subagent_name(subagent, payload))
        } else {
            source
                .get("internal")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        Self { agent }
    }

    fn source_kind(&self) -> SourceKind {
        if self.agent.is_some() {
            SourceKind::Subagent
        } else {
            SourceKind::Main
        }
    }
}

/// The SUBAGENTS row: a spawned thread's role (its nicknames are random per
/// spawn and would split one role into many rows), else the subagent kind.
fn subagent_name(subagent: &Value, payload: &Value) -> String {
    let name = match subagent {
        Value::String(kind) => Some(kind.clone()),
        Value::Object(variant) => match variant.get("thread_spawn") {
            Some(spawn) => Some(
                string_path(spawn, &["agent_role"])
                    .or_else(|| string_path(spawn, &["agent_type"]))
                    .or_else(|| string_path(payload, &["agent_role"]))
                    .unwrap_or_else(|| "default".to_owned()),
            ),
            None => variant
                .get("other")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| variant.keys().next().cloned()),
        },
        _ => None,
    };
    name.filter(|name| !name.is_empty())
        .unwrap_or_else(|| "subagent".to_owned())
}

/// Per-line attribution shared by the two usage sources.
struct UsageContext<'a> {
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&'a String>,
    model: Option<&'a String>,
    project: Option<&'a str>,
    origin: &'a Origin,
    line_index: usize,
}

impl UsageContext<'_> {
    fn event(&self, usage: TokenUsage) -> UsageEvent {
        UsageEvent {
            timestamp: self.timestamp,
            session_id: self.session_id.cloned(),
            model: self.model.cloned().or_else(|| Some("codex".to_owned())),
            source_kind: self.origin.source_kind(),
            attribution_agent: self.origin.agent.clone(),
            attribution_skill: None,
            project: self.project.map(ToOwned::to_owned),
            usage,
            reported_cost_usd: None,
            granularity: crate::model::UsageGranularity::Call,
        }
    }
}

/// The usage source wherever present (docs/codex.md). Every record's usage is
/// remembered, even one not counted here, so the `token_count` echoing it is
/// not counted twice.
fn collect_usage_record(
    value: &Value,
    context: &UsageContext<'_>,
    own_thread_id: Option<&str>,
    recorded: &mut HashSet<String>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("token_usage_record") {
        return;
    }
    let Some(usage_value) = value
        .get("payload")
        .and_then(|payload| payload.get("usage"))
        .and_then(fingerprintable)
    else {
        return;
    };
    recorded.insert(usage_fingerprint(usage_value));
    // A record owned by another thread is a copy of that thread's spending.
    if let (Some(owner), Some(own)) = (string_path(value, &["payload", "thread_id"]), own_thread_id)
        && owner != own
    {
        return;
    }
    let Some(usage) = parse_token_usage(usage_value) else {
        return;
    };
    let key = match string_path(value, &["payload", "response_id"]) {
        Some(response_id) => Some(format!("codex-usage:v3:{response_id}")),
        None => context.session_id.and_then(|sid| {
            positional_key("codex-record", sid, context.timestamp, context.line_index)
        }),
    };
    events.usage_events.push(KeyedUsageEvent {
        key,
        event: context.event(usage),
    });
}

fn collect_usage_event(
    value: &Value,
    context: &UsageContext<'_>,
    recorded: &HashSet<String>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    if string_path(value, &["payload", "type"]).as_deref() != Some("token_count") {
        return;
    }
    let session_id = context.session_id;
    collect_rate_limit_sample(
        value,
        context.timestamp,
        session_id,
        context.line_index,
        events,
    );
    let Some(last_usage) = value
        .get("payload")
        .and_then(|payload| payload.get("info"))
        .and_then(|info| info.get("last_token_usage"))
    else {
        return;
    };
    if recorded.contains(&usage_fingerprint(last_usage)) {
        return;
    }
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
        (Some(sid), None) => positional_key("codex", sid, context.timestamp, context.line_index),
        _ => None,
    };
    events.usage_events.push(KeyedUsageEvent {
        key,
        event: context.event(usage),
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

#[allow(
    clippy::too_many_arguments,
    reason = "Per-line parse context; bundling into a struct adds noise for one caller."
)]
fn collect_tool_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    origin: &Origin,
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
            source_kind: origin.source_kind(),
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
    // A `token_count` with only `total_tokens` (the post-compaction refill) is
    // no API call; counting it adds phantom calls.
    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }

    // input_tokens includes cached and cache-write input; subtract both so it
    // means fresh input, matching the Claude schema.
    let uncached = input_tokens.saturating_sub(cached_input_tokens);
    let cache_write = u64_field(value, "cache_write_input_tokens").min(uncached);
    Some(TokenUsage {
        input_tokens: uncached - cache_write,
        output_tokens,
        reasoning_output_tokens,
        cache_creation_input_tokens: cache_write,
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
    use std::io::Read;

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

    const COMPRESSED_PLAIN: &str =
        include_str!("../../tests/fixtures/codex/rollout-compressed.jsonl");
    const COMPRESSED_ZST: &[u8] =
        include_bytes!("../../tests/fixtures/codex/rollout-compressed.jsonl.zst");
    /// `resp_a` (1000 + 50) + `resp_b` (1200 + 80) in the fixture.
    const COMPRESSED_TOTAL: u64 = 2_330;

    fn total_volume(collection: &Collection) -> u64 {
        collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum()
    }

    fn usage_json(input: u64, cached: u64, output: u64) -> String {
        format!(
            r#"{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{total}}}"#,
            total = input + output,
        )
    }

    fn meta_line(ts: &str, id: &str, source: &str, extra: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"session_meta","payload":{{"id":"{id}","cwd":"/tmp/p","source":{source},"model_provider":"openai"{extra}}}}}"#
        )
    }

    fn record_line(ts: &str, thread: &str, response: &str, usage: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"token_usage_record","payload":{{"thread_id":"{thread}","turn_id":"t1","session_id":"{thread}","root_turn_id":"t1","response_id":"{response}","usage":{usage},"turn_token_usage":{usage},"thread_token_usage":{usage}}}}}"#
        )
    }

    fn token_count_line(ts: &str, last: &str, cumulative_total: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":0,"output_tokens":0,"total_tokens":{cumulative_total}}},"last_token_usage":{last},"model_context_window":258400}}}}}}"#
        )
    }

    fn write_lines(path: &Path, lines: &[String]) {
        fs::create_dir_all(path.parent().expect("fixture path has a parent"))
            .expect("test dirs should be created");
        let mut body = lines.join("\n");
        body.push('\n');
        fs::write(path, body).expect("fixture should be written");
    }

    #[test]
    fn compressed_fixture_decodes_to_its_plain_text() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("rollout-z.jsonl.zst");
        fs::write(&path, COMPRESSED_ZST).expect("fixture should be written");

        let mut decoded = Vec::new();
        open_rollout(&path)
            .expect("compressed rollout should open")
            .read_to_end(&mut decoded)
            .expect("compressed rollout should decode");

        assert_eq!(decoded, COMPRESSED_PLAIN.as_bytes());
    }

    #[test]
    fn reads_compressed_rollouts() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(day.join("rollout-z.jsonl.zst"), COMPRESSED_ZST)
            .expect("fixture should be written");
        fs::write(day.join("rollout-z.jsonl.zst.tmp"), b"partial")
            .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(total_volume(&collection), COMPRESSED_TOTAL);
        assert_eq!(collection.tool_events[0].tool_name, "cargo");
        assert_eq!(collection.duration_events.len(), 1);
    }

    /// The parse cache stores events per physical path, so the two
    /// representations must produce the same keys: when Codex compresses or
    /// restores a file between runs, the merge then sees one rollout, not two.
    #[test]
    fn plain_and_compressed_representations_yield_identical_keys() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let plain = temp.path().join("a/rollout-z.jsonl");
        let compressed = temp.path().join("b/rollout-z.jsonl.zst");
        fs::create_dir_all(plain.parent().expect("has parent")).expect("dirs");
        fs::create_dir_all(compressed.parent().expect("has parent")).expect("dirs");
        fs::write(&plain, COMPRESSED_PLAIN).expect("fixture should be written");
        fs::write(&compressed, COMPRESSED_ZST).expect("fixture should be written");

        let plain = parse_file(&plain, UtcOffset::UTC).expect("plain parses");
        let compressed = parse_file(&compressed, UtcOffset::UTC).expect("zst parses");

        let usage_keys = |events: &FileEvents| -> Vec<Option<String>> {
            events.usage_events.iter().map(|e| e.key.clone()).collect()
        };
        let tool_keys = |events: &FileEvents| -> Vec<Option<String>> {
            events.tool_events.iter().map(|e| e.key.clone()).collect()
        };
        let limit_keys = |events: &FileEvents| -> Vec<Option<String>> {
            events
                .rate_limit_samples
                .iter()
                .map(|e| e.key.clone())
                .collect()
        };
        assert_eq!(usage_keys(&plain), usage_keys(&compressed));
        assert_eq!(tool_keys(&plain), tool_keys(&compressed));
        assert_eq!(limit_keys(&plain), limit_keys(&compressed));
        assert_eq!(
            plain.session_touches.len(),
            compressed.session_touches.len()
        );
        assert_eq!(plain.lines_seen, compressed.lines_seen);
    }

    #[test]
    fn flipping_representation_between_runs_counts_the_session_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        let plain = day.join("rollout-z.jsonl");
        let compressed = day.join("rollout-z.jsonl.zst");
        let run = || collect(temp.path(), None, false, UtcOffset::UTC);

        fs::write(&plain, COMPRESSED_PLAIN).expect("fixture should be written");
        let before = run();
        fs::write(&compressed, COMPRESSED_ZST).expect("fixture should be written");
        let both = run();
        fs::remove_file(&plain).expect("plain should be removed");
        let after = run();

        for collection in [&before, &both, &after] {
            assert_eq!(collection.stats.files_seen, 1);
            assert_eq!(collection.usage_events.len(), 2);
            assert_eq!(total_volume(collection), COMPRESSED_TOTAL);
            assert_eq!(collection.duration_events.len(), 1);
            assert_eq!(collection.tool_events.len(), 1);
        }
    }

    /// A plain file beside its `.zst` wins, including the turn appended to it.
    #[test]
    fn resumed_plain_rollout_supersedes_its_compressed_sibling() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(day.join("rollout-z.jsonl.zst"), COMPRESSED_ZST)
            .expect("fixture should be written");
        let resumed = format!(
            "{COMPRESSED_PLAIN}{}\n",
            record_line(
                "2026-06-01T01:00:00Z",
                "z1",
                "resp_c",
                &usage_json(300, 0, 20)
            )
        );
        fs::write(day.join("rollout-z.jsonl"), resumed).expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 3);
        assert_eq!(total_volume(&collection), COMPRESSED_TOTAL + 320);
    }

    #[test]
    fn identical_plain_and_compressed_prefer_plain() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let plain = temp.path().join("rollout-z.jsonl");
        let compressed = temp.path().join("rollout-z.jsonl.zst");
        fs::write(&plain, COMPRESSED_PLAIN).expect("fixture should be written");
        fs::write(&compressed, COMPRESSED_ZST).expect("fixture should be written");

        assert!(supersedes(&plain, &compressed));
        assert!(!supersedes(&compressed, &plain));
    }

    /// Neither the frame header's claimed size nor the on-disk sizes decide:
    /// a plain copy smaller than the `.zst` itself still wins, in another
    /// directory too.
    #[test]
    fn plain_copy_wins_over_compressed_even_when_smaller() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let plain = temp.path().join("sessions/2026/06/01/rollout-z.jsonl");
        write_lines(
            &plain,
            &[
                meta_line("2026-06-01T00:00:00Z", "z1", r#""exec""#, ""),
                token_count_line("2026-06-01T00:00:03Z", &usage_json(100, 0, 10), 110),
            ],
        );
        let archived_day = temp.path().join("archived_sessions/2026/06/01");
        fs::create_dir_all(&archived_day).expect("test dirs should be created");
        fs::write(archived_day.join("rollout-z.jsonl.zst"), COMPRESSED_ZST)
            .expect("fixture should be written");
        assert!(fs::metadata(&plain).expect("plain exists").len() < COMPRESSED_ZST.len() as u64);

        let collection = collect(&temp.path().join("sessions"), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(total_volume(&collection), 110);
    }

    /// A zstd frame of raw and run-length blocks: `prefix`, `run` copies of
    /// `byte`, then `suffix`, at four bytes per 128 KiB of run.
    fn run_length_frame(prefix: &[u8], byte: u8, run: usize, suffix: &[u8]) -> Vec<u8> {
        const BLOCK: usize = 128 << 10;
        fn header(frame: &mut Vec<u8>, size: usize, kind: u32, last: bool) {
            let size = u32::try_from(size).expect("block size fits");
            let value = (size << 3) | (kind << 1) | u32::from(last);
            frame.extend_from_slice(&value.to_le_bytes()[..3]);
        }
        // Magic number, descriptor without content size, 128 KiB window.
        let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x38];
        header(&mut frame, prefix.len(), 0, false);
        frame.extend_from_slice(prefix);
        let mut left = run;
        while left > 0 {
            let size = left.min(BLOCK);
            header(&mut frame, size, 1, false);
            frame.push(byte);
            left -= size;
        }
        header(&mut frame, suffix.len(), 0, true);
        frame.extend_from_slice(suffix);
        frame
    }

    #[test]
    fn oversized_line_is_skipped_without_being_buffered() {
        let usage_line = format!(
            "{}\n",
            token_count_line("2026-06-01T00:00:03Z", &usage_json(100, 0, 10), 110)
        );
        let frame = run_length_frame(
            br#"{"pad":""#,
            b'a',
            1 << 20,
            format!("\"}}\n{usage_line}").as_bytes(),
        );
        let mut reader = BufReader::new(
            StreamingDecoder::new(frame.as_slice()).expect("frame header should parse"),
        );
        let mut line = Vec::new();

        let first = next_line(&mut reader, &mut line, 64 << 10).expect("decodes");
        assert!(matches!(first, Some(Line::Oversized)));
        assert!(line.capacity() < 1 << 20);
        let second = next_line(&mut reader, &mut line, 64 << 10).expect("decodes");
        assert!(matches!(second, Some(Line::Text)));
        assert_eq!(line, usage_line.as_bytes());
        assert!(
            next_line(&mut reader, &mut line, 64 << 10)
                .expect("decodes")
                .is_none()
        );
    }

    /// A few kilobytes of run-length blocks claim more than the decode
    /// budget; parsing stops with a parse error, keeping the rows before it.
    #[test]
    fn decompression_bomb_stops_at_the_decode_budget() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let usage_line = format!(
            "{}\n{}\n",
            meta_line("2026-06-01T00:00:00Z", "s1", r#""cli""#, ""),
            token_count_line("2026-06-01T00:00:03Z", &usage_json(100, 0, 10), 110)
        );
        let after_budget = format!(
            "\n{}\n",
            token_count_line("2026-06-01T00:00:09Z", &usage_json(200, 0, 20), 330)
        );
        let run = usize::try_from(MIN_DECODE_BUDGET).expect("budget fits");
        let frame = run_length_frame(usage_line.as_bytes(), b'a', run, after_budget.as_bytes());
        assert!(frame.len() < 8 << 10);
        let path = temp.path().join("rollout-s1.jsonl.zst");
        fs::write(&path, frame).expect("fixture should be written");

        let events = parse_file(&path, UtcOffset::UTC).expect("frame header should parse");

        assert_eq!(events.usage_events.len(), 1);
        assert_eq!(events.parse_errors, 1);
    }

    #[test]
    fn truncated_compressed_rollout_stops_with_a_parse_error() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("rollout-z.jsonl.zst");
        fs::write(&path, &COMPRESSED_ZST[..COMPRESSED_ZST.len() / 2])
            .expect("fixture should be written");

        let events = parse_file(&path, UtcOffset::UTC).expect("frame header is intact");

        assert_eq!(events.parse_errors, 1);
        assert!(events.usage_events.is_empty());
    }

    #[test]
    fn unreadable_compressed_header_counts_as_unreadable_file() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(temp.path().join("rollout-z.jsonl.zst"), b"not zstd")
            .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.stats.unreadable_files, 1);
    }

    /// Each record counts once; its `token_count` echoes (one repeated
    /// byte-identically), the embedded copy in `compacted` and the total-only
    /// refill row add nothing.
    #[test]
    fn usage_records_are_primary_and_their_token_count_echoes_count_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let first = usage_json(100, 40, 10);
        let compaction = usage_json(5_000, 0, 300);
        let third = usage_json(800, 700, 30);
        let echo = token_count_line("2026-06-01T00:00:03.100Z", &first, 110);
        write_lines(
            &temp.path().join("rollout-s1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "s1", r#""cli""#, ""),
                record_line("2026-06-01T00:00:03Z", "s1", "resp_1", &first),
                echo.clone(),
                echo.replace("00:00:03.100Z", "00:00:04.000Z"),
                record_line("2026-06-01T00:00:20Z", "s1", "resp_2", &compaction),
                format!(
                    r#"{{"timestamp":"2026-06-01T00:00:21Z","type":"compacted","payload":{{"message":"","compaction_response_id":"resp_2","latest_token_usage_record":{{"thread_id":"s1","response_id":"resp_2","usage":{compaction}}}}}}}"#
                ),
                r#"{"timestamp":"2026-06-01T00:00:22Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":0,"output_tokens":0,"total_tokens":258400},"last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":27000},"model_context_window":258400}}}"#.to_owned(),
                record_line("2026-06-01T00:00:30Z", "s1", "resp_3", &third),
                token_count_line("2026-06-01T00:00:30.100Z", &third, 5_940),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 3);
        assert_eq!(total_volume(&collection), 110 + 5_300 + 830);
    }

    /// A `token_count` with no matching record still counts beside the
    /// records that follow it.
    #[test]
    fn rollout_resumed_into_usage_records_counts_both_segments_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let early = usage_json(100, 0, 10);
        let late = usage_json(200, 100, 20);
        write_lines(
            &temp.path().join("rollout-s1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "s1", r#""cli""#, ""),
                token_count_line("2026-06-01T00:00:03Z", &early, 110),
                record_line("2026-06-02T00:00:03Z", "s1", "resp_1", &late),
                token_count_line("2026-06-02T00:00:03.100Z", &late, 330),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(total_volume(&collection), 110 + 220);
    }

    #[test]
    fn usage_record_of_another_thread_and_its_echo_are_not_counted() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let copied = usage_json(400, 0, 40);
        let own = usage_json(100, 0, 10);
        write_lines(
            &temp.path().join("rollout-c1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "c1", r#""cli""#, ""),
                record_line("2026-06-01T00:00:03Z", "p1", "resp_p", &copied),
                token_count_line("2026-06-01T00:00:03.100Z", &copied, 440),
                record_line("2026-06-01T00:00:09Z", "c1", "resp_c", &own),
                token_count_line("2026-06-01T00:00:09.100Z", &own, 550),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(total_volume(&collection), 110);
    }

    #[test]
    fn total_only_token_count_is_not_a_call() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_lines(
            &temp.path().join("rollout-s1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "s1", r#""cli""#, ""),
                token_count_line("2026-06-01T00:00:03Z", &usage_json(100, 0, 10), 110),
                r#"{"timestamp":"2026-06-01T00:00:04Z","type":"compacted","payload":{"message":""}}"#.to_owned(),
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":258400},"last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":27000}}}}"#.to_owned(),
                token_count_line("2026-06-01T00:00:09Z", &usage_json(0, 0, 12), 122),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(total_volume(&collection), 122);
    }

    #[test]
    fn cache_write_input_is_cache_creation_inside_input() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_lines(
            &temp.path().join("rollout-s1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "s1", r#""cli""#, ""),
                record_line(
                    "2026-06-01T00:00:03Z",
                    "s1",
                    "resp_1",
                    r#"{"input_tokens":100,"cached_input_tokens":40,"cache_write_input_tokens":30,"output_tokens":10,"reasoning_output_tokens":4,"total_tokens":110}"#,
                ),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let usage = &collection.usage_events[0].usage;
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.cache_read_input_tokens, 40);
        assert_eq!(usage.cache_creation_input_tokens, 30);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.reasoning_output_tokens, 4);
        assert_eq!(usage.token_volume(), 110);
    }

    #[test]
    fn subagent_rollouts_are_attributed_by_their_own_session_source() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let usage = usage_json(100, 0, 10);
        let cases = [
            (
                "a",
                r#"{"subagent":{"thread_spawn":{"parent_thread_id":"p","depth":1,"agent_nickname":"Nick","agent_role":"explorer"}}}"#,
                Some("explorer"),
            ),
            (
                "b",
                r#"{"subagent":{"thread_spawn":{"parent_thread_id":"p","depth":1,"agent_nickname":"Nick","agent_role":null}}}"#,
                Some("default"),
            ),
            ("c", r#"{"subagent":"review"}"#, Some("review")),
            (
                "d",
                r#"{"subagent":{"other":"guardian"}}"#,
                Some("guardian"),
            ),
            ("e", r#"{"internal":"guardian"}"#, Some("guardian")),
            ("f", r#""exec""#, None),
            ("g", r#"{"custom":"host-app"}"#, None),
        ];
        for (id, source, _) in &cases {
            write_lines(
                &temp.path().join(format!("rollout-{id}.jsonl")),
                &[
                    meta_line("2026-06-01T00:00:00Z", id, source, ""),
                    record_line("2026-06-01T00:00:03Z", id, &format!("resp_{id}"), &usage),
                    format!(
                        r#"{{"timestamp":"2026-06-01T00:00:04Z","type":"response_item","payload":{{"type":"function_call","call_id":"call_{id}","name":"apply_patch","arguments":"{{}}"}}}}"#
                    ),
                ],
            );
        }

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), cases.len());
        for (id, _, agent) in &cases {
            let event = collection
                .usage_events
                .iter()
                .find(|event| event.session_id.as_deref() == Some(*id))
                .expect("each rollout reports its usage");
            assert_eq!(event.attribution_agent.as_deref(), *agent, "rollout {id}");
            let expected = if agent.is_some() {
                SourceKind::Subagent
            } else {
                SourceKind::Main
            };
            assert_eq!(event.source_kind, expected, "rollout {id}");
            let tool = collection
                .tool_events
                .iter()
                .find(|event| event.session_id.as_deref() == Some(*id))
                .expect("each rollout reports its tool");
            assert_eq!(tool.source_kind, expected, "rollout {id}");
        }
    }

    /// The spawned child's copy of its parent opens with the parent's own
    /// `session_meta` (`source: "cli"`); that must not reclassify the child.
    #[test]
    fn fork_child_stays_a_subagent_past_the_copied_parent_session_meta() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_lines(
            &temp.path().join("rollout-c1.jsonl"),
            &[
                meta_line(
                    "2026-06-01T01:00:00Z",
                    "c1",
                    r#"{"subagent":{"thread_spawn":{"parent_thread_id":"p1","depth":1,"agent_role":"worker"}}}"#,
                    r#","forked_from_id":"p1""#,
                ),
                meta_line("2026-06-01T01:00:00Z", "p1", r#""cli""#, ""),
                record_line(
                    "2026-06-01T01:00:09Z",
                    "c1",
                    "resp_c",
                    &usage_json(100, 0, 10),
                ),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].source_kind, SourceKind::Subagent);
        assert_eq!(
            collection.usage_events[0].attribution_agent.as_deref(),
            Some("worker")
        );
    }

    /// Copied parent `token_count`s in the fork window, one of them past the
    /// second boundary, add nothing beside the parent's records.
    #[test]
    fn fork_copies_of_parent_token_counts_are_not_recounted_against_records() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let first = usage_json(100, 40, 10);
        let second = usage_json(120, 0, 20);
        let own = usage_json(90, 10, 40);
        write_lines(
            &temp.path().join("rollout-a-parent.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "p1", r#""cli""#, ""),
                record_line("2026-06-01T00:00:03Z", "p1", "resp_1", &first),
                token_count_line("2026-06-01T00:00:03.100Z", &first, 110),
                record_line("2026-06-01T00:00:09Z", "p1", "resp_2", &second),
                token_count_line("2026-06-01T00:00:09.100Z", &second, 250),
            ],
        );
        write_lines(
            &temp.path().join("rollout-b-child.jsonl"),
            &[
                meta_line(
                    "2026-06-01T01:00:00.998Z",
                    "c1",
                    r#"{"subagent":{"thread_spawn":{"parent_thread_id":"p1","depth":1,"agent_role":"worker"}}}"#,
                    r#","forked_from_id":"p1""#,
                ),
                meta_line("2026-06-01T01:00:00.999Z", "p1", r#""cli""#, ""),
                token_count_line("2026-06-01T01:00:00.999Z", &first, 110),
                token_count_line("2026-06-01T01:00:01.001Z", &second, 250),
                record_line("2026-06-01T01:00:09Z", "c1", "resp_c", &own),
                token_count_line("2026-06-01T01:00:09.100Z", &own, 380),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 3);
        assert_eq!(total_volume(&collection), 110 + 140 + 130);
    }

    /// A fork child with a single `session_meta` skips its fork window; a
    /// `history_base` fork has none.
    #[test]
    fn single_meta_fork_child_skips_its_copied_burst() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let own = usage_json(90, 10, 40);
        write_lines(
            &temp.path().join("rollout-c1.jsonl"),
            &[
                meta_line(
                    "2026-06-01T01:00:00.999Z",
                    "c1",
                    r#"{"subagent":{"thread_spawn":{"parent_thread_id":"p1","depth":1}}}"#,
                    r#","forked_from_id":"p1""#,
                ),
                token_count_line("2026-06-01T01:00:00.999Z", &usage_json(100, 40, 10), 110),
                token_count_line("2026-06-01T01:00:01.002Z", &usage_json(120, 0, 20), 250),
                token_count_line("2026-06-01T01:00:30Z", &own, 380),
            ],
        );
        write_lines(
            &temp.path().join("rollout-r1.jsonl"),
            &[
                meta_line(
                    "2026-06-01T02:00:00Z",
                    "r1",
                    r#""vscode""#,
                    r#","forked_from_id":"p1","history_base":{"thread_id":"p1","end_ordinal_exclusive":40,"end_byte_offset":9000}"#,
                ),
                token_count_line("2026-06-01T02:00:00.500Z", &usage_json(60, 0, 6), 66),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(total_volume(&collection), 130 + 66);
    }

    /// Records name their owner, so the child's own response written inside
    /// the fork window counts while the copied rows around it do not.
    #[test]
    fn child_owned_record_inside_the_fork_window_counts() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let copied = usage_json(100, 40, 10);
        let own = usage_json(90, 10, 40);
        write_lines(
            &temp.path().join("rollout-c1.jsonl"),
            &[
                meta_line(
                    "2026-06-01T01:00:00Z",
                    "c1",
                    r#"{"subagent":{"thread_spawn":{"parent_thread_id":"p1","depth":1}}}"#,
                    r#","forked_from_id":"p1""#,
                ),
                meta_line("2026-06-01T01:00:00Z", "p1", r#""cli""#, ""),
                token_count_line("2026-06-01T01:00:00Z", &copied, 110),
                record_line("2026-06-01T01:00:00.500Z", "c1", "resp_c", &own),
                token_count_line("2026-06-01T01:00:00.600Z", &own, 240),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(total_volume(&collection), 130);
    }

    #[test]
    fn fork_at_the_last_representable_instant_does_not_panic() {
        let last = OffsetDateTime::parse("9999-12-31T23:59:59.999999999Z", &Rfc3339)
            .expect("max timestamp should parse");
        assert!(in_fork_burst(last, last));

        let temp = TempDir::new().expect("test tempdir should be created");
        write_lines(
            &temp.path().join("rollout-c1.jsonl"),
            &[
                meta_line(
                    "9999-12-31T23:59:59.999Z",
                    "c1",
                    r#""cli""#,
                    r#","forked_from_id":"p1""#,
                ),
                token_count_line("9999-12-31T23:59:59.999Z", &usage_json(100, 0, 10), 110),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert!(collection.usage_events.is_empty());
    }
}
