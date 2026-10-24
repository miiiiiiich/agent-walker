use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::collector::{
    FileEvents, KeyedDurationEvent, KeyedEffortEvent, KeyedInterruptEvent, KeyedModeEvent,
    KeyedPaceEvent, KeyedPermissionEvent, KeyedToolEvent, KeyedUsageEvent, list_files, merge_into,
    parse_files_cached, project_from_cwd,
};
use crate::model::{
    Collection, DurationEvent, EffortEvent, InterruptEvent, ModeEvent, PaceEvent, PermissionEvent,
    Provider, SessionTouch, SourceKind, TokenUsage, ToolEvent, UsageEvent,
};

/// Background/observer harnesses (e.g. the claude-mem observer) keep
/// always-on sessions that would inflate session and active-day stats.
const NOISE_DIR_MARKERS: [&str; 1] = ["claude-mem-observer"];

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Claude, root.to_path_buf());
    if !root.exists() {
        return collection;
    }

    let files: Vec<_> = list_files(root, "jsonl", mtime_floor, &mut collection.stats)
        .into_iter()
        .filter(|path| !is_noise_path(path))
        .collect();
    let per_file = parse_files_cached(
        use_cache.then_some("claude"),
        &files,
        local_offset,
        |path| parse_file(path, root, local_offset),
    );
    merge_into(&mut collection, per_file);
    collection
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
    // Trim and normalize the home prefix so a trailing separator cannot produce a double hyphen.
    let home_prefix = crate::paths::home_dir()
        .ok()
        .and_then(|home| home.to_str().map(ToOwned::to_owned))
        .map(|home| {
            let trimmed = home.trim_end_matches(['/', '\\']);
            format!("{}-", trimmed.replace([':', '/', '\\'], "-"))
        })
        .unwrap_or_default();
    let trimmed = directory
        .strip_prefix(&home_prefix)
        .unwrap_or(directory)
        .trim_start_matches('-');
    trimmed.to_owned()
}

fn parse_file(path: &Path, root: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    let file = File::open(path).ok()?;
    let mut events = FileEvents::default();
    let source_kind = path_source_kind(path);
    let file_agent_id = file_agent_id(path);
    // Prefer the raw `cwd` carried on log lines (real slashes); the sanitized
    // directory name is a lossy fallback.
    let mut project = None;
    let fallback_project = project_label(path, root);
    let mut turn = TurnState::default();
    // Resumes and fork prefixes replay earlier rows verbatim, sometimes
    // in the middle of a later turn. A prompt uuid seen before must not
    // flush and restart the turn, and a question id seen before must not
    // reopen (its replayed answer then finds nothing pending and is inert).
    let mut seen_prompts: HashSet<String> = HashSet::new();
    let mut seen_questions: HashSet<String> = HashSet::new();
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
        if source_kind == SourceKind::Main {
            if is_interrupt_marker(&value) {
                // Clear an interrupted turn before timestamp validation; an undated
                // marker must also prevent a completed-duration event.
                turn = TurnState::default();
            } else if let Some(timestamp) = parse_timestamp(value.get("timestamp")) {
                if is_human_turn(&value) {
                    let key = string_field(&value, "uuid");
                    let replayed = key
                        .as_ref()
                        .is_some_and(|uuid| !seen_prompts.insert(uuid.clone()));
                    if !replayed {
                        // The gap from the previous turn's last activity to
                        // this prompt is the human's pace — only while the
                        // turn is still open (a cutoff means they were away).
                        if let (Some(_), Some(end)) = (turn.start, turn.last_activity)
                            && timestamp > end
                            && timestamp - end <= Duration::minutes(30)
                        {
                            let gap_ms =
                                u64::try_from((timestamp - end).whole_milliseconds()).unwrap_or(0);
                            events.pace_events.push(KeyedPaceEvent {
                                key: key.as_ref().map(|uuid| format!("claude-pace:{uuid}")),
                                event: PaceEvent {
                                    timestamp: Some(timestamp),
                                    gap_ms,
                                },
                            });
                        }
                        turn.flush(&mut events);
                        turn = TurnState::opened_at(&value, timestamp, key);
                    }
                } else if let Some(previous) = turn.last_activity {
                    if timestamp - previous > Duration::minutes(30) {
                        // A long silence means the turn ended and the session
                        // was resumed later (compaction, scheduled appends);
                        // close the turn at the last real activity instead of
                        // spanning days. A question left open that long ended
                        // the turn when it was asked (`flush` charges the
                        // tail as waiting), and its late answer is a fresh
                        // human input: the next turn starts there.
                        turn.flush(&mut events);
                        turn = turn.after_cutoff();
                        turn.open_from(&value, timestamp, &mut seen_prompts);
                    } else if turn.charge_answer(&value, timestamp) {
                        turn.last_activity = Some(previous.max(timestamp));
                    } else {
                        let is_assistant =
                            value.get("type").and_then(Value::as_str) == Some("assistant");
                        turn.responded |= is_assistant;
                        // An assistant row while a question is still open
                        // (a replay, or a parallel tool call answered first)
                        // is inside the human's wait, not the model's time.
                        if is_assistant && timestamp > previous && turn.pending_questions.is_empty()
                        {
                            let gap = u64::try_from((timestamp - previous).whole_milliseconds())
                                .unwrap_or(0);
                            turn.model_ms = turn.model_ms.saturating_add(gap);
                        }
                        turn.note_questions(&value, timestamp, &mut seen_questions);
                        turn.open_if_generated(&value, timestamp, &mut seen_prompts);
                        turn.last_activity = Some(previous.max(timestamp));
                    }
                } else {
                    turn.open_from(&value, timestamp, &mut seen_prompts);
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

    turn.flush(&mut events);
    events.compress_touches(local_offset);
    Some(events)
}

/// Generated prompts that the origin marker alone lets through: scheduled
/// runs and shell output are marked human, and older logs have no marker.
const GENERATED_PROMPT_PREFIXES: [&str; 6] = [
    "<task-notification>",
    "<scheduled-task",
    "<bash-stdout>",
    "<bash-stderr>",
    "<local-command-stdout>",
    "<local-command-stderr>",
];

fn flagged(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_bool).unwrap_or(false)
}

/// The text of a main-thread user row that reads as a prompt rather than a
/// tool result.
fn prompt_text(value: &Value) -> Option<&str> {
    if value.get("type").and_then(Value::as_str) != Some("user")
        || flagged(value, "isSidechain")
        || flagged(value, "isMeta")
    {
        return None;
    }
    match value
        .get("message")
        .and_then(|message| message.get("content"))?
    {
        Value::String(text) => Some(text.as_str()),
        Value::Array(blocks) => {
            if blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            {
                return None;
            }
            blocks
                .iter()
                .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .and_then(|block| block.get("text"))
                .and_then(Value::as_str)
        }
        _ => None,
    }
}

/// A prompt Claude Code wrote itself (a background-task notification, a
/// peer session's message, a scheduled run) is not a human turn: counting
/// it as one splits one delegation into several short turns and records
/// the wait for a background task as the human's pace.
fn is_human_turn(value: &Value) -> bool {
    let Some(text) = prompt_text(value) else {
        return false;
    };
    if flagged(value, "isCompactSummary") {
        return false;
    }
    let origin = value
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
        .or_else(|| value.get("turnOrigin").and_then(Value::as_str));
    if origin.is_some_and(|kind| kind != "human") {
        return false;
    }
    let text = text.trim_start();
    !GENERATED_PROMPT_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

fn is_compact_command(value: &Value) -> bool {
    prompt_text(value).is_some_and(|text| text.contains("<command-name>/compact</command-name>"))
}

fn is_generated_prompt(value: &Value) -> bool {
    prompt_text(value).is_some() && !is_human_turn(value)
}

#[derive(Default)]
struct TurnState {
    start: Option<OffsetDateTime>,
    session_id: Option<String>,
    /// The prompt row's uuid — fork children replay the parent's history,
    /// so a turn copied into a child file must dedupe against the original.
    key: Option<String>,
    last_activity: Option<OffsetDateTime>,
    human_wait_ms: u64,
    model_ms: u64,
    pending_questions: HashMap<String, OffsetDateTime>,
    /// End of the last charged wait, so overlapping questions answered in
    /// sequence charge their shared interval once.
    wait_cursor: Option<OffsetDateTime>,
    /// A local slash command, or a prompt cancelled before the first reply,
    /// opens a turn the model never answers; without this, its output and
    /// reminder rows count as a short turn of agent work. A manual compaction
    /// runs the model without writing an assistant row, so its command row
    /// counts as answered.
    responded: bool,
    /// Opened by a generated prompt with no turn in progress. Dropping such
    /// a turn loses the scheduled run or the post-notification work from
    /// working time, so it is kept, but it took no human prompt to start.
    generated: bool,
}

impl TurnState {
    fn answered_ids(&self, value: &Value) -> Vec<String> {
        if self.pending_questions.is_empty()
            || value.get("type").and_then(Value::as_str) != Some("user")
        {
            return Vec::new();
        }
        let Some(Value::Array(blocks)) = value
            .get("message")
            .and_then(|message| message.get("content"))
        else {
            return Vec::new();
        };
        blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
            .filter(|id| self.pending_questions.contains_key(*id))
            .map(str::to_owned)
            .collect()
    }

    /// Record the questions an assistant row asks — first sighting only,
    /// so a replayed row cannot reopen one (`seen` is file-wide).
    fn note_questions(
        &mut self,
        value: &Value,
        asked_at: OffsetDateTime,
        seen: &mut HashSet<String>,
    ) {
        for id in question_tool_use_ids(value) {
            if seen.insert(id.clone()) {
                self.pending_questions.insert(id, asked_at);
            }
        }
    }

    fn after_cutoff(self) -> Self {
        Self {
            session_id: self.session_id,
            pending_questions: self.pending_questions,
            ..Self::default()
        }
    }

    /// If the row answers a question left open across a cutoff, retire it
    /// and start a fresh turn here: the late answer is the human's input.
    /// The wait before the cutoff was charged to the previous turn, so this
    /// one starts clean, with the cursor at its start so questions still
    /// open charge only from here.
    fn restart_if_answer(&mut self, value: &Value, now: OffsetDateTime) {
        let answered = self.answered_ids(value);
        if answered.is_empty() {
            return;
        }
        for id in answered {
            self.pending_questions.remove(&id);
        }
        self.start = Some(now);
        self.key = string_field(value, "uuid");
        if let Some(id) = session_id_field(value) {
            self.session_id = Some(id);
        }
        self.last_activity = Some(now);
        self.human_wait_ms = 0;
        self.wait_cursor = Some(now);
        self.responded = false;
        self.generated = false;
    }

    /// With no turn in progress, a late answer or a generated prompt opens one.
    fn open_from(
        &mut self,
        value: &Value,
        now: OffsetDateTime,
        seen_prompts: &mut HashSet<String>,
    ) {
        self.restart_if_answer(value, now);
        self.open_if_generated(value, now, seen_prompts);
    }

    /// With no turn in progress, a generated prompt opens one; inside a turn
    /// it continues that turn. Its uuid is remembered either way so a
    /// replayed copy cannot open a turn later.
    fn open_if_generated(
        &mut self,
        value: &Value,
        now: OffsetDateTime,
        seen_prompts: &mut HashSet<String>,
    ) {
        if !is_generated_prompt(value) {
            return;
        }
        let key = string_field(value, "uuid");
        let replayed = key
            .as_ref()
            .is_some_and(|uuid| !seen_prompts.insert(uuid.clone()));
        if replayed || self.start.is_some() {
            return;
        }
        *self = Self::opened_at(value, now, key);
    }

    fn opened_at(value: &Value, now: OffsetDateTime, key: Option<String>) -> Self {
        Self {
            start: Some(now),
            session_id: session_id_field(value),
            key,
            last_activity: Some(now),
            responded: is_compact_command(value),
            generated: is_generated_prompt(value),
            ..Self::default()
        }
    }

    /// Waiting so far plus the stretch the human has been blocked on up to
    /// `until`: from the earliest still-open question (the human has been
    /// busy since then, whichever question they answer first) or from the
    /// end of the last charged stretch, whichever is later — so overlapping
    /// questions never charge a shared interval twice.
    fn waited_until(&self, until: OffsetDateTime) -> u64 {
        let Some(earliest) = self.pending_questions.values().min().copied() else {
            return self.human_wait_ms;
        };
        let from = self
            .wait_cursor
            .map_or(earliest, |cursor| cursor.max(earliest));
        if until <= from {
            return self.human_wait_ms;
        }
        let wait = u64::try_from((until - from).whole_milliseconds()).unwrap_or(0);
        self.human_wait_ms.saturating_add(wait)
    }

    /// Reminders and hook attachments between a question and its answer must not
    /// shorten the wait, which is measured from the ask.
    fn charge_answer(&mut self, value: &Value, now: OffsetDateTime) -> bool {
        let answered = self.answered_ids(value);
        if answered.is_empty() {
            return false;
        }
        self.human_wait_ms = self.waited_until(now);
        self.wait_cursor = Some(self.wait_cursor.map_or(now, |cursor| cursor.max(now)));
        for id in answered {
            self.pending_questions.remove(&id);
        }
        true
    }

    /// A question still open at the end charges its tail as waiting — the agent stopped
    /// working when it asked.
    fn flush(&self, events: &mut FileEvents) {
        let (Some(start), Some(end)) = (self.start, self.last_activity) else {
            return;
        };
        if !self.responded {
            return;
        }
        let duration_ms = u64::try_from((end - start).whole_milliseconds()).unwrap_or(0);
        if duration_ms == 0 {
            return;
        }
        events.duration_events.push(KeyedDurationEvent {
            key: self.key.as_ref().map(|uuid| format!("claude-turn:{uuid}")),
            event: DurationEvent {
                timestamp: Some(end),
                session_id: self.session_id.clone(),
                duration_ms,
                human_wait_ms: self.waited_until(end).min(duration_ms),
                model_ms: Some(self.model_ms.min(duration_ms)),
                status: Some(if self.generated { "generated" } else { "turn" }.to_owned()),
            },
        });
    }
}

fn question_tool_use_ids(value: &Value) -> Vec<String> {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let Some(Value::Array(blocks)) = value
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_use")
                && block.get("name").and_then(Value::as_str) == Some("AskUserQuestion")
        })
        .filter_map(|block| block.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn session_id_field(value: &Value) -> Option<String> {
    string_field(value, "sessionId")
        .or_else(|| string_field(value, "session_id"))
        .or_else(|| string_field(value, "session_id_v2"))
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
    let session_id = session_id_field(value);
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
    let attribution_skill = string_field(value, "attributionSkill")
        .or_else(|| string_field(value, "attribution_skill"));

    let Some(message) = value.get("message") else {
        return;
    };
    // Request-log rows (`api-request-blob`) replay earlier tool calls without
    // a timestamp; only transcript rows are a call's own record.
    if !matches!(
        value.get("type").and_then(Value::as_str),
        Some("user" | "assistant")
    ) {
        return;
    }

    collect_mode_event(value, message, timestamp, events);
    collect_effort_event(value, message, timestamp, events);
    collect_permission_event(value, timestamp, events);
    collect_interrupt_event(value, timestamp, source_kind, events);

    let usage_value = message.get("usage").or_else(|| value.get("usage"));
    let message_id = string_field(message, "id");
    let top_model = string_field(message, "model").or_else(|| string_field(value, "model"));
    if let Some(usage) = parse_usage(usage_value) {
        events.usage_events.push(KeyedUsageEvent {
            key: message_id.as_ref().map(|id| format!("message:{id}")),
            event: UsageEvent {
                timestamp,
                session_id: session_id.clone(),
                model: top_model.clone(),
                source_kind: event_source_kind,
                attribution_agent: attribution_agent.clone(),
                attribution_skill: attribution_skill.clone(),
                project: project.map(ToOwned::to_owned),
                usage,
                reported_cost_usd: None,
                granularity: crate::model::UsageGranularity::Call,
            },
        });
    }

    // Top-level usage includes main-model iterations; emit only separately billed
    // advisor iterations, keyed by message and iteration index (ccusage#1115).
    if let Some(iterations) = usage_value
        .and_then(|usage| usage.get("iterations"))
        .and_then(Value::as_array)
    {
        for (index, iteration) in iterations.iter().enumerate() {
            if string_field(iteration, "type").as_deref() != Some("advisor_message") {
                continue;
            }
            let Some(usage) = parse_usage(Some(iteration)) else {
                continue;
            };
            events.usage_events.push(KeyedUsageEvent {
                key: message_id
                    .as_ref()
                    .map(|id| format!("message:{id}:iter:{index}")),
                event: UsageEvent {
                    timestamp,
                    session_id: session_id.clone(),
                    model: string_field(iteration, "model").or_else(|| top_model.clone()),
                    source_kind: event_source_kind,
                    attribution_agent: attribution_agent.clone(),
                    attribution_skill: attribution_skill.clone(),
                    project: project.map(ToOwned::to_owned),
                    usage,
                    reported_cost_usd: None,
                    granularity: crate::model::UsageGranularity::Call,
                },
            });
        }
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

fn collect_mode_event(
    value: &Value,
    message: &Value,
    timestamp: Option<OffsetDateTime>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(message_id) = string_field(message, "id") else {
        return;
    };
    let has_thinking = message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("thinking"))
        });
    let fast = message
        .get("usage")
        .and_then(|usage| usage.get("speed"))
        .and_then(Value::as_str)
        == Some("fast");
    events.mode_events.push(KeyedModeEvent {
        key: Some(format!("mode:{message_id}")),
        event: ModeEvent {
            timestamp,
            has_thinking,
            fast,
        },
    });
}

/// One permission event per human turn (keyed by the row uuid, which resume /
/// fork copies share), from the top-level `permissionMode` field. Gated on
/// `is_human_turn`: cross-session agent-message rows (`isMeta`) also carry
/// the field and would let orchestration-heavy windows swamp the mix. The
/// `type:"permission-mode"` change-stream rows are deliberately not used —
/// per-turn values give the distribution, not just the switch points.
fn collect_permission_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    events: &mut FileEvents,
) {
    if !is_human_turn(value) {
        return;
    }
    let Some(mode) = string_field(value, "permissionMode") else {
        return;
    };
    let Some(uuid) = string_field(value, "uuid") else {
        return;
    };
    events.permission_events.push(KeyedPermissionEvent {
        key: Some(format!("claude-permission:{uuid}")),
        event: PermissionEvent { timestamp, mode },
    });
}

/// Count only exact main-thread interruption markers; exclude `isMeta` rows
/// (they can quote the marker) and sidechain echoes, accepting that
/// sidechain-only interruptions are omitted.
fn is_interrupt_marker(value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let flagged = |field: &str| value.get(field).and_then(Value::as_bool).unwrap_or(false);
    if flagged("isMeta") || flagged("isSidechain") {
        return false;
    }
    let is_marker = |text: &str| {
        matches!(
            text.trim(),
            "[Request interrupted by user]" | "[Request interrupted by user for tool use]"
        )
    };
    // Array content must be SOLELY the marker block (every real marker row
    // is a single-text array) — a prompt attaching an image alongside a
    // quoted marker is a real message, not an esc.
    match value
        .get("message")
        .and_then(|message| message.get("content"))
    {
        Some(Value::String(text)) => is_marker(text),
        Some(Value::Array(blocks)) => match blocks.as_slice() {
            [block] => block
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(is_marker),
            _ => false,
        },
        _ => false,
    }
}

/// Count marker rows alone because `interruptedMessageId` rows are a strict subset.
fn collect_interrupt_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    source_kind: SourceKind,
    events: &mut FileEvents,
) {
    if source_kind != SourceKind::Main {
        return;
    }
    if !is_interrupt_marker(value) {
        return;
    }
    let Some(uuid) = string_field(value, "uuid") else {
        return;
    };
    events.interrupt_events.push(KeyedInterruptEvent {
        key: Some(format!("claude-interrupt:{uuid}")),
        event: InterruptEvent { timestamp },
    });
}

fn collect_effort_event(
    value: &Value,
    message: &Value,
    timestamp: Option<OffsetDateTime>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(effort) = string_field(value, "effort") else {
        return;
    };
    let Some(message_id) = string_field(message, "id") else {
        return;
    };
    events.effort_events.push(KeyedEffortEvent {
        key: Some(format!("claude-effort:{message_id}")),
        event: EffortEvent { timestamp, effort },
    });
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
            r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30},"content":[{"type":"tool_use","name":"Agent","input":{"subagent_type":"Explore"}},{"type":"tool_use","name":"Read","input":{"file_path":"/secret"}}]}}"#,
        )
        .expect("main fixture should be written");
        fs::write(
            subagent_dir.join("agent-abc.jsonl"),
            r#"{"timestamp":"2026-06-01T00:01:00Z","sessionId":"s1","isSidechain":true,"attributionAgent":"Explore","type":"assistant","message":{"id":"m2","model":"claude-haiku-4-5","usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":10},"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo hidden"}}]}}"#,
        )
        .expect("subagent fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

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

    /// A mid-turn model fallback: the failed attempt in `usage.iterations`
    /// is NOT billed (the turn is billed as the serving model, mirrored at
    /// the top level), so exactly one event must come out — the top-level
    /// serving call. A streamed duplicate still dedupes.
    #[test]
    fn fallback_attempt_is_not_counted() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let line = r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":2,"output_tokens":2156,"cache_creation_input_tokens":0,"cache_read_input_tokens":313782,"iterations":[{"type":"message","model":"claude-fable-5","input_tokens":2,"output_tokens":601,"cache_creation_input_tokens":991,"cache_read_input_tokens":495675},{"type":"fallback_message","model":"claude-opus-4-8","input_tokens":2,"output_tokens":2156,"cache_creation_input_tokens":0,"cache_read_input_tokens":313782}]},"content":[{"type":"text","text":"hi"}]}}"#;
        fs::write(
            temp.path().join("session.jsonl"),
            format!("{line}\n{line}\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(
            collection.usage_events[0].model.as_deref(),
            Some("claude-opus-4-8")
        );
        assert_eq!(
            collection.usage_events[0].usage.token_volume(),
            2 + 2156 + 313_782
        );
    }

    /// An advisor turn (ccusage#1115 shape): the top level sums the
    /// main-model iterations, while the `advisor_message` in between is an
    /// additional billed call under its own model, absent from the top-level
    /// counters — it must surface as its own event, and the main-model
    /// iterations must not be re-emitted.
    #[test]
    fn advisor_iteration_is_counted_under_its_own_model() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-sonnet-5","usage":{"input_tokens":22,"output_tokens":12,"cache_creation_input_tokens":0,"cache_read_input_tokens":220,"iterations":[{"type":"message","model":"claude-sonnet-5","input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":100},{"type":"advisor_message","model":"claude-opus-4-8","input_tokens":3,"output_tokens":9,"cache_creation_input_tokens":0,"cache_read_input_tokens":50},{"type":"message","model":"claude-sonnet-5","input_tokens":12,"output_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":120}]},"content":[{"type":"text","text":"hi"}]}}"#,
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        let advisor = collection
            .usage_events
            .iter()
            .find(|event| event.model.as_deref() == Some("claude-opus-4-8"))
            .expect("advisor call should be counted");
        assert_eq!(advisor.usage.token_volume(), 3 + 9 + 50);
        let main = collection
            .usage_events
            .iter()
            .find(|event| event.model.as_deref() == Some("claude-sonnet-5"))
            .expect("main turn should be counted once");
        assert_eq!(main.usage.token_volume(), 22 + 12 + 220);
    }

    #[test]
    fn single_mirror_iteration_adds_nothing() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"iterations":[{"type":"message","input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30}]},"content":[{"type":"text","text":"hi"}]}}"#,
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 63);
    }

    #[test]
    fn collects_skill_attribution_and_mode_events() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","attributionSkill":"sk:review","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":10,"output_tokens":3,"speed":"fast"},"content":[{"type":"thinking","thinking":"…"},{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"m2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"plain"}]}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(
            collection.usage_events[0].attribution_skill.as_deref(),
            Some("sk:review")
        );
        assert_eq!(collection.usage_events[1].attribution_skill, None);
        assert_eq!(collection.mode_events.len(), 2);
        assert!(collection.mode_events[0].has_thinking);
        assert!(collection.mode_events[0].fast);
        assert!(!collection.mode_events[1].has_thinking);
        assert!(!collection.mode_events[1].fast);
    }

    #[test]
    fn collects_interrupt_events_from_marker_rows() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"i1","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"i1","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"user","uuid":"i2","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:02:00Z","sessionId":"s1","type":"user","uuid":"i3","interruptedMessageId":"m9","message":{"role":"user","content":"a plain follow-up"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:03:00Z","sessionId":"s1","type":"user","uuid":"i4","isMeta":true,"message":{"role":"user","content":"[Request interrupted by user] quoted in an agent report"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:04:00Z","sessionId":"s1","type":"user","uuid":"i5","message":{"role":"user","content":"the log said [Request interrupted by user] mid-sentence"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"i6","isSidechain":true,"message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:06:00Z","sessionId":"s1","type":"user","uuid":"i7","message":{"role":"user","content":"[Request interrupted by user] what does this marker mean?"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:07:00Z","sessionId":"s1","type":"user","uuid":"i9","message":{"role":"user","content":[{"type":"image","source":{"type":"base64"}},{"type":"text","text":"[Request interrupted by user]"}]}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");
        let subagent_dir = temp.path().join("project").join("subagents");
        fs::create_dir_all(&subagent_dir).expect("test dirs should be created");
        fs::write(
            subagent_dir.join("agent-abc.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:07:00Z","sessionId":"s1","type":"user","uuid":"i8","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n"
            ),
        )
        .expect("subagent fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.interrupt_events.len(), 2);
    }

    /// An interrupted turn is discarded from completion durations (matching
    /// Codex, where `turn_aborted` never reaches the durations), and the
    /// marker row does not start a bogus turn of its own.
    #[test]
    fn interrupted_turns_are_excluded_from_durations() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"do the thing"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"working"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:02:00Z","sessionId":"s1","type":"user","uuid":"i1","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"try again"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:06:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:07:00Z","sessionId":"s1","type":"user","uuid":"h3","message":{"role":"user","content":"thanks"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.interrupt_events.len(), 1);
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 60_000);
    }

    #[test]
    fn ask_user_question_answer_time_is_charged_to_human_wait() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"pick one"}}"#,
                "
",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}]}}"#,
                "
",
                r#"{"timestamp":"2026-07-20T00:11:00Z","sessionId":"s1","type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q1","content":"A"}]}}"#,
                "
",
                r#"{"timestamp":"2026-07-20T00:12:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "
",
                r#"{"timestamp":"2026-07-20T00:15:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"thanks"}}"#,
                "
"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(
            collection.duration_events[0].session_id.as_deref(),
            Some("s1")
        );
        let turn = &collection.duration_events[0];
        assert_eq!(turn.duration_ms, 720_000);
        assert_eq!(turn.human_wait_ms, 600_000);
        assert_eq!(turn.active_ms(), 120_000);
        assert_eq!(
            turn.timestamp,
            Some(time::macros::datetime!(2026-07-20 00:12 UTC))
        );
    }

    /// Rows landing between the question and the answer (reminders, hook
    /// attachments) must not shorten the wait, and two questions open at
    /// once charge their shared interval once.
    #[test]
    fn question_waits_ignore_intervening_rows_and_overlap() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"pick"}}"#,
                "
",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}},{"type":"tool_use","id":"q2","name":"AskUserQuestion","input":{}}]}}"#,
                "
",
                r#"{"timestamp":"2026-07-20T00:20:00Z","sessionId":"s1","type":"attachment","attachment":{"type":"total_tokens_reminder"}}"#,
                "
",
                r#"{"timestamp":"2026-07-20T00:41:00Z","sessionId":"s1","type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q1","content":"A"}]}}"#,
                "
",
                r#"{"timestamp":"2026-07-20T00:43:00Z","sessionId":"s1","type":"user","uuid":"r2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q2","content":"B"}]}}"#,
                "
",
                r#"{"timestamp":"2026-07-20T00:46:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "
",
                r#"{"timestamp":"2026-07-20T00:50:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"thanks"}}"#,
                "
"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 1);
        let turn = &collection.duration_events[0];
        assert_eq!(turn.duration_ms, 46 * 60_000);
        assert_eq!(turn.human_wait_ms, 42 * 60_000);
        assert_eq!(turn.active_ms(), 4 * 60_000);
    }

    /// A question answered after more than 30 minutes ended its turn when
    /// it was asked (the tail is waiting, not work), and the late answer is
    /// a fresh human input that starts the next turn — so the work after it
    /// is kept and no single wait exceeds the silence cutoff.
    #[test]
    fn answer_after_long_silence_starts_a_new_turn() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"pick"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:41:00Z","sessionId":"s2","type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q1","content":"x"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:46:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:50:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"thanks"}}"#,
                "\n",
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let mut turns: Vec<(u64, u64)> = collection
            .duration_events
            .iter()
            .map(|turn| (turn.duration_ms, turn.human_wait_ms))
            .collect();
        turns.sort_unstable();
        assert_eq!(turns, vec![(60_000, 0), (300_000, 0)]);
        let sessions: Vec<_> = collection
            .duration_events
            .iter()
            .map(|turn| turn.session_id.as_deref())
            .collect();
        assert_eq!(sessions, vec![Some("s1"), Some("s2")]);
    }

    /// The cutoff may be triggered by a reminder row rather than the answer
    /// itself; the open question must survive it so the late answer still
    /// starts the next turn. A second question still open when that turn
    /// starts charges its answer time from the new start.
    #[test]
    fn open_questions_survive_a_cutoff_by_another_row() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"pick"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}},{"type":"tool_use","id":"q2","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:35:00Z","sessionId":"s1","type":"attachment","attachment":{"type":"total_tokens_reminder"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:41:00Z","sessionId":"s1","type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q1","content":"A"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:43:00Z","sessionId":"s1","type":"user","uuid":"r2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q2","content":"B"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:46:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:50:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"thanks"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let mut turns: Vec<(u64, u64)> = collection
            .duration_events
            .iter()
            .map(|turn| (turn.duration_ms, turn.human_wait_ms))
            .collect();
        turns.sort_unstable();
        assert_eq!(turns, vec![(60_000, 0), (300_000, 120_000)]);
    }

    /// A replayed copy of the prompt row (same uuid) must not flush and
    /// restart the turn, or the question it left open is forgotten and its
    /// answer time counts as work.
    #[test]
    fn replayed_prompt_row_does_not_restart_the_turn() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let prompt = r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"pick"}}"#;
        fs::write(
            temp.path().join("session.jsonl"),
            [
                prompt,
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}]}}"#,
                prompt,
                r#"{"timestamp":"2026-07-20T00:11:00Z","sessionId":"s1","type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q1","content":"A"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:12:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:15:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"thanks"}}"#,
                "",
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 1);
        let turn = &collection.duration_events[0];
        assert_eq!(turn.duration_ms, 12 * 60_000);
        assert_eq!(turn.human_wait_ms, 10 * 60_000);
    }

    #[test]
    fn model_time_is_the_gaps_ending_in_assistant_rows() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"run it"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:04:00Z","sessionId":"s1","type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:08:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"thanks"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 1);
        let turn = &collection.duration_events[0];
        assert_eq!(turn.duration_ms, 5 * 60_000);
        assert_eq!(turn.model_ms, Some(2 * 60_000));
    }

    /// Model time never overlaps a human wait: an assistant row that lands
    /// while a question is still open adds nothing, so model + tools +
    /// waiting stays a partition of the turn.
    #[test]
    fn model_time_excludes_gaps_inside_a_human_wait() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"pick"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}},{"type":"tool_use","id":"q2","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:03:00Z","sessionId":"s1","type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q1","content":"A"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:04:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:06:00Z","sessionId":"s1","type":"user","uuid":"r2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q2","content":"B"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:07:00Z","sessionId":"s1","type":"user","uuid":"r3","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:08:00Z","sessionId":"s1","type":"assistant","message":{"id":"a3","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:10:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"thanks"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 1);
        let turn = &collection.duration_events[0];
        assert_eq!(turn.duration_ms, 8 * 60_000);
        assert_eq!(turn.human_wait_ms, 5 * 60_000);
        assert_eq!(turn.model_ms, Some(2 * 60_000));
    }

    /// A row Claude Code writes on its own continues the open turn: one
    /// delegation stays one turn, the wait for a background task is not the
    /// human's pace, and the row's permission mode is not a human turn's.
    #[test]
    fn generated_prompts_continue_the_open_turn() {
        let generated = [
            r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"g1","permissionMode":"auto","origin":{"kind":"task-notification"},"turnOrigin":"task_notification","promptSource":"system","message":{"role":"user","content":"<task-notification>\n<status>completed</status>\n</task-notification>"}}"#,
            r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"g1","permissionMode":"auto","turnOrigin":"peer","message":{"role":"user","content":"a message from another session"}}"#,
            r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"g1","permissionMode":"auto","origin":{"kind":"human"},"promptSource":"sdk","message":{"role":"user","content":"<scheduled-task name=\"daily\">run the job</scheduled-task>"}}"#,
            r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"g1","turnOrigin":"human","message":{"role":"user","content":"<bash-stdout>ok</bash-stdout><bash-stderr></bash-stderr>"}}"#,
            r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"g1","isCompactSummary":true,"isVisibleInTranscriptOnly":true,"message":{"role":"user","content":"summary of the earlier conversation"}}"#,
            r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"g1","message":{"role":"user","content":[{"type":"text","text":"<task-notification>\n<status>completed</status>\n</task-notification>"}]}}"#,
        ];
        for row in generated {
            let temp = TempDir::new().expect("test tempdir should be created");
            fs::write(
                temp.path().join("session.jsonl"),
                [
                    r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","permissionMode":"auto","origin":{"kind":"human"},"turnOrigin":"human","promptSource":"typed","message":{"role":"user","content":"start a background job"}}"#,
                    r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"started"}]}}"#,
                    row,
                    r#"{"timestamp":"2026-07-20T00:06:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                    r#"{"timestamp":"2026-07-20T00:08:00Z","sessionId":"s1","type":"user","uuid":"h2","permissionMode":"auto","origin":{"kind":"human"},"promptSource":"typed","message":{"role":"user","content":"thanks"}}"#,
                    "",
                ]
                .join("\n"),
            )
            .expect("fixture should be written");

            let collection = collect(temp.path(), None, false, UtcOffset::UTC);

            let durations: Vec<u64> = collection
                .duration_events
                .iter()
                .map(|turn| turn.duration_ms)
                .collect();
            assert_eq!(durations, vec![6 * 60_000], "{row}");
            let gaps: Vec<u64> = collection.pace_events.iter().map(|e| e.gap_ms).collect();
            assert_eq!(gaps, vec![2 * 60_000], "{row}");
            assert_eq!(collection.permission_events.len(), 2, "{row}");
        }
    }

    fn turn_shapes(collection: &Collection) -> Vec<(u64, &str)> {
        let mut shapes: Vec<(u64, &str)> = collection
            .duration_events
            .iter()
            .map(|turn| (turn.duration_ms, turn.status.as_deref().unwrap_or("")))
            .collect();
        shapes.sort_unstable();
        shapes
    }

    /// A session opened by a scheduled run keeps the run's working time as a
    /// generated turn; the run adds no pace sample or permission mode.
    #[test]
    fn scheduled_session_start_opens_a_generated_turn() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            [
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"g1","permissionMode":"auto","origin":{"kind":"human"},"promptSource":"sdk","message":{"role":"user","content":"<scheduled-task name=\"daily\">run the job</scheduled-task>"}}"#,
                r#"{"timestamp":"2026-07-20T00:04:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"working"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:10:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:15:00Z","sessionId":"s1","type":"user","uuid":"h1","permissionMode":"auto","origin":{"kind":"human"},"promptSource":"typed","message":{"role":"user","content":"looks good, one more"}}"#,
                r#"{"timestamp":"2026-07-20T00:16:00Z","sessionId":"s1","type":"assistant","message":{"id":"a3","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:20:00Z","sessionId":"s1","type":"user","uuid":"h2","permissionMode":"auto","origin":{"kind":"human"},"promptSource":"typed","message":{"role":"user","content":"thanks"}}"#,
                "",
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(
            turn_shapes(&collection),
            vec![(60_000, "turn"), (10 * 60_000, "generated")]
        );
        let mut gaps: Vec<u64> = collection.pace_events.iter().map(|e| e.gap_ms).collect();
        gaps.sort_unstable();
        assert_eq!(gaps, vec![4 * 60_000, 5 * 60_000]);
        assert_eq!(collection.permission_events.len(), 2);
    }

    /// A notification arriving after a long background job (past the silence
    /// cutoff) opens a generated turn, so the rest of the delegation keeps
    /// its working time, whether a queue row or the notification itself
    /// trips the cutoff.
    #[test]
    fn notification_after_a_long_gap_opens_a_generated_turn() {
        let notification = r#"{"timestamp":"2026-07-20T00:45:00Z","sessionId":"s1","type":"user","uuid":"g1","permissionMode":"auto","origin":{"kind":"task-notification"},"turnOrigin":"task_notification","promptSource":"system","message":{"role":"user","content":"<task-notification>\n<status>completed</status>\n</task-notification>"}}"#;
        let queued = r#"{"timestamp":"2026-07-20T00:45:00Z","sessionId":"s1","type":"queue-operation","operation":"enqueue"}"#;
        for lead in [Some(queued), None] {
            let temp = TempDir::new().expect("test tempdir should be created");
            let mut rows = vec![
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","permissionMode":"auto","origin":{"kind":"human"},"promptSource":"typed","message":{"role":"user","content":"run the long job in the background"}}"#,
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"started"}]}}"#,
            ];
            rows.extend(lead);
            rows.extend([
                notification,
                r#"{"timestamp":"2026-07-20T00:50:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:55:00Z","sessionId":"s1","type":"user","uuid":"h2","permissionMode":"auto","origin":{"kind":"human"},"promptSource":"typed","message":{"role":"user","content":"thanks"}}"#,
                "",
            ]);
            fs::write(temp.path().join("session.jsonl"), rows.join("\n"))
                .expect("fixture should be written");

            let collection = collect(temp.path(), None, false, UtcOffset::UTC);

            assert_eq!(
                turn_shapes(&collection),
                vec![(60_000, "turn"), (5 * 60_000, "generated")],
                "{lead:?}"
            );
            let gaps: Vec<u64> = collection.pace_events.iter().map(|e| e.gap_ms).collect();
            assert_eq!(gaps, vec![5 * 60_000], "{lead:?}");
            assert_eq!(collection.permission_events.len(), 2, "{lead:?}");
        }
    }

    /// A generated prompt replayed after the turn it continued was discarded
    /// (resume writes history back with the original stamp) must not open a
    /// turn of its own.
    #[test]
    fn replayed_generated_prompt_does_not_open_a_turn() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let notification = r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"g1","origin":{"kind":"task-notification"},"message":{"role":"user","content":"<task-notification>\n<status>completed</status>\n</task-notification>"}}"#;
        fs::write(
            temp.path().join("session.jsonl"),
            [
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"start a background job"}}"#,
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"started"}]}}"#,
                notification,
                r#"{"timestamp":"2026-07-20T00:06:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:07:00Z","sessionId":"s1","type":"user","uuid":"i1","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                notification,
                r#"{"timestamp":"2026-07-20T00:08:00Z","sessionId":"s1","type":"assistant","message":{"id":"a3","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"resumed"}]}}"#,
                "",
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert!(turn_shapes(&collection).is_empty());
    }

    /// A manual compaction is the model's work without an assistant row: it
    /// stays a turn, and where the typed command is echoed after the
    /// summary, the compaction still counts once.
    #[test]
    fn manual_compaction_counts_once_as_a_turn() {
        let command = r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"c1","message":{"role":"user","content":"<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args></command-args>"}}"#;
        let stdout = r#"{"timestamp":"2026-07-20T00:02:00Z","sessionId":"s1","type":"user","uuid":"c2","message":{"role":"user","content":"<local-command-stdout>Compacted</local-command-stdout>"}}"#;
        let next = r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"continue"}}"#;
        let echoed = [
            r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"t1","message":{"role":"user","content":"/compact"}}"#,
            r#"{"timestamp":"2026-07-20T00:02:00Z","sessionId":"s1","type":"system","subtype":"compact_boundary"}"#,
            r#"{"timestamp":"2026-07-20T00:02:00Z","sessionId":"s1","type":"user","uuid":"s1","isCompactSummary":true,"message":{"role":"user","content":"summary of the earlier conversation"}}"#,
            command,
            stdout,
            next,
            "",
        ];
        for rows in [vec![command, stdout, next, ""], echoed.to_vec()] {
            let temp = TempDir::new().expect("test tempdir should be created");
            fs::write(temp.path().join("session.jsonl"), rows.join("\n"))
                .expect("fixture should be written");

            let collection = collect(temp.path(), None, false, UtcOffset::UTC);

            assert_eq!(turn_shapes(&collection), vec![(2 * 60_000, "turn")]);
        }
    }

    /// A local slash command and a prompt cancelled before the first reply
    /// start like prompts, but the model never answers: neither is a turn.
    #[test]
    fn turns_without_a_model_response_are_not_counted() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            [
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"do"}}"#,
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:03:00Z","sessionId":"s1","type":"user","uuid":"l1","message":{"role":"user","content":"<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"}}"#,
                r#"{"timestamp":"2026-07-20T00:03:01Z","sessionId":"s1","type":"user","uuid":"l2","message":{"role":"user","content":"<local-command-stdout>Set model</local-command-stdout>"}}"#,
                r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"never mind"}}"#,
                r#"{"timestamp":"2026-07-20T00:05:10Z","sessionId":"s1","type":"attachment","attachment":{"type":"total_tokens_reminder"}}"#,
                r#"{"timestamp":"2026-07-20T00:05:30Z","sessionId":"s1","type":"user","uuid":"h3","message":{"role":"user","content":"next"}}"#,
                r#"{"timestamp":"2026-07-20T00:07:30Z","sessionId":"s1","type":"assistant","message":{"id":"a3","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:09:00Z","sessionId":"s1","type":"user","uuid":"h4","message":{"role":"user","content":"thanks"}}"#,
                "",
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let mut durations: Vec<u64> = collection
            .duration_events
            .iter()
            .map(|turn| turn.duration_ms)
            .collect();
        durations.sort_unstable();
        assert_eq!(durations, vec![60_000, 2 * 60_000]);
    }

    /// A request-log row replays tool calls from the request history with
    /// no timestamp; a call whose transcript row is gone must not reappear
    /// from the replay.
    #[test]
    fn request_log_rows_add_no_tool_calls() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            [
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"api-request","id":"r1","querySource":"repl_main_thread","params":{"model":"claude-fable-5"}}"#,
                r#"{"type":"api-request-blob","hash":"b1","sessionId":"s1","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_gone","name":"Read","input":{}},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]}}"#,
                r#"{"timestamp":"2026-07-20T00:00:05Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}]}}"#,
                "",
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.tool_events[0].tool_name, "Bash");
        assert!(collection.tool_events[0].timestamp.is_some());
    }

    #[test]
    fn pace_is_the_gap_before_a_prompt_under_the_cutoff() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"do"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:03:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"more"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:04:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:50:00Z","sessionId":"s1","type":"user","uuid":"h3","message":{"role":"user","content":"back"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let gaps: Vec<u64> = collection.pace_events.iter().map(|e| e.gap_ms).collect();
        assert_eq!(gaps, vec![120_000]);
    }

    /// A completed earlier turn replayed in the middle of a later one
    /// (resume writes history back) must leave the later turn untouched:
    /// its prompt, question and answer are all inert on second sight.
    #[test]
    fn replayed_earlier_turn_does_not_contaminate_the_current_one() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let h1 = r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"pick"}}"#;
        let ask = r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}]}}"#;
        let answer = r#"{"timestamp":"2026-07-20T00:11:00Z","sessionId":"s1","type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q1","content":"A"}]}}"#;
        fs::write(
            temp.path().join("session.jsonl"),
            [
                h1,
                ask,
                answer,
                r#"{"timestamp":"2026-07-20T00:12:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:15:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"next"}}"#,
                h1,
                ask,
                answer,
                r#"{"timestamp":"2026-07-20T00:20:00Z","sessionId":"s1","type":"assistant","message":{"id":"a3","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done again"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:25:00Z","sessionId":"s1","type":"user","uuid":"h3","message":{"role":"user","content":"thanks"}}"#,
                "",
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let mut turns: Vec<(u64, u64)> = collection
            .duration_events
            .iter()
            .map(|turn| (turn.duration_ms, turn.human_wait_ms))
            .collect();
        turns.sort_unstable();
        assert_eq!(turns, vec![(300_000, 0), (720_000, 600_000)]);
    }

    /// A replayed copy of the question row (resume / fork prefix) must not
    /// reopen a question already answered, or the tail would be charged as
    /// waiting again.
    #[test]
    fn replayed_question_row_does_not_reopen_an_answered_question() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let ask = r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}]}}"#;
        fs::write(
            temp.path().join("session.jsonl"),
            [
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"pick"}}"#,
                ask,
                r#"{"timestamp":"2026-07-20T00:11:00Z","sessionId":"s1","type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q1","content":"A"}]}}"#,
                r#"{"timestamp":"2026-07-20T00:12:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                ask,
                "",
            ]
            .join("\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 1);
        let turn = &collection.duration_events[0];
        assert_eq!(turn.duration_ms, 12 * 60_000);
        assert_eq!(turn.human_wait_ms, 10 * 60_000);
    }

    #[test]
    fn staggered_questions_charge_the_interval_union() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"pick"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:05:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q2","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:10:00Z","sessionId":"s1","type":"user","uuid":"r2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q2","content":"x"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:15:00Z","sessionId":"s1","type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"q1","content":"x"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:16:00Z","sessionId":"s1","type":"assistant","message":{"id":"a3","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:20:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"thanks"}}"#,
                "\n",
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 1);
        let turn = &collection.duration_events[0];
        assert_eq!(turn.duration_ms, 16 * 60_000);
        assert_eq!(turn.human_wait_ms, 14 * 60_000);
    }

    #[test]
    fn unanswered_question_charges_its_tail_as_wait() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"pick"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:10:00Z","sessionId":"s1","type":"attachment","attachment":{"type":"total_tokens_reminder"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:12:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"never mind"}}"#,
                "\n",
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.duration_events.len(), 1);
        let turn = &collection.duration_events[0];
        assert_eq!(turn.duration_ms, 10 * 60_000);
        assert_eq!(turn.human_wait_ms, 9 * 60_000);
        assert_eq!(turn.active_ms(), 60_000);
    }

    /// A fork child replays the parent's history: the copied turn shares
    /// the prompt uuid and must not count twice — and when the copy is only
    /// a prefix (the fork happened mid-turn), the complete observation wins
    /// whichever file is scanned first.
    #[test]
    fn replayed_turns_dedupe_by_prompt_uuid_keeping_the_complete_one() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let prefix = concat!(
            r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"do"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"working"}]}}"#,
            "\n",
        );
        let full = concat!(
            r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"do"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"working"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-07-20T00:12:00Z","sessionId":"s1","type":"assistant","message":{"id":"a2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"done"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-07-20T00:15:00Z","sessionId":"s1","type":"user","uuid":"h2","message":{"role":"user","content":"ok"}}"#,
            "\n",
        );
        for (parent, child) in [
            ("a-parent.jsonl", "b-child.jsonl"),
            ("b-parent.jsonl", "a-child.jsonl"),
        ] {
            let dir = temp.path().join(parent.split('-').next().unwrap_or("x"));
            fs::create_dir_all(&dir).expect("dir");
            fs::write(dir.join(parent), full).expect("fixture");
            fs::write(dir.join(child), prefix).expect("fixture");

            let collection = collect(&dir, None, false, UtcOffset::UTC);

            assert_eq!(collection.duration_events.len(), 1, "{parent}");
            let turn = &collection.duration_events[0];
            assert_eq!(turn.duration_ms, 12 * 60_000, "{parent}");
            assert_eq!(
                turn.timestamp,
                Some(time::macros::datetime!(2026-07-20 00:12 UTC)),
                "{parent}"
            );
        }
    }

    /// A marker without a timestamp still clears the active turn: the abort
    /// must not flush as a completed duration at the next prompt or EOF.
    #[test]
    fn undated_marker_still_discards_the_aborted_turn() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"h1","message":{"role":"user","content":"do the thing"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","message":{"id":"a1","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"working"}]}}"#,
                "\n",
                r#"{"sessionId":"s1","type":"user","uuid":"i1","message":{"role":"user","content":"[Request interrupted by user]"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.interrupt_events.len(), 1);
        assert!(collection.interrupt_events[0].timestamp.is_none());
        assert_eq!(collection.duration_events.len(), 0);
    }

    #[test]
    fn collects_permission_events_deduped_by_uuid() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"u1","permissionMode":"dontAsk","message":{"role":"user","content":"do it"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"user","uuid":"u1","permissionMode":"dontAsk","message":{"role":"user","content":"do it"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"user","uuid":"u2","permissionMode":"auto","message":{"role":"user","content":"next"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:02:00Z","sessionId":"s1","type":"user","uuid":"u3","message":{"role":"user","content":"no mode field"}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:03:00Z","sessionId":"s1","type":"user","uuid":"u4","isMeta":true,"permissionMode":"bypassPermissions","message":{"role":"user","content":"agent-message injection"}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let mut modes: Vec<&str> = collection
            .permission_events
            .iter()
            .map(|event| event.mode.as_str())
            .collect();
        modes.sort_unstable();
        assert_eq!(modes, ["auto", "dontAsk"]);
    }

    #[test]
    fn collects_effort_events_deduped_by_message_id() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-07-20T00:00:00Z","sessionId":"s1","type":"assistant","effort":"xhigh","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":10,"output_tokens":3},"content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:00:01Z","sessionId":"s1","type":"assistant","effort":"xhigh","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":10,"output_tokens":3},"content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-07-20T00:01:00Z","sessionId":"s1","type":"assistant","effort":"max","message":{"id":"m2","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"deep"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m3","model":"claude-fable-5","usage":{"input_tokens":5,"output_tokens":2},"content":[{"type":"text","text":"old CLI"}]}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let mut efforts: Vec<&str> = collection
            .effort_events
            .iter()
            .map(|event| event.effort.as_str())
            .collect();
        efforts.sort_unstable();
        assert_eq!(efforts, ["max", "xhigh"]);
    }

    /// Streaming duplicates of one message can disagree: the larger-volume
    /// line may lack attribution while a smaller fragment carries it. The
    /// winner keeps its tokens but absorbs the loser's metadata (fill), and
    /// mode flags merge with OR.
    #[test]
    fn duplicate_lines_fill_metadata_instead_of_dropping_it() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":100,"output_tokens":50},"content":[{"type":"text","text":"big"}]}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:01Z","sessionId":"s1","type":"assistant","attributionSkill":"sk:review","message":{"id":"m1","model":"claude-fable-5","usage":{"input_tokens":1,"output_tokens":1},"content":[{"type":"thinking","thinking":"…"}]}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.input_tokens, 100);
        assert_eq!(
            collection.usage_events[0].attribution_skill.as_deref(),
            Some("sk:review")
        );
        assert_eq!(collection.mode_events.len(), 1);
        assert!(collection.mode_events[0].has_thinking);
    }

    #[test]
    fn skips_malformed_lines_without_aborting() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            "not-json\n{\"timestamp\":\"2026-06-01T00:00:00Z\",\"sessionId\":\"s1\",\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-4-8\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n",
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(collection.usage_events.len(), 1);
    }

    #[test]
    fn deduplicates_usage_and_tool_use_ids() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let line = r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","type":"assistant","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30},"content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/secret"}}]}}"#;
        fs::write(
            temp.path().join("session.jsonl"),
            format!("{line}\n{line}\n"),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 63);
    }

    #[test]
    fn skips_files_older_than_mtime_floor() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("old.jsonl"),
            r#"{"timestamp":"2026-01-01T00:00:00Z","sessionId":"old","type":"assistant","message":{"id":"m0","usage":{"input_tokens":1,"output_tokens":1}}}"#,
        )
        .expect("fixture should be written");

        let future = SystemTime::now() + std::time::Duration::from_hours(1);
        let collection = collect(temp.path(), Some(future), false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 0);
        assert!(collection.usage_events.is_empty());
    }
}
