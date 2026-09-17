use std::collections::HashMap;

use time::{Date, OffsetDateTime, UtcOffset};

use crate::model::{Collection, Orchestration};

/// Bound session spans per (session, local day): a resumed id reused across
/// days must not collapse into one giant span.
pub(crate) fn session_day_bounds(
    collection: &Collection,
    period_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> HashMap<(&str, Date), (OffsetDateTime, OffsetDateTime)> {
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
}

#[allow(
    clippy::cast_precision_loss,
    reason = "avg_concurrency is a display-only weighted mean, never fed back into integer math."
)]
pub(super) fn orchestration(
    collection: &Collection,
    period_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> Orchestration {
    let bounds = session_day_bounds(collection, period_start, period_end, local_offset);

    let mut events: Vec<(OffsetDateTime, i32)> = Vec::with_capacity(bounds.len() * 2);
    for (start, end) in bounds.into_values() {
        if end <= start {
            continue;
        }
        events.push((start, 1));
        events.push((end, -1));
    }
    if events.is_empty() {
        return Orchestration::default();
    }

    // At equal timestamps, ends (-1) sort before starts (+1) so that two
    // sessions merely touching end-to-end are not counted as overlapping.
    events.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));

    let mut active: i32 = 0;
    let mut peak: i32 = 0;
    let mut active_secs: i64 = 0;
    let mut level_secs = [0i64; 6];
    let mut prev: Option<OffsetDateTime> = None;
    for (time, delta) in events {
        if let Some(previous) = prev {
            let dur = (time - previous).whole_seconds().max(0);
            if active >= 1 {
                active_secs += dur;
            }
            if let Some(bucket) = level_bucket(active) {
                level_secs[bucket] += dur;
            }
        }
        active += delta;
        peak = peak.max(active);
        prev = Some(time);
    }

    let avg_concurrency = if active_secs > 0 {
        let midpoints = [1.0_f64, 2.0, 3.0, 5.0, 8.0, 11.0];
        let weighted: f64 = level_secs
            .iter()
            .zip(midpoints)
            .map(|(secs, midpoint)| *secs as f64 * midpoint)
            .sum();
        weighted / active_secs as f64
    } else {
        0.0
    };

    Orchestration {
        avg_concurrency,
        peak_concurrency: usize::try_from(peak.max(0)).unwrap_or(0),
        time_by_level: level_secs.map(|secs| u64::try_from(secs).unwrap_or(0)),
    }
}

fn level_bucket(active: i32) -> Option<usize> {
    match active {
        1 => Some(0),
        2 => Some(1),
        3 => Some(2),
        4..=6 => Some(3),
        7..=9 => Some(4),
        n if n >= 10 => Some(5),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use time::macros::datetime;

    use crate::model::{Collection, Provider, SessionTouch};

    fn touch(session: &str, at: OffsetDateTime) -> SessionTouch {
        SessionTouch {
            timestamp: at,
            session_id: session.to_owned(),
        }
    }

    fn collection(touches: Vec<SessionTouch>) -> Collection {
        Collection {
            session_touches: touches,
            ..Collection::new(Provider::Claude, "/tmp".into())
        }
    }

    #[test]
    fn two_fully_overlapping_sessions_score_full_parallel() {
        let c = collection(vec![
            touch("a", datetime!(2026-06-08 10:00 UTC)),
            touch("a", datetime!(2026-06-08 12:00 UTC)),
            touch("b", datetime!(2026-06-08 10:30 UTC)),
            touch("b", datetime!(2026-06-08 11:30 UTC)),
        ]);
        let result = super::orchestration(
            &c,
            datetime!(2026-06-01 0:00 UTC).date(),
            datetime!(2026-06-30 0:00 UTC).date(),
            time::UtcOffset::UTC,
        );
        assert_eq!(result.peak_concurrency, 2);
        assert_eq!(result.time_by_level, [3600, 3600, 0, 0, 0, 0]);
        assert!((result.avg_concurrency - 1.5).abs() < 1e-9);
    }

    #[test]
    fn back_to_back_sessions_are_not_parallel() {
        let c = collection(vec![
            touch("a", datetime!(2026-06-08 10:00 UTC)),
            touch("a", datetime!(2026-06-08 11:00 UTC)),
            touch("b", datetime!(2026-06-08 11:00 UTC)),
            touch("b", datetime!(2026-06-08 12:00 UTC)),
        ]);
        let result = super::orchestration(
            &c,
            datetime!(2026-06-01 0:00 UTC).date(),
            datetime!(2026-06-30 0:00 UTC).date(),
            time::UtcOffset::UTC,
        );
        assert_eq!(result.peak_concurrency, 1);
        assert_eq!(result.time_by_level[1..], [0, 0, 0, 0, 0]);
    }
}
