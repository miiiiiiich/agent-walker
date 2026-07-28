//! LIMITS panel data: daily peak of the Codex plan's 5h window.
use std::collections::{BTreeMap, HashSet};

use time::{Date, Duration, UtcOffset};

use crate::model::{Collection, LimitDay, LimitsHistory};

/// Daily-peak LIMITS history over the fixed 30-day window. Tri-state per day:
/// a day with samples carries its peak `used_percent`; a day with provider
/// activity but no sample (older CLI versions) is `NoSample`; a day without
/// activity is `NoUse` — the chart renders the three differently, so a
/// measured 0% is never confused with "didn't use Codex that day".
pub(super) fn limits_history(
    collection: &Collection,
    window_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
    active_days: &HashSet<Date>,
) -> Option<LimitsHistory> {
    let mut daily_peak: BTreeMap<Date, f64> = BTreeMap::new();
    for sample in &collection.rate_limit_samples {
        let date = sample.timestamp.to_offset(local_offset).date();
        if date < window_start || date > period_end {
            continue;
        }
        let entry = daily_peak.entry(date).or_insert(0.0);
        if sample.used_percent > *entry {
            *entry = sample.used_percent;
        }
    }
    if daily_peak.is_empty() {
        return None;
    }

    let mut days = Vec::new();
    let mut peak: Option<(Date, f64)> = None;
    let mut date = window_start;
    while date <= period_end {
        let day = match daily_peak.get(&date) {
            Some(&value) => {
                if peak.is_none_or(|(_, best)| value > best) {
                    peak = Some((date, value));
                }
                LimitDay::Measured(value)
            }
            None if active_days.contains(&date) => LimitDay::NoSample,
            None => LimitDay::NoUse,
        };
        days.push((date, day));
        date += Duration::days(1);
    }
    Some(LimitsHistory { days, peak })
}
