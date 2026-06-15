use std::collections::HashMap;

use time::{Date, OffsetDateTime, UtcOffset};

use crate::model::{Collection, DurationBucket, DurationSummary, SessionSpan};

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
        let date = touch.timestamp.to_offset(local_offset).date();
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
        .map(|((session_id, _), (started_at, ended_at))| SessionSpan {
            session_id: session_id.to_owned(),
            started_at,
            ended_at,
        })
        .max_by_key(SessionSpan::duration_secs)
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
                let date = timestamp.to_offset(local_offset).date();
                date >= period_start && date <= period_end
            })
        })
        .map(|event| event.duration_ms)
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

fn percentile_ms(sorted_values: &[u64], percentile: usize) -> u64 {
    let rank = sorted_values.len().saturating_mul(percentile).div_ceil(100);
    let index = rank.saturating_sub(1).min(sorted_values.len() - 1);
    sorted_values[index]
}

fn duration_buckets(sorted_values: &[u64]) -> Vec<DurationBucket> {
    const SECOND: u64 = 1_000;
    const MINUTE: u64 = 60 * SECOND;
    // Weighted toward the autonomy range: in 90 days of real data ~96% of
    // turns finish under 20m, so the short side gets three buckets and the
    // 20m+ tail (the "can it run unattended" signal) gets three. Six total so
    // the section aligns row-for-row with PARALLEL AGENTS. The first three are
    // <20m; `.skip(3)` therefore still isolates the unattended tail.
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
