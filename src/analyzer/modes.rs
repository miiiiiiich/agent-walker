//! MODES panel data: Claude thinking / fast flags and Codex reasoning effort.
use std::collections::BTreeMap;

use time::{Date, OffsetDateTime, UtcOffset};

use crate::model::{Collection, ModesSummary};

/// Mode usage over the fixed 30-day window: Claude thinking / fast flags per
/// assistant message, Codex reasoning-effort per turn.
pub(super) fn modes_summary(
    collection: &Collection,
    window_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> ModesSummary {
    let in_window = |timestamp: Option<OffsetDateTime>| {
        timestamp
            .map(|ts| ts.to_offset(local_offset).date())
            .is_some_and(|date| date >= window_start && date <= period_end)
    };

    let mut modes = ModesSummary::default();
    for event in &collection.mode_events {
        if !in_window(event.timestamp) {
            continue;
        }
        modes.assistant_turns += 1;
        if event.has_thinking {
            modes.thinking_turns += 1;
        }
        if event.fast {
            modes.fast_turns += 1;
        }
    }

    let mut effort_counts: BTreeMap<String, usize> = BTreeMap::new();
    for event in &collection.effort_events {
        if !in_window(event.timestamp) {
            continue;
        }
        *effort_counts.entry(event.effort.clone()).or_default() += 1;
    }
    modes.efforts = effort_counts.into_iter().collect();
    modes
        .efforts
        .sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    modes
}
