use std::collections::HashMap;

use time::{Date, OffsetDateTime, UtcOffset};

use crate::model::{Collection, DurationBucket, DurationSummary, SessionSpan};
use crate::timestamp::local_date;

pub(super) fn longest_session_span(
    collection: &Collection,
    period_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> Option<SessionSpan> {
    // Span per (session, local day): resumed sessions reuse their id across
    // days, so a raw per-session min/max would report multi-day "sessions".
    let mut bounds: HashMap<(&str, Date), (OffsetDateTime, OffsetDateTime)> = HashMap::new();
    for touch in &collection.session_touches {
        let Some(date) = local_date(touch.timestamp, local_offset) else {
            continue;
        };
        if date < period_start || date > period_end {
            continue;
        }
        bounds
            .entry((touch.session_id.as_str(), date))
            .and_modify(|(start, end)| {
                *start = (*start).min(touch.timestamp);
                *end = (*end).max(touch.timestamp);
            })
            .or_insert((touch.timestamp, touch.timestamp));
    }

    bounds
        .into_iter()
        .map(|(_, (started_at, ended_at))| SessionSpan {
            started_at,
            ended_at,
        })
        // `bounds` iterates in a random order, so equally long spans showed
        // a different session on every run. Full precision, because JSON
        // reports milliseconds that `duration_secs` truncates; equal lengths
        // then go to the latest start, which fixes the end as well.
        .max_by_key(|span| (span.ended_at - span.started_at, span.started_at))
}

pub(super) fn completion_duration_summary(
    collection: &Collection,
    period_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> Option<DurationSummary> {
    let mut values = collection
        .duration_events
        .iter()
        .filter(|event| {
            event.timestamp.is_none_or(|timestamp| {
                local_date(timestamp, local_offset)
                    .is_some_and(|date| date >= period_start && date <= period_end)
            })
        })
        // The human's answer time inside a turn is not the agent running.
        .map(crate::model::DurationEvent::active_ms)
        .filter(|duration_ms| *duration_ms > 0)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(DurationSummary {
        count: values.len(),
        p50_ms: percentile_ms(&values, 50),
        p90_ms: percentile_ms(&values, 90),
        p95_ms: percentile_ms(&values, 95),
        max_ms: *values.last().unwrap_or(&0),
        buckets: duration_buckets(&values),
    })
}

/// User interruptions dated inside the window. Only dated events count
/// (the same `is_some_and` ruling as the mode summaries): an undated event
/// would otherwise appear in every requested window.
pub(super) fn interrupted_count(
    collection: &Collection,
    period_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> usize {
    collection
        .interrupt_events
        .iter()
        .filter(|event| {
            event
                .timestamp
                .and_then(|timestamp| local_date(timestamp, local_offset))
                .is_some_and(|date| date >= period_start && date <= period_end)
        })
        .count()
}

fn percentile_ms(sorted_values: &[u64], percentile: usize) -> u64 {
    let rank = sorted_values.len().saturating_mul(percentile).div_ceil(100);
    let index = rank.saturating_sub(1).min(sorted_values.len() - 1);
    sorted_values[index]
}

fn duration_buckets(sorted_values: &[u64]) -> Vec<DurationBucket> {
    const SECOND: u64 = 1_000;
    const MINUTE: u64 = 60 * SECOND;
    // Six rows aligned with PARALLEL AGENTS; the first three are <20m —
    // completion.rs and card.rs take `.skip(3)` as the unattended tail.
    const BUCKETS: [(&str, u64, u64); 6] = [
        ("<2m", 0, 2 * MINUTE),
        ("2-10m", 2 * MINUTE, 10 * MINUTE),
        ("10-20m", 10 * MINUTE, 20 * MINUTE),
        ("20-30m", 20 * MINUTE, 30 * MINUTE),
        ("30-60m", 30 * MINUTE, 60 * MINUTE),
        ("1h+", 60 * MINUTE, u64::MAX),
    ];

    BUCKETS
        .iter()
        .map(|(label, start, end)| DurationBucket {
            label: (*label).to_owned(),
            count: sorted_values
                .iter()
                .filter(|value| **value >= *start && **value < *end)
                .count(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use time::Duration;
    use time::macros::{date, datetime};

    use super::*;
    use crate::model::{Provider, SessionTouch};

    #[test]
    fn equally_long_sessions_resolve_to_the_latest() {
        let first = datetime!(2026-06-01 09:00 UTC);
        let session_touches = (0..8)
            .flat_map(|day| {
                let start = first + Duration::days(day);
                [start, start + Duration::hours(1)].map(|timestamp| SessionTouch {
                    timestamp,
                    session_id: format!("s{day}"),
                })
            })
            .collect();
        let collection = Collection {
            session_touches,
            ..Collection::new(Provider::Claude, "/tmp/claude".into())
        };

        let span = longest_session_span(
            &collection,
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("the sessions should have a span");

        assert_eq!(span.started_at, datetime!(2026-06-08 09:00 UTC));
    }

    #[test]
    fn subsecond_differences_decide_the_longest() {
        let start = datetime!(2026-06-01 09:00 UTC);
        let session_touches = [("short", 100), ("long", 900)]
            .into_iter()
            .flat_map(|(id, end_ms)| {
                [
                    start,
                    start + Duration::hours(1) + Duration::milliseconds(end_ms),
                ]
                .map(|timestamp| SessionTouch {
                    timestamp,
                    session_id: id.to_owned(),
                })
            })
            .collect();
        let collection = Collection {
            session_touches,
            ..Collection::new(Provider::Claude, "/tmp/claude".into())
        };

        let span = longest_session_span(
            &collection,
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 01),
            UtcOffset::UTC,
        )
        .expect("the sessions should have a span");

        assert_eq!(span.ended_at, datetime!(2026-06-01 10:00:00.9 UTC));
    }
}
