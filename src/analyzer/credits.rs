//! CREDITS panel data: daily AI-credit spend from Copilot's nano-AIU ledger.
use std::collections::BTreeMap;

use time::{Date, Duration, UtcOffset};

use crate::model::{Collection, CreditsHistory};

/// Daily AI-credit spend over the fixed 30-day window: the sum of Copilot's
/// `totalNanoAiu` deltas per local day, in credits (1e9 nano-AIU). `None`
/// when the provider records no credit samples at all.
#[allow(
    clippy::cast_precision_loss,
    reason = "Credits are a display quantity; nano-AIU never approaches 2^52."
)]
pub(super) fn credits_history(
    collection: &Collection,
    window_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> Option<CreditsHistory> {
    let mut daily: BTreeMap<Date, u64> = BTreeMap::new();
    for sample in &collection.credit_samples {
        let date = sample.timestamp.to_offset(local_offset).date();
        if date < window_start || date > period_end {
            continue;
        }
        let entry = daily.entry(date).or_insert(0);
        *entry = entry.saturating_add(sample.nano_aiu);
    }
    if daily.is_empty() {
        return None;
    }

    let mut days = Vec::new();
    let mut total_nano = 0u64;
    let mut peak: Option<(Date, f64)> = None;
    let mut date = window_start;
    while date <= period_end {
        let nano = daily.get(&date).copied().unwrap_or(0);
        total_nano = total_nano.saturating_add(nano);
        let credits = nano as f64 / 1e9;
        if nano > 0 && peak.is_none_or(|(_, best)| credits > best) {
            peak = Some((date, credits));
        }
        days.push((date, credits));
        date += Duration::days(1);
    }
    Some(CreditsHistory {
        days,
        total: total_nano as f64 / 1e9,
        peak,
    })
}
