use std::collections::{HashMap, HashSet};

use serde_json::Value;
use time::OffsetDateTime;

use crate::collector::{FileEvents, KeyedDurationEvent};
use crate::model::DurationEvent;

use super::{session_id_field, string_field};

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
pub(in crate::collector::claude) fn is_human_turn(value: &Value) -> bool {
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
pub(in crate::collector::claude) struct TurnState {
    pub(in crate::collector::claude) start: Option<OffsetDateTime>,
    session_id: Option<String>,
    /// The prompt row's uuid — fork children replay the parent's history,
    /// so a turn copied into a child file must dedupe against the original.
    key: Option<String>,
    pub(in crate::collector::claude) last_activity: Option<OffsetDateTime>,
    human_wait_ms: u64,
    pub(in crate::collector::claude) model_ms: u64,
    pub(in crate::collector::claude) pending_questions: HashMap<String, OffsetDateTime>,
    /// End of the last charged wait, so overlapping questions answered in
    /// sequence charge their shared interval once.
    wait_cursor: Option<OffsetDateTime>,
    /// A local slash command, or a prompt cancelled before the first reply,
    /// opens a turn the model never answers; without this, its output and
    /// reminder rows count as a short turn of agent work. A manual compaction
    /// runs the model without writing an assistant row, so its command row
    /// counts as answered.
    pub(in crate::collector::claude) responded: bool,
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
    pub(in crate::collector::claude) fn note_questions(
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

    pub(in crate::collector::claude) fn after_cutoff(self) -> Self {
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
    pub(in crate::collector::claude) fn open_from(
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
    pub(in crate::collector::claude) fn open_if_generated(
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

    pub(in crate::collector::claude) fn opened_at(
        value: &Value,
        now: OffsetDateTime,
        key: Option<String>,
    ) -> Self {
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
    pub(in crate::collector::claude) fn charge_answer(
        &mut self,
        value: &Value,
        now: OffsetDateTime,
    ) -> bool {
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
    pub(in crate::collector::claude) fn flush(&self, events: &mut FileEvents) {
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use time::UtcOffset;

    use crate::collector::claude::collect;
    use crate::model::Collection;

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
}
