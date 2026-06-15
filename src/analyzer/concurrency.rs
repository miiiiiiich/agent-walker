use std::collections::HashMap;

use time::{Date, OffsetDateTime, UtcOffset};

use crate::model::{Collection, Orchestration};

/// Reconstruct session spans from touches and sweep them for concurrency.
///
/// `parallel_rate` is the share of active wall-time covered by two or more
/// simultaneous sessions; `peak_concurrency` is the largest simultaneous
/// count. This is the "orchestration" primitive: running many sessions at
/// once, measurable on any agent, not a Claude-specific subagent feature.
#[allow(
    clippy::cast_precision_loss,
    reason = "parallel_rate is a display-only 0.0–1.0 ratio, never fed back into integer math."
)]
pub(super) fn orchestration(
    collection: &Collection,
    period_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> Orchestration {
    // Bound spans per (session, local day) exactly like longest_session_span:
    // a resumed id reused across days must not collapse into one giant span.
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

    // Only spans with real width can overlap; a single-touch session is a point.
    let mut events: Vec<(OffsetDateTime, i32)> = Vec::with_capacity(bounds.len() * 2);
    let mut span_count = 0usize;
    for (start, end) in bounds.into_values() {
        if end <= start {
            continue;
        }
        span_count += 1;
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
    let mut parallel_secs: i64 = 0;
    let mut level_secs = [0i64; 6];
    let mut prev: Option<OffsetDateTime> = None;
    for (time, delta) in events {
        if let Some(previous) = prev {
            let dur = (time - previous).whole_seconds().max(0);
            if active >= 1 {
                active_secs += dur;
            }
            if active >= 2 {
                parallel_secs += dur;
            }
            if let Some(bucket) = level_bucket(active) {
                level_secs[bucket] += dur;
            }
        }
        active += delta;
        peak = peak.max(active);
        prev = Some(time);
    }

    let parallel_rate = if active_secs > 0 {
        // active_secs >= parallel_secs >= 0, so the ratio stays within 0.0..=1.0.
        parallel_secs as f64 / active_secs as f64
    } else {
        0.0
    };

    Orchestration {
        parallel_rate,
        peak_concurrency: usize::try_from(peak.max(0)).unwrap_or(0),
        span_count,
        time_by_level: level_secs.map(|secs| u64::try_from(secs).unwrap_or(0)),
    }
}

/// Bucket a live concurrency count into the 5 distribution slots
/// (1 / 2 / 3 / 4–6 / 7+); `None` for idle stretches.
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

    use crate::model::{Collection, Provider, ScanStats, SessionTouch};

    fn touch(session: &str, at: OffsetDateTime) -> SessionTouch {
        SessionTouch {
            timestamp: at,
            session_id: session.to_owned(),
        }
    }

    fn collection(touches: Vec<SessionTouch>) -> Collection {
        Collection {
            provider: Provider::Claude,
            root: "/tmp".into(),
            usage_events: Vec::new(),
            tool_events: Vec::new(),
            session_touches: touches,
            duration_events: Vec::new(),
            stats: ScanStats::default(),
        }
    }

    #[test]
    fn two_fully_overlapping_sessions_score_full_parallel() {
        // a: 10:00-12:00, b: 10:30-11:30 (entirely inside a).
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
        assert_eq!(result.span_count, 2);
        // b's full hour overlaps; a runs 2h total => parallel 1h of 2h = 0.5.
        assert!((result.parallel_rate - 0.5).abs() < 1e-9);
        // 1h at solo (two 30m a-only stretches) + 1h at level 2.
        assert_eq!(result.time_by_level, [3600, 3600, 0, 0, 0, 0]);
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
        assert!(result.parallel_rate.abs() < 1e-9);
    }
}
