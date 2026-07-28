use ratatui::prelude::*;

use crate::model::{LimitDay, Summary};

use super::{ChartColumn, column_chart_lines};
use crate::ui::{theme, utils};

/// frame. The y-axis is FIXED at 0-100% (unlike the auto-scaled charts) so a
/// quiet month doesn't inflate a 3% day into a full column. A day that hit
/// the limit renders red; a day with no provider use renders as a faint dot;
/// a day with use but no recorded sample stays blank.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
pub(in crate::ui) fn limits_chart_lines(
    summary: &Summary,
    width: u16,
    body_height: usize,
) -> Vec<Line<'static>> {
    let Some(limits) = &summary.limits else {
        return Vec::new();
    };
    if limits.days.is_empty() {
        return Vec::new();
    }
    let annotation = limits.peak.map_or_else(String::new, |(date, value)| {
        format!(
            "peak {value:.0}% · {} {}",
            utils::month_abbrev(date.month()),
            date.day()
        )
    });

    let half_cells = body_height.max(1) * 2;
    let columns: Vec<ChartColumn> = limits
        .days
        .iter()
        .map(|(_, day)| match day {
            LimitDay::Measured(value) => ChartColumn {
                level: if *value > 0.0 {
                    ((value / 100.0) * half_cells as f64).round().max(1.0) as usize
                } else {
                    0
                },
                color: if *value >= 99.5 {
                    theme::HOT
                } else {
                    theme::GREEN
                },
                // Measured 0% is a real data point, not an absence.
                baseline: "▁",
            },
            LimitDay::NoUse => ChartColumn {
                level: 0,
                color: theme::DIM,
                baseline: "·",
            },
            LimitDay::NoSample => ChartColumn {
                level: 0,
                color: theme::DIM,
                baseline: " ",
            },
        })
        .collect();
    let day_count = limits.days.len();
    let x_points: Vec<(usize, String)> = [0.0, 0.5, 1.0]
        .iter()
        .filter_map(|fraction| {
            let index = ((day_count.saturating_sub(1)) as f64 * fraction).round() as usize;
            let (date, _) = limits.days.get(index)?;
            Some((
                index,
                format!("{} {}", utils::month_abbrev(date.month()), date.day()),
            ))
        })
        .collect();
    column_chart_lines(
        "LIMITS",
        &annotation,
        &["100%".to_owned(), "50%".to_owned(), "0".to_owned()],
        &columns,
        &x_points,
        width,
        body_height,
    )
}
