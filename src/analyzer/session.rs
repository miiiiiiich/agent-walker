use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use time::{Date, Duration, OffsetDateTime, UtcOffset};

use crate::format::short_model_name;
use crate::model::{Collection, DurationEvent, Provider, Summary};
use crate::timestamp::local_date;

use super::{duration, summarize};

pub(crate) struct SessionReport {
    pub(crate) id: String,
    pub(crate) first_at: OffsetDateTime,
    pub(crate) last_at: OffsetDateTime,
    pub(crate) summary: Summary,
    pub(crate) turn_lengths_ms: Vec<u64>,
    pub(crate) models: Vec<SessionModel>,
}

pub(crate) struct SessionModel {
    pub(crate) label: String,
    pub(crate) tokens: u64,
}

/// The same analysis as a whole window, over one session's slice of the logs.
pub(crate) fn session_report(
    collections: &[Collection],
    id: &str,
    now: OffsetDateTime,
    days: u16,
    local_offset: UtcOffset,
) -> Option<SessionReport> {
    let mut slices: Vec<Collection> = collections
        .iter()
        .map(|c| slice(c, id))
        .filter(holds_anything)
        .collect();
    // A session is one agent's: kept as that agent's, CONTEXT is worked out as on
    // its own tab. The total's mix has no one cache rule and leaves it out, so
    // only an id two agents share lands there.
    let session = if slices.len() == 1 {
        slices.remove(0)
    } else {
        Collection::combined(PathBuf::from("one session"), &slices)
    };
    let summary = summarize(&session, now, days, local_offset);
    let (first_at, last_at) = bounds(
        &session,
        summary.period_start,
        summary.period_end,
        local_offset,
    )?;
    let models = models_by_label(&summary);
    let turn_lengths_ms = duration::turn_lengths_in_order(
        &session,
        summary.period_start,
        summary.period_end,
        local_offset,
    );
    Some(SessionReport {
        id: id.to_owned(),
        first_at,
        last_at,
        summary,
        turn_lengths_ms,
        models,
    })
}

/// Rows merge by printed label, as on the card: dated and undated ids of one
/// model would otherwise read as two models with the same name.
fn models_by_label(summary: &Summary) -> Vec<SessionModel> {
    let mut by_label: BTreeMap<String, u64> = BTreeMap::new();
    for model in &summary.models {
        let tokens = model.usage.token_volume();
        if tokens > 0 {
            let entry = by_label.entry(short_model_name(&model.name)).or_default();
            *entry = entry.saturating_add(tokens);
        }
    }
    let mut models: Vec<SessionModel> = by_label
        .into_iter()
        .map(|(label, tokens)| SessionModel { label, tokens })
        .collect();
    models.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.label.cmp(&b.label)));
    models
}

/// Modes, efforts, permissions and interrupts carry no session id, and the
/// limits belong to the account, so they stay out rather than count another
/// session's. A pace sample has no id either, but on Claude it is dated at the
/// prompt that opens a turn, to the millisecond, so a session's samples are the
/// ones on its turns' starts. Codex's turn lengths come from the payload and
/// miss by a few milliseconds, which would keep a part of the samples as if it
/// were all of them, so Codex shows none.
fn slice(collection: &Collection, id: &str) -> Collection {
    let this_session = |candidate: &Option<String>| candidate.as_deref() == Some(id);
    let mut slice = Collection::new(collection.provider, collection.root.clone());
    slice.usage_events = collection
        .usage_events
        .iter()
        .filter(|event| this_session(&event.session_id))
        .cloned()
        .collect();
    slice.tool_events = collection
        .tool_events
        .iter()
        .filter(|event| this_session(&event.session_id))
        .cloned()
        .collect();
    slice.session_touches = collection
        .session_touches
        .iter()
        .filter(|touch| touch.session_id == id)
        .cloned()
        .collect();
    slice.duration_events = collection
        .duration_events
        .iter()
        .filter(|event| this_session(&event.session_id))
        .cloned()
        .collect();
    if collection.provider == Provider::Claude {
        let turn_starts: HashSet<OffsetDateTime> = slice
            .duration_events
            .iter()
            .filter_map(turn_start)
            .collect();
        slice.pace_events = collection
            .pace_events
            .iter()
            .filter(|event| event.timestamp.is_some_and(|at| turn_starts.contains(&at)))
            .cloned()
            .collect();
    }
    slice
}

fn holds_anything(slice: &Collection) -> bool {
    !(slice.usage_events.is_empty()
        && slice.tool_events.is_empty()
        && slice.session_touches.is_empty()
        && slice.duration_events.is_empty())
}

fn turn_start(event: &DurationEvent) -> Option<OffsetDateTime> {
    let length = Duration::milliseconds(i64::try_from(event.duration_ms).ok()?);
    event.timestamp?.checked_sub(length)
}

/// Only what falls in the window: the numbers cover nothing else, and a session
/// that ended before it has nothing to report, however recent its log file is.
fn bounds(
    session: &Collection,
    period_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let touches = session.session_touches.iter().map(|touch| touch.timestamp);
    let usage = session
        .usage_events
        .iter()
        .filter_map(|event| event.timestamp);
    let turns = session
        .duration_events
        .iter()
        .filter_map(|event| event.timestamp);
    // Legacy OpenCode dates a tool call after the session's last message, with
    // no touch of its own: left out, the span would end before what it counts.
    let tools = session
        .tool_events
        .iter()
        .filter_map(|event| event.timestamp);
    let stamps: Vec<OffsetDateTime> = touches
        .chain(usage)
        .chain(turns)
        .chain(tools)
        .filter(|&at| {
            local_date(at, local_offset)
                .is_some_and(|date| date >= period_start && date <= period_end)
        })
        .collect();
    Some((*stamps.iter().min()?, *stamps.iter().max()?))
}

#[cfg(test)]
mod tests {
    use time::macros::{datetime, offset};

    use super::*;
    use crate::model::{
        PaceEvent, SessionTouch, SourceKind, TokenUsage, ToolEvent, UsageEvent, UsageGranularity,
    };

    const NOW: OffsetDateTime = datetime!(2026-11-06 12:00 +09:00);

    fn usage(session: &str, input_tokens: u64) -> UsageEvent {
        UsageEvent {
            timestamp: Some(datetime!(2026-11-06 10:00 +09:00)),
            session_id: Some(session.to_owned()),
            model: Some("gpt-5-mini".to_owned()),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            attribution_skill: None,
            project: None,
            usage: TokenUsage {
                input_tokens,
                ..TokenUsage::default()
            },
            granularity: UsageGranularity::Call,
        }
    }

    fn turn(session: &str, end: OffsetDateTime, duration_ms: u64) -> DurationEvent {
        DurationEvent {
            timestamp: Some(end),
            session_id: Some(session.to_owned()),
            duration_ms,
            human_wait_ms: 0,
            model_ms: None,
            status: None,
        }
    }

    fn logs() -> Collection {
        let mut logs = Collection::new(Provider::Claude, PathBuf::new());
        logs.usage_events = vec![usage("mine", 100), usage("other", 900)];
        logs.tool_events = ["mine", "other", "other"]
            .map(|session| ToolEvent {
                timestamp: Some(datetime!(2026-11-06 10:00 +09:00)),
                session_id: Some(session.to_owned()),
                tool_name: "Bash".to_owned(),
                subagent_type: None,
                source_kind: SourceKind::Main,
            })
            .to_vec();
        logs.session_touches = ["mine", "other"]
            .map(|session| SessionTouch {
                timestamp: datetime!(2026-11-06 09:30 +09:00),
                session_id: session.to_owned(),
            })
            .to_vec();
        logs.duration_events = vec![
            turn("mine", datetime!(2026-11-06 10:02 +09:00), 120_000),
            turn("mine", datetime!(2026-11-06 10:01 +09:00), 60_000),
            turn("other", datetime!(2026-11-06 11:01 +09:00), 60_000),
        ];
        logs.pace_events = vec![
            PaceEvent {
                timestamp: Some(datetime!(2026-11-06 10:00 +09:00)),
                gap_ms: 5_000,
            },
            PaceEvent {
                timestamp: Some(datetime!(2026-11-06 11:00 +09:00)),
                gap_ms: 900_000,
            },
        ];
        logs
    }

    fn report(id: &str) -> Option<SessionReport> {
        session_report(&[logs()], id, NOW, 30, offset!(+09:00))
    }

    #[test]
    fn only_this_sessions_events_count() {
        let report = report("mine").unwrap();
        assert_eq!(report.summary.total_usage.input_tokens, 100);
        assert_eq!(report.summary.tools[0].calls, 1);
        assert_eq!(report.summary.completion_duration.unwrap().count, 2);
    }

    #[test]
    fn a_pace_sample_belongs_to_the_session_whose_turn_it_opened() {
        let report = report("mine").unwrap();
        assert_eq!(report.summary.active_time.unwrap().pace_gaps_ms, [5_000]);
    }

    #[test]
    fn turns_stay_in_the_order_they_ran() {
        assert_eq!(report("mine").unwrap().turn_lengths_ms, [60_000, 120_000]);
    }

    #[test]
    fn the_session_spans_its_first_and_last_event() {
        let report = report("mine").unwrap();
        assert_eq!(report.first_at, datetime!(2026-11-06 09:30 +09:00));
        assert_eq!(report.last_at, datetime!(2026-11-06 10:02 +09:00));
    }

    #[test]
    fn a_session_of_one_agent_keeps_that_agents_cache_numbers() {
        let report = report("mine").unwrap();
        assert_eq!(report.summary.provider, Provider::Claude);
        assert!(report.summary.context.is_some());
    }

    #[test]
    fn a_tool_call_after_the_last_message_still_falls_in_the_span() {
        let mut logs = logs();
        logs.tool_events.push(ToolEvent {
            timestamp: Some(datetime!(2026-11-06 11:30 +09:00)),
            session_id: Some("mine".to_owned()),
            tool_name: "bash".to_owned(),
            subagent_type: None,
            source_kind: SourceKind::Main,
        });
        let report = session_report(&[logs], "mine", NOW, 30, offset!(+09:00)).unwrap();
        assert_eq!(report.last_at, datetime!(2026-11-06 11:30 +09:00));
    }

    #[test]
    fn a_session_with_only_tool_calls_in_the_window_is_still_a_session() {
        let mut logs = Collection::new(Provider::OpenCode, PathBuf::new());
        logs.tool_events.push(ToolEvent {
            timestamp: Some(datetime!(2026-11-06 11:30 +09:00)),
            session_id: Some("mine".to_owned()),
            tool_name: "bash".to_owned(),
            subagent_type: None,
            source_kind: SourceKind::Main,
        });
        assert!(session_report(&[logs], "mine", NOW, 30, offset!(+09:00)).is_some());
    }

    #[test]
    fn an_id_no_log_holds_is_no_session() {
        assert!(report("nobody").is_none());
    }

    #[test]
    fn a_session_that_ended_before_the_window_is_no_session() {
        let report = session_report(
            &[logs()],
            "mine",
            NOW + Duration::days(90),
            30,
            offset!(+09:00),
        );
        assert!(report.is_none());
    }

    #[test]
    fn a_session_across_the_window_edge_spans_only_what_the_numbers_cover() {
        let mut logs = logs();
        logs.session_touches.push(SessionTouch {
            timestamp: datetime!(2026-08-01 09:30 +09:00),
            session_id: "mine".to_owned(),
        });
        let report = session_report(&[logs], "mine", NOW, 30, offset!(+09:00)).unwrap();
        assert_eq!(report.first_at, datetime!(2026-11-06 09:30 +09:00));
    }

    #[test]
    fn codex_shows_no_pace_rather_than_a_part_of_it() {
        let mut logs = logs();
        logs.provider = Provider::Codex;
        let report = session_report(&[logs], "mine", NOW, 30, offset!(+09:00)).unwrap();
        assert_eq!(
            report.summary.active_time.unwrap().pace_gaps_ms,
            Vec::<u64>::new()
        );
    }

    #[test]
    fn dated_and_undated_ids_of_one_model_read_as_one() {
        let mut logs = logs();
        logs.usage_events[0].model = Some("claude-opus-4-8".to_owned());
        let mut dated = usage("mine", 300);
        dated.model = Some("claude-opus-4-8-20260101".to_owned());
        logs.usage_events.push(dated);
        let report = session_report(&[logs], "mine", NOW, 30, offset!(+09:00)).unwrap();
        let labels: Vec<&str> = report.models.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels.len(), 1, "{labels:?}");
        assert_eq!(report.models[0].tokens, 400);
    }

    #[test]
    fn an_absurd_turn_length_does_not_panic() {
        assert_eq!(
            turn_start(&turn("mine", OffsetDateTime::UNIX_EPOCH, u64::MAX)),
            None
        );
        let huge = turn("mine", datetime!(2026-11-06 10:00 UTC), i64::MAX as u64);
        assert_eq!(turn_start(&huge), None);
    }
}
