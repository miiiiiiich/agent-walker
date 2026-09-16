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
    // Claude Code names its project directories by flattening the cwd with
    // every path separator replaced by `-`. Strip the home prefix so the label
    // is a relative project path. Exact on macOS / Linux. Windows-native
    // Claude Code's flattening rule isn't confirmed (especially whether the
    // drive-letter `:` is dropped or rewritten), so we sanitize the home like
    // the directory name itself — replacing `:` as well as the separators —
    // and trim any trailing separator first so a home such as `/home/me/` or
    // `D:\` doesn't yield a double-hyphen prefix. When the flattened prefix
    // still doesn't match we fall back to the leading-`-` trim; the cwd field
    // in the session is preferred as the project label whenever present.
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
    // Claude logs carry no explicit completion event; derive turn durations
    // as "human prompt -> last activity before the next human prompt".
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
                // The active turn was aborted: discard it (completion stats
                // count completed turns only, matching Codex where
                // `turn_aborted` never reaches the durations) and don't
                // start a bogus turn from the marker row itself. Recognized
                // before requiring a timestamp — an undated marker must
                // still clear the turn, or the abort would flush as a
                // completed duration at the next prompt or EOF.
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
                        turn = TurnState {
                            start: Some(timestamp),
                            session_id: session_id_field(&value),
                            key,
                            last_activity: Some(timestamp),
                            ..TurnState::default()
                        };
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
                        turn.restart_if_answer(&value, timestamp);
                    } else if turn.charge_answer(&value, timestamp) {
                        turn.last_activity = Some(previous.max(timestamp));
                    } else {
                        // An assistant row while a question is still open
                        // (a replay, or a parallel tool call answered first)
                        // is inside the human's wait, not the model's time.
                        if value.get("type").and_then(Value::as_str) == Some("assistant")
                            && timestamp > previous
                            && turn.pending_questions.is_empty()
                        {
                            let gap = u64::try_from((timestamp - previous).whole_milliseconds())
                                .unwrap_or(0);
                            turn.model_ms = turn.model_ms.saturating_add(gap);
                        }
                        turn.note_questions(&value, timestamp, &mut seen_questions);
                        turn.last_activity = Some(previous.max(timestamp));
                    }
                } else {
                    // Between turns after a cutoff: the only row that
                    // matters is a late answer to a question still open.
                    turn.restart_if_answer(&value, timestamp);
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

/// The turn being assembled: its prompt, the last activity seen, and the
/// `AskUserQuestion` bookkeeping that separates the human's answer time
/// from the agent's working time.
#[derive(Default)]
struct TurnState {
    start: Option<OffsetDateTime>,
    /// The prompt row's session, so JSON consumers can join turns to sessions.
    session_id: Option<String>,
    /// The prompt row's uuid — fork children replay the parent's history,
    /// so a turn copied into a child file must dedupe against the original.
    key: Option<String>,
    last_activity: Option<OffsetDateTime>,
    human_wait_ms: u64,
    /// Time the model was thinking / writing: the gaps that end in an
    /// assistant row (the rest of a turn is tools running).
    model_ms: u64,
    /// Pending question tool-call ids and when they were asked.
    pending_questions: HashMap<String, OffsetDateTime>,
    /// End of the last charged wait, so overlapping questions answered in
    /// sequence charge their shared interval once.
    wait_cursor: Option<OffsetDateTime>,
}

impl TurnState {
    /// Tool-result ids on a user row that answer a pending question.
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

    /// The state between turns once the cutoff closed this one: no turn in
    /// progress, but questions left open carry over so a late answer can
    /// be recognized and start the next turn.
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

    /// If the row answers pending questions, charge the wait up to it (see
    /// `waited_until`), retire the answered ids, and report `true`. Rows
    /// landing between the question and the answer (reminders, hook
    /// attachments) never shorten the wait: it is measured from the ask.
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

    /// Emit the completed turn (if any): stamped at its END like every other
    /// provider's turn, keyed by the prompt uuid for cross-file dedup, with
    /// the human's answer time clamped to the turn length. A question still
    /// open at the end charges its tail as waiting — the agent stopped
    /// working when it asked.
    fn flush(&self, events: &mut FileEvents) {
        let (Some(start), Some(end)) = (self.start, self.last_activity) else {
            return;
        };
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
                status: Some("turn".to_owned()),
            },
        });
    }
}

/// Ids of `AskUserQuestion` tool calls on an assistant row — the questions
/// whose answers will land as tool results later in the same turn.
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
            },
        });
    }

    // `usage.iterations` (log-schema addition, 2026-04) breaks one turn into
    // its underlying API calls. The top level is the turn's BILLED usage for
    // the serving model: a failed fallback attempt is not billed (fallback
    // credit refunds the switch) and the turn is billed as the serving model
    // alone, and on advisor turns the top level already sums the main-model
    // iterations. So main-model `message` and `fallback_message` entries must
    // never be re-emitted — only `advisor_message` entries are additional
    // billed calls, made under their own model and absent from the top-level
    // counters (ccusage#1115 lost them entirely). Keyed per iteration index
    // so streamed duplicates of the message still dedupe.
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

/// One mode event per assistant message (keyed by message id): did extended
/// thinking fire (a `thinking` content block exists — presence only, the text
/// is never read), and did fast mode serve it (`usage.speed == "fast"`).
/// Streaming duplicates of the same message merge with OR in `merge_into`.
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

/// A main-thread row the harness writes when the user hits esc: a user row
/// whose content IS one of the two complete marker forms the harness
/// emits (the only variants across real logs) — a prompt that merely
/// quotes or starts with the marker text must not count or clear a turn.
/// `isMeta` rows are excluded because agent messages QUOTING the marker
/// would otherwise count. `isSidechain` rows are excluded because one esc
/// against a parallel team fans out as marker echoes into every subagent
/// transcript (bursts of 10-16 observed — counting them would overstate
/// interruptions ~1.8x, load-dependently). The trade-off: an esc recorded
/// only in sidechain files (~14% of esc moments) is deliberately not
/// counted — the same turn-level ruling as Codex, where `turn_aborted`
/// is used and `sub_agent_activity: interrupted` is discarded.
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

/// One interrupt event per main-thread esc (`interruptedMessageId` rows are
/// a strict subset of marker rows, so the marker alone carries the count).
/// Subagent-file rows are excluded by file provenance too, not only by the
/// row's `isSidechain` flag. Keyed by the row uuid, which resume/fork
/// copies share.
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

/// One effort event per assistant message (keyed by message id), from the
/// top-level `effort` field Claude Code writes since v2.1.212 (2026-07-17).
/// Older lines lack the field and contribute nothing; subagent (sidechain)
/// messages carry it too, so the mix covers delegated turns.
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
            r#"{"timestamp":"2026-06-01T00:00:00Z","sessionId":"s1","message":{"id":"m1","model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":3,"cache_creation_input_tokens":20,"cache_read_input_tokens":30},"content":[{"type":"tool_use","name":"Agent","input":{"subagent_type":"Explore"}},{"type":"tool_use","name":"Read","input":{"file_path":"/secret"}}]}}"#,
        )
        .expect("main fixture should be written");
        fs::write(
            subagent_dir.join("agent-abc.jsonl"),
            r#"{"timestamp":"2026-06-01T00:01:00Z","sessionId":"s1","isSidechain":true,"attributionAgent":"Explore","message":{"id":"m2","model":"claude-haiku-4-5","usage":{"input_tokens":5,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":10},"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo hidden"}}]}}"#,
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

    /// The ordinary shape — one main-model `message` iteration mirroring the
    /// top-level numbers — must not create a second event.
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
        // One mode event per assistant message: thinking+fast, then neither.
        assert_eq!(collection.mode_events.len(), 2);
        assert!(collection.mode_events[0].has_thinking);
        assert!(collection.mode_events[0].fast);
        assert!(!collection.mode_events[1].has_thinking);
        assert!(!collection.mode_events[1].fast);
    }

    /// Interrupt markers count once per esc: duplicate uuids (resume/fork
    /// copies) dedupe, block-content markers count, and a row carrying only
    /// `interruptedMessageId` without the marker does not count (real logs
    /// show such rows always carry the marker too).
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
        // A subagent-file echo that omits the redundant `isSidechain` flag:
        // file provenance alone must exclude it.
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
        // Only the completed turn survives ("try again" 00:05 -> last
        // activity "done" 00:06 = 60s); the aborted first turn and the
        // marker row contribute no durations.
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 60_000);
    }

    /// The time between an `AskUserQuestion` call and its answer is the
    /// human's, not the agent's: it stays inside the turn length but is
    /// charged to `human_wait_ms`, so `active_ms` excludes it.
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

        // One completed turn: 00:00 -> last activity 00:12 = 12 min, of
        // which the 10 min between the question and the answer is the
        // human's. The turn is stamped at its end, like Codex / Copilot.
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

        // 00:00 -> 00:46 = 46 min. Waits: q1 00:01 -> 00:41 (40 min), then
        // q2 from the cursor 00:41 -> 00:43 (2 min), not 00:01 -> 00:43.
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
        // 00:00 -> 00:01 (the ask ends it, nothing to wait for yet) and
        // 00:41 -> 00:46 (the answer starts it, all work).
        assert_eq!(turns, vec![(60_000, 0), (300_000, 0)]);
        // The restarted turn belongs to the session of the row that started it.
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
        // 00:00 -> 00:01, then 00:41 -> 00:46 of which 00:41 -> 00:43 was
        // spent answering q2.
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

    /// The gaps that end in an assistant row are the model working; the
    /// gap from a tool call to its result is the tool running.
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
        // 5 min turn: 1 + 1 min model, 3 min Bash.
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
        // 8 min turn: waiting 00:01→00:03 + 00:03→00:06 = 5, model 00:00→00:01
        // + 00:07→00:08 = 2 (the 00:03→00:04 assistant row sits inside the
        // q2 wait), tools = the remaining 1.
        assert_eq!(turn.duration_ms, 8 * 60_000);
        assert_eq!(turn.human_wait_ms, 5 * 60_000);
        assert_eq!(turn.model_ms, Some(2 * 60_000));
    }

    /// The gap from a turn's last activity to the next prompt is the
    /// human's pace, keyed by the prompt uuid; a prompt after the 30-minute
    /// cutoff is the human coming back, not their pace.
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

        // h2 came 2 min after a1; h3 came 46 min after a2 (cutoff, not pace).
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
        // h2 (00:15 -> 00:20) is five working minutes; h1's replayed
        // question and answer charge nothing to it.
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

    /// Staggered questions answered out of order charge the union of their
    /// intervals: the human was busy from the first ask to the last answer.
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
        // [00:01, 00:15] = 14 min, not 5 + 5.
        assert_eq!(turn.human_wait_ms, 14 * 60_000);
    }

    /// A question still open when the turn is replaced (or the file ends)
    /// charges its tail as waiting: the agent stopped working when it asked.
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

        // The undated marker yields an event the analyzer will exclude by
        // date, and no duration: EOF must not flush the aborted turn.
        assert_eq!(collection.interrupt_events.len(), 1);
        assert!(collection.interrupt_events[0].timestamp.is_none());
        assert_eq!(collection.duration_events.len(), 0);
    }

    /// The top-level `permissionMode` field on user rows becomes one
    /// permission event per turn; resume/fork copies share the row uuid and
    /// dedupe, and rows without the field contribute nothing.
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

    /// The top-level `effort` field (present since CLI v2.1.212) becomes one
    /// effort event per assistant message; duplicate lines for the same
    /// message dedupe by id, and lines without the field contribute nothing.
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

        // One deduped usage event: the big line's tokens, the small line's skill.
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.input_tokens, 100);
        assert_eq!(
            collection.usage_events[0].attribution_skill.as_deref(),
            Some("sk:review")
        );
        // One deduped mode event with OR-merged thinking.
        assert_eq!(collection.mode_events.len(), 1);
        assert!(collection.mode_events[0].has_thinking);
    }

    #[test]
    fn skips_malformed_lines_without_aborting() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(
            temp.path().join("session.jsonl"),
            "not-json\n{\"timestamp\":\"2026-06-01T00:00:00Z\",\"sessionId\":\"s1\",\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-4-8\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n",
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

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
            r#"{"timestamp":"2026-01-01T00:00:00Z","sessionId":"old","message":{"id":"m0","usage":{"input_tokens":1,"output_tokens":1}}}"#,
        )
        .expect("fixture should be written");

        let future = SystemTime::now() + std::time::Duration::from_hours(1);
        let collection = collect(temp.path(), Some(future), false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 0);
        assert!(collection.usage_events.is_empty());
    }
}
