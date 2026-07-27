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

/// Tokens by hour of day as hand-rendered bars with a labelled y-axis; the
/// peak hour glows gold.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
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
    let graph_available = usize::from(width).saturating_sub(7);
    let chars_per_bar = if graph_available >= 48 { 2 } else { 1 };
    let height = body_height.max(1);
    let half_cells = height * 2;

    let annotation = summary
        .busiest_hour
        .map_or_else(String::new, |(hour, usage)| {
            if width < 40 {
                format!("peak {hour:02}:00")
            } else {
                format!("peak {hour:02}:00 · {}", format_tokens(usage))
            }
        });
    let mut out = vec![utils::section_title("BY HOUR", &annotation)];

    let levels: Vec<usize> = summary
        .hourly_usage
        .iter()
        .map(|value| {
            if *value == 0 {
                0
            } else {
                ((*value as f64 / max as f64) * half_cells as f64)
                    .round()
                    .max(1.0) as usize
            }
        })
        .collect();

    for row in 0..height {
        let label = if row == 0 {
            format!("{:>6}", format_tokens(max))
        } else if row == height / 2 {
            format!(
                "{:>6}",
                format_tokens((max as f64 * (height - height / 2) as f64 / height as f64) as u64)
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
        let half_bottom = 2 * (height - 1 - row);
        let half_top = half_bottom + 1;
        for (hour, level) in levels.iter().enumerate() {
            let glyph = if *level > half_top {
                "█"
            } else if *level > half_bottom {
                "▄"
            } else {
                " "
            };
            let color = if peak_hour == Some(hour) {
                theme::GOLD
            } else {
                theme::BLUE
            };
            spans.push(Span::styled(
                glyph.repeat(chars_per_bar),
                Style::default().fg(color),
            ));
        }
        out.push(Line::from(spans));
    }

    let points: Vec<(usize, String)> = (0..=6)
        .map(|step| {
            let hour = step * 4;
            let column = (hour * chars_per_bar).min(24 * chars_per_bar - 1);
            (7 + column, format!("{hour:02}"))
        })
        .collect();
    out.push(axis_label_row(width, &points));
    out
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
    const Y_WIDTH: usize = 7; // 6-char label column + axis bar
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

/// LIMITS history: daily peak of the plan's 5h window as BY HOUR-style bars.
/// The y-axis is FIXED at 0-100% (unlike the auto-scaled charts) so a quiet
/// month doesn't inflate a 3% day into a full column. A day that hit the
/// limit renders red; a day with no provider use renders as a faint dot; a
/// day with use but no recorded sample stays blank.
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
    let height = body_height.max(1);
    let half_cells = height * 2;
    let day_count = limits.days.len();
    let graph_available = usize::from(width).saturating_sub(7);
    let chars_per_bar = if graph_available >= day_count * 2 {
        2
    } else {
        1
    };

    let annotation = limits.peak.map_or_else(String::new, |(date, value)| {
        format!(
            "peak {value:.0}% · {} {}",
            utils::month_abbrev(date.month()),
            date.day()
        )
    });
    let mut out = vec![utils::section_title("LIMITS", &annotation)];

    let level_of = |day: &LimitDay| -> usize {
        match day {
            LimitDay::Measured(value) if *value > 0.0 => {
                ((value / 100.0) * half_cells as f64).round().max(1.0) as usize
            }
            _ => 0,
        }
    };

    for row in 0..height {
        let label = match row {
            0 => format!("{:>5}%", 100),
            _ if row == height / 2 => format!("{:>5}%", 50),
            _ if row == height - 1 => format!("{:>6}", 0),
            _ => " ".repeat(6),
        };
        let mut spans = vec![
            Span::styled(label, Style::default().fg(theme::MUTED)),
            Span::styled("│", Style::default().fg(theme::DIM)),
        ];
        let half_bottom = 2 * (height - 1 - row);
        let half_top = half_bottom + 1;
        let baseline = row == height - 1;
        for (_, day) in &limits.days {
            let level = level_of(day);
            let glyph = if level > half_top {
                "█"
            } else if level > half_bottom {
                "▄"
            } else if baseline {
                match day {
                    // Measured 0% is a real data point, not an absence.
                    LimitDay::Measured(_) => "▁",
                    LimitDay::NoUse => "·",
                    LimitDay::NoSample => " ",
                }
            } else {
                " "
            };
            let color = match day {
                LimitDay::Measured(value) if *value >= 99.5 => theme::HOT,
                LimitDay::Measured(_) => theme::GREEN,
                _ => theme::DIM,
            };
            spans.push(Span::styled(
                glyph.repeat(chars_per_bar),
                Style::default().fg(color),
            ));
        }
        out.push(Line::from(spans));
    }

    let points: Vec<(usize, String)> = [0.0, 0.5, 1.0]
        .iter()
        .filter_map(|fraction| {
            let index = ((day_count.saturating_sub(1)) as f64 * fraction).round() as usize;
            let (date, _) = limits.days.get(index)?;
            Some((
                7 + index * chars_per_bar,
                format!("{} {}", utils::month_abbrev(date.month()), date.day()),
            ))
        })
        .collect();
    out.push(axis_label_row(width, &points));
    out
}

/// CREDITS history: daily AI-credit spend as BY HOUR-style bars, auto-scaled
/// to the busiest day (credits are a spend quantity, not a utilization
/// percentage). Historical by design — a ledger of spend that already
/// happened, never a remaining-quota meter. A zero day on the baseline
/// renders as a faint dot so "no spend" reads differently from "tiny spend".
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
    let height = body_height.max(1);
    let half_cells = height * 2;
    let day_count = credits.days.len();
    let graph_available = usize::from(width).saturating_sub(7);
    let chars_per_bar = if graph_available >= day_count * 2 {
        2
    } else {
        1
    };

    let annotation = format!(
        "30d total {total} · peak {peak} · {month} {day}",
        total = utils::format_credits(credits.total),
        peak = utils::format_credits(peak_value),
        month = utils::month_abbrev(peak_date.month()),
        day = peak_date.day(),
    );
    let mut out = vec![utils::section_title("CREDITS", &annotation)];

    for row in 0..height {
        let label = match row {
            0 => format!("{:>6}", utils::format_credits(peak_value)),
            _ if row == height / 2 => format!("{:>6}", utils::format_credits(peak_value / 2.0)),
            _ if row == height - 1 => format!("{:>6}", 0),
            _ => " ".repeat(6),
        };
        let mut spans = vec![
            Span::styled(label, Style::default().fg(theme::MUTED)),
            Span::styled("│", Style::default().fg(theme::DIM)),
        ];
        let half_bottom = 2 * (height - 1 - row);
        let half_top = half_bottom + 1;
        let baseline = row == height - 1;
        for (_, value) in &credits.days {
            let level = if *value > 0.0 {
                ((value / peak_value) * half_cells as f64).round().max(1.0) as usize
            } else {
                0
            };
            let glyph = if level > half_top {
                "█"
            } else if level > half_bottom {
                "▄"
            } else if baseline {
                if *value > 0.0 { "▁" } else { "·" }
            } else {
                " "
            };
            let color = if *value > 0.0 {
                theme::GREEN
            } else {
                theme::DIM
            };
            spans.push(Span::styled(
                glyph.repeat(chars_per_bar),
                Style::default().fg(color),
            ));
        }
        out.push(Line::from(spans));
    }

    let points: Vec<(usize, String)> = [0.0, 0.5, 1.0]
        .iter()
        .filter_map(|fraction| {
            let index = ((day_count.saturating_sub(1)) as f64 * fraction).round() as usize;
            let (date, _) = credits.days.get(index)?;
            Some((
                7 + index * chars_per_bar,
                format!("{} {}", utils::month_abbrev(date.month()), date.day()),
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
