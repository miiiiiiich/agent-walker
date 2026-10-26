use std::collections::HashSet;

use serde_json::Value;
use time::{Duration, OffsetDateTime};

use crate::collector::{
    FileEvents, KeyedDurationEvent, KeyedEffortEvent, KeyedInterruptEvent, KeyedPaceEvent,
    KeyedPermissionEvent,
};
use crate::model::{DurationEvent, EffortEvent, InterruptEvent, PaceEvent, PermissionEvent};

use super::usage::positional_key;
use super::{string_path, u64_path};

/// Key by `turn_id`, which fork replays copy verbatim — the replayed
/// `turn_context` collapses with its original despite the rewritten timestamp
/// (GH-36). Logs predating `turn_id` fall back to the positional key.
pub(super) fn collect_effort_event(
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
pub(super) fn collect_interrupt_event(
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

pub(super) fn collect_permission_event(
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
pub(super) struct PaceState {
    pending_completion: Option<OffsetDateTime>,
    newest_completion: Option<OffsetDateTime>,
}

/// The human's pace: `task_complete` → the next `user_message`, when under
/// the 30-minute cutoff (longer is the human being away). Keyed by session
/// and prompt time so a replayed prompt row doesn't count twice.
pub(super) fn collect_pace_event(
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
pub(super) struct TurnItems {
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

    pub(super) fn note(&mut self, value: &Value, timestamp: Option<OffsetDateTime>) {
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

pub(super) fn collect_duration_event(
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use time::UtcOffset;

    use crate::collector::codex::collect;

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
}
