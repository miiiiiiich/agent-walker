use std::collections::BTreeSet;

use time::{Date, Duration};

pub(super) fn streaks(
    active_dates: &BTreeSet<Date>,
    period_start: Date,
    period_end: Date,
) -> (usize, usize) {
    let mut longest = 0;
    let mut current_run = 0;
    let mut date = period_start;
    while date <= period_end {
        if active_dates.contains(&date) {
            current_run += 1;
            longest = longest.max(current_run);
        } else {
            current_run = 0;
        }
        date += Duration::days(1);
    }

    let mut current = 0;
    let mut cursor = period_end;
    while cursor >= period_start && active_dates.contains(&cursor) {
        current += 1;
        cursor -= Duration::days(1);
    }

    (longest, current)
}
