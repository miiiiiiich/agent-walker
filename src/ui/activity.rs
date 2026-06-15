use std::collections::BTreeMap;

use ratatui::prelude::*;
use time::{Date, Duration};

use crate::format::format_date;
use crate::model::Summary;

use super::theme;
use super::utils;

pub(super) fn activity_lines(summary: &Summary) -> Vec<Line<'static>> {
    let mut title = utils::section_title(
        "ACTIVITY",
        &format!(
            "{} – {}",
            format_date(summary.period_start),
            format_date(summary.period_end)
        ),
    );
    title
        .spans
        .push(Span::styled("   less ", Style::default().fg(theme::DIM)));
    title
        .spans
        .push(Span::styled("▄", Style::default().fg(theme::HEAT_ZERO)));
    for color in theme::HEAT_RAMP {
        title.spans.push(Span::raw(" "));
        title
            .spans
            .push(Span::styled("▄", Style::default().fg(color)));
    }
    title
        .spans
        .push(Span::styled(" more", Style::default().fg(theme::DIM)));
    let mut lines = vec![title];

    if !utils::token_usage_available(summary) && summary.scan_stats.lines_seen > 0 {
        lines.push(Line::from(Span::styled(
            "No token-volume heatmap for this provider — activity below uses session touches only.",
            Style::default().fg(theme::MUTED),
        )));
        lines.extend(session_heatmap(summary));
        return lines;
    }

    lines.extend(usage_heatmap(summary));
    lines
}

/// GitHub-style weekly grid driven by token volume.
fn usage_heatmap(summary: &Summary) -> Vec<Line<'static>> {
    let usage_by_date = summary
        .daily
        .iter()
        .map(|day| (day.date, day.usage.token_volume()))
        .collect::<BTreeMap<_, _>>();
    heatmap_grid(summary, &usage_by_date)
}

/// Fallback heatmap from per-day session counts (providers without usage numbers).
fn session_heatmap(summary: &Summary) -> Vec<Line<'static>> {
    let sessions_by_date = summary
        .daily_sessions
        .iter()
        .map(|day| (day.date, u64::try_from(day.sessions).unwrap_or(u64::MAX)))
        .collect::<BTreeMap<_, _>>();
    heatmap_grid(summary, &sessions_by_date)
}

/// Grass grid with guaranteed square cells: "▄" is 1 char wide x 1/2 line
/// tall = 1:1 on a ~1:2 terminal cell. The one-char horizontal gutter and
/// the empty upper half-line are the smallest gaps the character lattice
/// allows without giving up squareness.
fn heatmap_grid(summary: &Summary, value_by_date: &BTreeMap<Date, u64>) -> Vec<Line<'static>> {
    const CELL_PITCH: usize = 2; // 1-char cell + 1-char gap
    let thresholds = heat_thresholds(value_by_date);
    let start = summary.period_start
        - Duration::days(i64::from(utils::weekday_index(summary.period_start)));
    let weeks = ((summary.period_end - start).whole_days() / 7 + 1).max(1);

    let mut lines = Vec::new();

    // Month markers aligned to week columns.
    let mut months = " ".repeat(5);
    let mut last_month = None;
    for week in 0..weeks {
        let month = (start + Duration::days(week * 7)).month();
        if last_month.is_none_or(|last| last != month) {
            last_month = Some(month);
            let position = 5 + usize::try_from(week).unwrap_or(0) * CELL_PITCH;
            if position >= months.chars().count() {
                while months.chars().count() < position {
                    months.push(' ');
                }
                months.push_str(utils::month_abbrev(month));
                months.push(' ');
            }
        }
    }
    lines.push(Line::from(Span::styled(
        months,
        Style::default().fg(theme::MUTED),
    )));

    for weekday in 0..7 {
        let label = match weekday {
            0 => "Mon",
            2 => "Wed",
            4 => "Fri",
            6 => "Sun",
            _ => "",
        };
        let mut spans = vec![Span::styled(
            format!("{label:<5}"),
            Style::default().fg(theme::DIM),
        )];
        for week in 0..weeks {
            let date = start + Duration::days(week * 7 + weekday);
            if date < summary.period_start || date > summary.period_end {
                spans.push(Span::raw("  "));
                continue;
            }
            let value = value_by_date.get(&date).copied().unwrap_or(0);
            spans.push(Span::styled(
                "▄",
                Style::default().fg(heat_color(value, &thresholds)),
            ));
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Quartile thresholds over the non-zero days. Quantile bucketing keeps the
/// four greens evenly used even when one outlier day dwarfs the rest —
/// linear max-scaling collapsed everything else into the darkest shade.
fn heat_thresholds(value_by_date: &BTreeMap<Date, u64>) -> Vec<u64> {
    let mut values = value_by_date
        .values()
        .copied()
        .filter(|value| *value > 0)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Vec::new();
    }
    values.sort_unstable();
    [25, 50, 75]
        .iter()
        .map(|quantile| values[(values.len() - 1) * quantile / 100])
        .collect()
}

fn heat_color(value: u64, thresholds: &[u64]) -> Color {
    if value == 0 || thresholds.is_empty() {
        return theme::HEAT_ZERO;
    }
    let bucket = thresholds
        .iter()
        .filter(|threshold| value > **threshold)
        .count();
    theme::HEAT_RAMP[bucket.min(theme::HEAT_RAMP.len() - 1)]
}
