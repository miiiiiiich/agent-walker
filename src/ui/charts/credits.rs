use ratatui::prelude::*;

use crate::model::Summary;

use super::{ChartColumn, column_chart_lines, level_for};
use crate::ui::{theme, utils};

/// CREDITS history: daily AI-credit spend in the shared column frame,
/// auto-scaled to the busiest day (credits are a spend quantity, not a
/// utilization percentage). Historical by design — a ledger of spend that
/// already happened, never a remaining-quota meter. A zero day on the
/// baseline renders as a faint dot so "no spend" reads differently from
/// "tiny spend".
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
pub(in crate::ui) fn credits_chart_lines(
    summary: &Summary,
    width: u16,
    body_height: usize,
) -> Vec<Line<'static>> {
    let Some(credits) = &summary.credits else {
        return Vec::new();
    };
    let Some((peak_date, peak_value)) = credits.peak else {
        return Vec::new();
    };
    if credits.days.is_empty() || peak_value <= 0.0 {
        return Vec::new();
    }
    let annotation = format!(
        "30d total {total} · peak {peak} · {month} {day}",
        total = utils::format_credits(credits.total),
        peak = utils::format_credits(peak_value),
        month = utils::month_abbrev(peak_date.month()),
        day = peak_date.day(),
    );

    let columns: Vec<ChartColumn> = credits
        .days
        .iter()
        .map(|(_, value)| ChartColumn {
            level: level_for(*value, peak_value, body_height),
            color: if *value > 0.0 {
                theme::GREEN
            } else {
                theme::DIM
            },
            baseline: "·",
        })
        .collect();
    let day_count = credits.days.len();
    let x_points: Vec<(usize, String)> = [0.0, 0.5, 1.0]
        .iter()
        .filter_map(|fraction| {
            let index = ((day_count.saturating_sub(1)) as f64 * fraction).round() as usize;
            let (date, _) = credits.days.get(index)?;
            Some((
                index,
                format!("{} {}", utils::month_abbrev(date.month()), date.day()),
            ))
        })
        .collect();
    column_chart_lines(
        "CREDITS",
        &annotation,
        &[
            utils::format_credits(peak_value),
            utils::format_credits(peak_value / 2.0),
            "0".to_owned(),
        ],
        &columns,
        &x_points,
        width,
        body_height,
    )
}
