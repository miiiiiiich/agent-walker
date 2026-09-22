use std::collections::BTreeMap;

use time::{Date, UtcOffset};

use crate::model::{ActiveTimeSummary, Collection};

pub(super) fn active_time_summary(
    collection: &Collection,
    window_start: Date,
    period_end: Date,
    window_days: u16,
    local_offset: UtcOffset,
) -> Option<ActiveTimeSummary> {
    let in_window = |date: Date| date >= window_start && date <= period_end;
    let mut summary = ActiveTimeSummary {
        window_days,
        ..ActiveTimeSummary::default()
    };
    let mut daily: BTreeMap<Date, u64> = BTreeMap::new();
    for event in &collection.duration_events {
        let Some(date) = event
            .timestamp
            .map(|timestamp| timestamp.to_offset(local_offset).date())
            .filter(|date| in_window(*date))
        else {
            continue;
        };
        if event.duration_ms == 0 {
            continue;
        }
        summary.turns += 1;
        summary.active_ms = summary.active_ms.saturating_add(event.active_ms());
        summary.human_wait_ms = summary.human_wait_ms.saturating_add(event.human_wait_ms);
        if let Some(model_ms) = event.model_ms {
            summary.model_ms = summary
                .model_ms
                .saturating_add(model_ms.min(event.active_ms()));
            summary.measured_ms = summary.measured_ms.saturating_add(event.active_ms());
        }
        let day = daily.entry(date).or_default();
        *day = day.saturating_add(event.active_ms());
    }
    if summary.turns == 0 {
        return None;
    }
    summary.daily_active_ms = daily.into_iter().collect();
    summary.pace_gaps_ms = collection
        .pace_events
        .iter()
        .filter(|event| {
            event
                .timestamp
                .map(|timestamp| timestamp.to_offset(local_offset).date())
                .is_some_and(in_window)
        })
        .map(|event| event.gap_ms)
        .collect();
    summary.pace_gaps_ms.sort_unstable();
    for event in &collection.usage_events {
        let dated = event
            .timestamp
            .map(|timestamp| timestamp.to_offset(local_offset).date())
            .is_some_and(in_window);
        if !dated {
            continue;
        }
        let usage = &event.usage;
        summary.context_tokens = summary
            .context_tokens
            .saturating_add(usage.input_tokens)
            .saturating_add(usage.cache_creation_input_tokens)
            .saturating_add(usage.cache_read_input_tokens);
        summary.output_tokens = summary.output_tokens.saturating_add(usage.output_tokens);
    }
    Some(summary)
}
