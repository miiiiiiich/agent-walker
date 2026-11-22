use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::collector::ident::ident;
use crate::collector::{FileEvents, KeyedUsageEvent};
use crate::model::{SourceKind, UsageEvent, UsageGranularity};

use super::{Line, RawCounters, u64_field};

/// `session.usage_record` receipts read so far in one session's stream, and
/// those no snapshot has covered yet.
#[derive(Default)]
pub(super) struct Receipts {
    pending: Vec<Pending>,
    seen: HashSet<String>,
    index: usize,
}

struct Pending {
    model: String,
    counters: RawCounters,
    identity: Option<(String, u64)>,
}

impl Receipts {
    /// Keyed by the accounting identity, so a fork's copy collapses into the
    /// parent's original.
    pub(super) fn collect(
        &mut self,
        value: &Value,
        line: &Line,
        project: Option<&str>,
        events: &mut FileEvents,
    ) {
        let Some(usage) = value.pointer("/data/usage") else {
            return;
        };
        let Some(model) = usage.get("model").and_then(Value::as_str).and_then(ident) else {
            return;
        };
        let identity = usage.get("accounting").and_then(|accounting| {
            let source = accounting
                .get("sourceSessionId")
                .and_then(Value::as_str)
                .and_then(ident)?;
            let sequence = accounting.get("sequence").and_then(Value::as_u64)?;
            Some((source.to_owned(), sequence))
        });
        let key = match (&identity, line.event_id, line.session_id) {
            (Some((source, sequence)), _, _) => format!("copilot-receipt:{source}:{sequence}"),
            (None, Some(id), _) => format!("copilot-receipt:{id}"),
            (None, None, Some(sid)) => format!("copilot-receipt:{sid}:{}", self.index),
            (None, None, None) => return,
        };
        self.index += 1;
        if !self.seen.insert(key.clone()) {
            return;
        }
        let counters = RawCounters {
            input: u64_field(usage, "inputTokens"),
            output: u64_field(usage, "outputTokens"),
            cache_read: u64_field(usage, "cacheReadTokens"),
            cache_write: u64_field(usage, "cacheWriteTokens"),
            reasoning: u64_field(usage, "reasoningTokens"),
        };
        // An undated receipt stays out of `pending`: the next snapshot then
        // counts its tokens, at that exit, instead of losing them.
        if line.timestamp.is_none() {
            return;
        }
        self.pending.push(Pending {
            model: model.to_owned(),
            counters,
            identity,
        });
        let usage_event = counters.to_usage();
        if usage_event.token_volume() == 0 {
            return;
        }
        let subagent = value.get("agentId").and_then(Value::as_str).is_some()
            || usage.get("initiator").and_then(Value::as_str) == Some("sub-agent");
        events.usage_events.push(KeyedUsageEvent {
            key: Some(key),
            event: UsageEvent {
                timestamp: line.timestamp,
                session_id: line.session_id.cloned(),
                model: Some(model.to_owned()),
                source_kind: if subagent {
                    SourceKind::Subagent
                } else {
                    SourceKind::Main
                },
                attribution_agent: None,
                attribution_skill: None,
                project: project.map(ToOwned::to_owned),
                usage: usage_event,
                granularity: UsageGranularity::Call,
            },
        });
    }

    /// What the receipts read since the previous exit already counted, per
    /// model, of this exit's cumulative. A receipt past its source's mark is
    /// left for the next exit; an exit without marks predates receipts and
    /// covers all of them.
    pub(super) fn covered_by(&mut self, snapshot: &Value) -> HashMap<String, RawCounters> {
        let marks = snapshot
            .pointer("/data/usageAccountingWatermarks")
            .and_then(Value::as_object);
        let mut covered: HashMap<String, RawCounters> = HashMap::new();
        self.pending.retain(|receipt| {
            let subsumed = match (marks, &receipt.identity) {
                (None, _) | (Some(_), None) => true,
                (Some(marks), Some((source, sequence))) => marks
                    .get(source)
                    .and_then(Value::as_u64)
                    .is_some_and(|mark| mark >= *sequence),
            };
            if subsumed {
                covered
                    .entry(receipt.model.clone())
                    .or_default()
                    .add(receipt.counters);
            }
            !subsumed
        });
        covered
    }
}

#[cfg(test)]
pub(super) mod tests {
    use tempfile::TempDir;
    use time::UtcOffset;

    use crate::collector::copilot::collect;
    use crate::collector::copilot::tests::{shutdown, total_volume, write_session};
    use crate::model::{Collection, SourceKind, UsageGranularity};

    pub(in crate::collector::copilot) fn receipt(
        at: &str,
        source: &str,
        sequence: u64,
        input: u64,
        output: u64,
    ) -> String {
        format!(
            r#"{{"type":"session.usage_record","id":"r-{source}-{sequence}","timestamp":"{at}","parentId":null,"data":{{"usage":{{"model":"gpt-5-mini","inputTokens":{input},"outputTokens":{output},"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0,"accounting":{{"sourceSessionId":"{source}","sequence":{sequence},"usageId":"u-{sequence}"}}}}}}}}"#
        )
    }

    fn marked(line: &str, marks: &str) -> String {
        line.replacen(
            r#""data":{"#,
            &format!(r#""data":{{"usageAccountingWatermarks":{marks},"#),
            1,
        )
    }

    fn granularities(collection: &Collection) -> Vec<(UsageGranularity, u64)> {
        collection
            .usage_events
            .iter()
            .map(|event| (event.granularity, event.usage.token_volume()))
            .collect()
    }

    fn write(temp: &TempDir, session: &str, lines: &[String]) {
        write_session(temp.path(), session, &(lines.join("\n") + "\n"));
    }

    #[test]
    fn session_without_an_exit_counts_its_receipts() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let sub = receipt("2026-07-27T10:01:00.000Z", "s1", 2, 200, 20).replacen(
            r#""parentId":null"#,
            r#""parentId":null,"agentId":"agent-1""#,
            1,
        );
        write(
            &temp,
            "s1",
            &[
                r#"{"type":"session.start","id":"st","timestamp":"2026-07-27T09:59:00.000Z","data":{"context":{"cwd":"/Users/me/code/app"}}}"#.to_owned(),
                receipt("2026-07-27T10:00:00.000Z", "s1", 1, 100, 10),
                sub,
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(
            granularities(&collection),
            vec![(UsageGranularity::Call, 110), (UsageGranularity::Call, 220)]
        );
        assert_eq!(collection.usage_events[0].source_kind, SourceKind::Main);
        assert_eq!(collection.usage_events[1].source_kind, SourceKind::Subagent);
        assert_eq!(
            collection.usage_events[0].project.as_deref(),
            Some("Users/me/code/app")
        );
    }

    #[test]
    fn an_exit_whose_marks_cover_the_receipts_adds_nothing() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write(
            &temp,
            "s1",
            &[
                receipt("2026-07-27T10:00:00.000Z", "s1", 1, 100, 10),
                receipt("2026-07-27T10:01:00.000Z", "s1", 2, 200, 20),
                marked(
                    &shutdown("e-1", "2026-07-27T11:00:00.000Z", 300, 30),
                    r#"{"s1":2}"#,
                ),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(
            granularities(&collection),
            vec![(UsageGranularity::Call, 110), (UsageGranularity::Call, 220)]
        );
    }

    /// Calls from before the CLI wrote receipts are in the cumulative only.
    #[test]
    fn an_exit_keeps_what_its_receipts_do_not_carry() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write(
            &temp,
            "s1",
            &[
                receipt("2026-07-27T10:00:00.000Z", "s1", 1, 100, 10),
                receipt("2026-07-27T10:01:00.000Z", "s1", 2, 200, 20),
                marked(
                    &shutdown("e-1", "2026-07-27T11:00:00.000Z", 1_300, 130),
                    r#"{"s1":2}"#,
                ),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(
            granularities(&collection),
            vec![
                (UsageGranularity::Call, 110),
                (UsageGranularity::Call, 220),
                (UsageGranularity::Aggregate, 1_100)
            ]
        );
    }

    /// A receipt written after the snapshot was taken is past its mark and
    /// belongs to the next exit's cumulative.
    #[test]
    fn a_receipt_past_the_mark_waits_for_the_next_exit() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write(
            &temp,
            "s1",
            &[
                receipt("2026-07-27T10:00:00.000Z", "s1", 1, 100, 10),
                receipt("2026-07-27T10:01:00.000Z", "s1", 2, 200, 20),
                marked(
                    &shutdown("e-1", "2026-07-27T11:00:00.000Z", 100, 10),
                    r#"{"s1":1}"#,
                ),
                marked(
                    &shutdown("e-2", "2026-07-27T12:00:00.000Z", 300, 30),
                    r#"{"s1":2}"#,
                ),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 330);
        assert_eq!(collection.usage_events.len(), 2);
    }

    #[test]
    fn a_plain_exit_after_a_marked_one_counts_only_its_delta() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write(
            &temp,
            "s1",
            &[
                receipt("2026-07-27T10:00:00.000Z", "s1", 1, 100, 10),
                marked(
                    &shutdown("e-1", "2026-07-27T11:00:00.000Z", 100, 10),
                    r#"{"s1":1}"#,
                ),
                shutdown("e-2", "2026-07-27T12:00:00.000Z", 300, 30),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(
            granularities(&collection),
            vec![
                (UsageGranularity::Call, 110),
                (UsageGranularity::Aggregate, 220)
            ]
        );
    }

    #[test]
    fn a_forks_copy_of_a_receipt_collapses_into_the_original() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let original = receipt("2026-07-27T10:00:00.000Z", "parent", 1, 100, 10);
        write(&temp, "parent", std::slice::from_ref(&original));
        write(
            &temp,
            "fork",
            &[
                original,
                receipt("2026-07-27T11:00:00.000Z", "fork", 1, 400, 40),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110 + 440);
    }

    #[test]
    fn an_undated_receipt_is_left_to_the_next_exit() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let undated = receipt("2026-07-27T10:00:00.000Z", "s1", 1, 100, 10).replacen(
            r#""timestamp":"2026-07-27T10:00:00.000Z","#,
            "",
            1,
        );
        write(
            &temp,
            "s1",
            &[
                undated,
                marked(
                    &shutdown("e-1", "2026-07-27T11:00:00.000Z", 100, 10),
                    r#"{"s1":1}"#,
                ),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(
            granularities(&collection),
            vec![(UsageGranularity::Aggregate, 110)]
        );
    }
}
