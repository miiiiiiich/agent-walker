use std::collections::BTreeMap;

use ratatui::prelude::*;

use crate::format::format_tokens;
use crate::model::Summary;

use super::{Y_AXIS_WIDTH, axis_label_row};
use crate::ui::{theme, utils};

/// Daily volume as stacked per-model bars, rendered by hand: one column per
/// day (or per day-bucket on narrow terminals), each half-cell colored by
/// the segment that owns it. The Chart widget painter-stacking left rounding
/// artifacts (floating caps, bleeding columns); exact half-cell assignment
/// cannot.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
#[allow(
    clippy::too_many_lines,
    reason = "Flat renderer: bucketing, scaling, and cell painting in one pass."
)]
pub(in crate::ui) fn model_chart_lines(
    summary: &Summary,
    width: u16,
    body_height: usize,
) -> Vec<Line<'static>> {
    const Y_WIDTH: usize = Y_AXIS_WIDTH;
    let day_count = summary.daily.len();
    let graph_width = usize::from(width).saturating_sub(Y_WIDTH).max(1);
    let height = body_height.max(1);
    if day_count == 0 {
        return Vec::new();
    }
    let chunk = day_count.div_ceil(graph_width);
    let columns = day_count.div_ceil(chunk);

    let mut out = vec![utils::section_title(
        "TOKENS PER DAY",
        if chunk > 1 {
            "stacked by model · bucket avg"
        } else {
            "stacked by model"
        },
    )];

    // Per-segment per-column mean volume: top models, then the remainder.
    let top_models: Vec<_> = summary
        .models
        .iter()
        .filter(|model| model.usage.token_volume() > 0)
        .take(6)
        .collect();
    let bucket_mean = |values: &[u64]| -> Vec<f64> {
        (0..columns)
            .map(|column| {
                let slice = &values[column * chunk..((column + 1) * chunk).min(values.len())];
                if slice.is_empty() {
                    0.0
                } else {
                    slice.iter().sum::<u64>() as f64 / slice.len() as f64
                }
            })
            .collect()
    };
    let daily_totals: Vec<u64> = summary
        .daily
        .iter()
        .map(|stat| stat.usage.token_volume())
        .collect();
    let totals = bucket_mean(&daily_totals);
    let mut segments: Vec<(Color, Vec<f64>)> = top_models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            (
                theme::model_color(index),
                bucket_mean(&model_daily_values(summary, &model.name)),
            )
        })
        .collect();
    let known: Vec<f64> = (0..columns)
        .map(|column| segments.iter().map(|(_, values)| values[column]).sum())
        .collect();
    if totals
        .iter()
        .zip(&known)
        .any(|(total, accounted)| *total > *accounted + 0.5)
    {
        segments.push((
            theme::DIM,
            totals
                .iter()
                .zip(&known)
                .map(|(total, accounted)| (total - accounted).max(0.0))
                .collect(),
        ));
    }

    let max_total = totals.iter().copied().fold(0.0_f64, f64::max).max(1.0);
    let half_cells = height * 2;
    let to_level = |value: f64| -> usize {
        let level = (value / max_total * half_cells as f64).round() as usize;
        if value > 0.0 {
            level.max(1).min(half_cells)
        } else {
            0
        }
    };

    // Cumulative segment boundaries per column, in half-cell units.
    let boundaries: Vec<Vec<usize>> = (0..columns)
        .map(|column| {
            let mut running = 0.0;
            segments
                .iter()
                .map(|(_, values)| {
                    running += values[column];
                    to_level(running)
                })
                .collect()
        })
        .collect();
    let color_at = |column: usize, half_index: usize| -> Option<Color> {
        boundaries[column]
            .iter()
            .position(|boundary| half_index < *boundary)
            .map(|segment| segments[segment].0)
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    for row in 0..height {
        let label = if row == 0 {
            format!("{:>6}", format_tokens(max_total as u64))
        } else if row == height / 2 {
            format!(
                "{:>6}",
                format_tokens((max_total * (height - height / 2) as f64 / height as f64) as u64)
            )
        } else if row == height - 1 {
            format!("{:>6}", 0)
        } else {
            " ".repeat(6)
        };
        let mut spans = vec![
            Span::styled(label, Style::default().fg(theme::MUTED)),
            Span::styled("│", Style::default().fg(theme::DIM)),
        ];
        let half_top = half_cells - 1 - 2 * row;
        let half_bottom = half_cells - 2 - 2 * row;
        for column in 0..columns {
            let top = color_at(column, half_top);
            let bottom = color_at(column, half_bottom);
            spans.push(match (top, bottom) {
                (None, None) => Span::raw(" "),
                (Some(color_top), Some(color_bottom)) if color_top == color_bottom => {
                    Span::styled("█", Style::default().fg(color_top))
                }
                (Some(color_top), Some(color_bottom)) => {
                    Span::styled("▀", Style::default().fg(color_top).bg(color_bottom))
                }
                (None, Some(color_bottom)) => Span::styled("▄", Style::default().fg(color_bottom)),
                (Some(color_top), None) => Span::styled("▀", Style::default().fg(color_top)),
            });
        }
        lines.push(Line::from(spans));
    }
    out.extend(lines);

    let points: Vec<(usize, String)> = [0.0, 0.25, 0.5, 0.75, 1.0]
        .iter()
        .filter_map(|fraction| {
            let index = ((day_count.saturating_sub(1)) as f64 * fraction).round() as usize;
            let day = summary.daily.get(index)?;
            // Center the label on the exact column that draws this day.
            Some((
                Y_WIDTH + index / chunk,
                format!(
                    "{} {}",
                    utils::month_abbrev(day.date.month()),
                    day.date.day()
                ),
            ))
        })
        .collect();
    out.push(axis_label_row(width, &points));
    out
}

fn model_daily_values(summary: &Summary, model_name: &str) -> Vec<u64> {
    let usage_by_date = summary
        .model_daily
        .iter()
        .filter(|day| day.model == model_name)
        .map(|day| (day.date, day.usage.token_volume()))
        .collect::<BTreeMap<_, _>>();
    summary
        .daily
        .iter()
        .map(|day| usage_by_date.get(&day.date).copied().unwrap_or(0))
        .collect()
}
