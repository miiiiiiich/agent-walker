use serde_json::Value;
use time::{Duration, OffsetDateTime};

use crate::model::SourceKind;

use super::string_path;

/// A `history_base` fork has no copied burst; skipping its first second would
/// drop its own rows.
pub(super) fn is_copied_fork(session_meta: &Value) -> bool {
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
pub(super) fn in_fork_burst(fork: OffsetDateTime, at: OffsetDateTime) -> bool {
    at.unix_timestamp() >= fork.unix_timestamp() && at - fork < Duration::SECOND
}

/// Read from the first `session_meta` only: a fork child's second one is the
/// parent's copy and would make the child a main session.
#[derive(Default)]
pub(super) struct Origin {
    pub(super) agent: Option<String>,
}

impl Origin {
    pub(super) fn from_session_meta(session_meta: &Value) -> Self {
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

    pub(super) fn source_kind(&self) -> SourceKind {
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use time::UtcOffset;
    use time::format_description::well_known::Rfc3339;

    use super::*;
    use crate::collector::codex::collect;
    use crate::collector::codex::tests::{
        meta_line, record_line, token_count_line, total_volume, usage_json, write_lines,
    };
    use crate::model::Collection;

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
