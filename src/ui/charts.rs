use std::collections::BTreeMap;

use ratatui::prelude::*;

use crate::format::format_tokens;
use crate::model::{LimitDay, Summary};

use super::theme;
use super::utils;

/// Hand-positioned x-axis label row. Each label is centered on an absolute
/// character column, computed by the caller from the same mapping that
/// placed the data — so labels cannot drift from the bars they annotate.
fn axis_label_row(width: u16, points: &[(usize, String)]) -> Line<'static> {
    let total = usize::from(width);
    let mut buffer = vec![' '; total];
    for (center, text) in points {
        let length = text.chars().count();
        if length > total {
            continue;
        }
        let start = center.saturating_sub(length / 2).min(total - length);
        for (index, character) in text.chars().enumerate() {
            buffer[start + index] = character;
        }
    }
    Line::from(Span::styled(
        buffer.into_iter().collect::<String>(),
        Style::default().fg(theme::MUTED),
    ))
}

/// Geometry shared by every vertical (column) chart: a 6-char y-label
/// column plus the axis bar, then exactly ONE character per column — the
/// deliberate density standard (wider 2-char bars read worse; user decision
/// 2026-07-28) — and an x-axis label row underneath. New column charts must
/// render through `column_chart_lines` so they inherit this frame.
pub(super) const Y_AXIS_WIDTH: usize = 7;

/// One column of a column chart.
pub(super) struct ChartColumn {
    /// Fill level in half-cells (`0..=2 * body_height`).
    pub level: usize,
    pub color: Color,
    /// Glyph drawn on the baseline row when the column is empty — charts
    /// distinguish "measured zero" (`▁`), "no data" (`·`), and blank.
    pub baseline: &'static str,
}

/// Render a column chart in the shared frame. `y_labels` are the top /
/// middle / bottom axis labels (right-aligned into the 6-char gutter);
/// `x_points` pair a column index with its label.
pub(super) fn column_chart_lines(
    title: &'static str,
    annotation: &str,
    y_labels: &[String; 3],
    columns: &[ChartColumn],
    x_points: &[(usize, String)],
    width: u16,
    body_height: usize,
) -> Vec<Line<'static>> {
    let height = body_height.max(1);
    let mut out = vec![utils::section_title(title, annotation)];
    for row in 0..height {
        let label = if row == 0 {
            format!("{:>6}", y_labels[0])
        } else if row == height / 2 {
            format!("{:>6}", y_labels[1])
        } else if row == height - 1 {
            format!("{:>6}", y_labels[2])
        } else {
            " ".repeat(6)
        };
        let mut spans = vec![
            Span::styled(label, Style::default().fg(theme::MUTED)),
            Span::styled("│", Style::default().fg(theme::DIM)),
        ];
        let half_bottom = 2 * (height - 1 - row);
        let half_top = half_bottom + 1;
        let baseline = row == height - 1;
        for column in columns {
            let glyph = if column.level > half_top {
                "█"
            } else if column.level > half_bottom {
                "▄"
            } else if baseline && column.level == 0 {
                column.baseline
            } else {
                " "
            };
            spans.push(Span::styled(
                glyph.to_owned(),
                Style::default().fg(column.color),
            ));
        }
        out.push(Line::from(spans));
    }
    let points: Vec<(usize, String)> = x_points
        .iter()
        .map(|(index, label)| (Y_AXIS_WIDTH + index, label.clone()))
        .collect();
    out.push(axis_label_row(width, &points));
    out
}

/// Half-cell fill level for a value against the chart maximum: zero stays
/// zero, anything positive shows at least one half-cell.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
pub(super) fn level_for(value: f64, max: f64, body_height: usize) -> usize {
    if value <= 0.0 || max <= 0.0 {
        return 0;
    }
    let half_cells = body_height.max(1) * 2;
    ((value / max) * half_cells as f64).round().max(1.0) as usize
}

pub(super) fn hourly_chart_lines(
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
pub(super) fn model_chart_lines(
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

/// LIMITS history: daily peak of the plan's 5h window in the shared column
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
pub(super) fn limits_chart_lines(
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
pub(super) fn credits_chart_lines(
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

/// Per-day volumes for one model, aligned to `summary.daily`'s date axis.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared column frame is the density contract: exactly one char per
    /// column after the 7-char y-axis gutter, so every column chart stacked
    /// in the left rail has the same width for the same day count.
    #[test]
    fn column_chart_body_width_is_axis_plus_one_char_per_column() {
        let columns: Vec<ChartColumn> = (0..30)
            .map(|index| ChartColumn {
                level: index % 13,
                color: theme::GREEN,
                baseline: "·",
            })
            .collect();
        let lines = column_chart_lines(
            "LIMITS",
            "",
            &["100%".to_owned(), "50%".to_owned(), "0".to_owned()],
            &columns,
            &[],
            120,
            6,
        );
        // Title + 6 body rows + axis row.
        assert_eq!(lines.len(), 8);
        for body in &lines[1..7] {
            assert_eq!(body.width(), Y_AXIS_WIDTH + columns.len());
        }
    }

    /// Baseline glyphs distinguish measured-zero, no-data, and blank — and
    /// only appear on the bottom row.
    #[test]
    fn baseline_glyphs_render_only_on_the_bottom_row() {
        let columns = vec![
            ChartColumn {
                level: 0,
                color: theme::GREEN,
                baseline: "▁",
            },
            ChartColumn {
                level: 0,
                color: theme::DIM,
                baseline: "·",
            },
            ChartColumn {
                level: 12,
                color: theme::GREEN,
                baseline: "▁",
            },
        ];
        let lines = column_chart_lines(
            "CREDITS",
            "",
            &["1".to_owned(), "0.5".to_owned(), "0".to_owned()],
            &columns,
            &[],
            80,
            6,
        );
        let row = |index: usize| -> String {
            lines[index]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        let bottom = row(6);
        assert!(bottom.ends_with("▁·█"));
        // No baseline glyph leaks into upper rows.
        assert!(!row(1).contains('▁') && !row(1).contains('·'));
    }
}
