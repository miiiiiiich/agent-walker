use ratatui::prelude::*;

use crate::format::format_tokens;
use crate::model::Summary;

use super::{ChartColumn, column_chart_lines, level_for};
use crate::ui::theme;

pub(in crate::ui) fn hourly_chart_lines(
    summary: &Summary,
    width: u16,
    body_height: usize,
) -> Vec<Line<'static>> {
    let max = summary.hourly_usage.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return Vec::new();
    }
    let peak_hour = summary.busiest_hour.map(|(hour, _)| usize::from(hour));
    let annotation = summary
        .busiest_hour
        .map_or_else(String::new, |(hour, usage)| {
            if width < 40 {
                format!("peak {hour:02}:00")
            } else {
                format!("peak {hour:02}:00 · {}", format_tokens(usage))
            }
        });

    #[allow(
        clippy::cast_precision_loss,
        reason = "Chart geometry is display-only."
    )]
    let columns: Vec<ChartColumn> = summary
        .hourly_usage
        .iter()
        .enumerate()
        .map(|(hour, value)| ChartColumn {
            level: level_for(*value as f64, max as f64, body_height),
            color: if peak_hour == Some(hour) {
                theme::GOLD
            } else {
                theme::BLUE
            },
            baseline: " ",
        })
        .collect();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "Chart geometry is display-only."
    )]
    let mid = (max as f64 / 2.0) as u64;
    let x_points: Vec<(usize, String)> = (0..=6)
        .map(|step| {
            let hour = step * 4;
            (hour.min(23), format!("{hour:02}"))
        })
        .collect();
    column_chart_lines(
        "BY HOUR",
        &annotation,
        &[format_tokens(max), format_tokens(mid), "0".to_owned()],
        &columns,
        &x_points,
        width,
        body_height,
    )
}
