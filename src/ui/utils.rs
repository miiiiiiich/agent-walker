use ratatui::prelude::*;
use time::{Date, Weekday};

use crate::format::format_count;
use crate::model::Summary;

use super::theme;

/// Key column width for kv-style rows, shrinking on narrow columns.
pub(super) fn kv_label_width(width: u16) -> usize {
    if width < 40 { 11 } else { 17 }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Bar width is a bounded terminal rendering concern."
)]
pub(super) fn bar_fill(value: u64, max: u64, width: usize) -> usize {
    if max == 0 {
        0
    } else {
        ((value as f64 / max as f64) * width as f64).round() as usize
    }
    .min(width)
}

/// The bar track every horizontal stat row shares: a filled head and a
/// FAINT remainder, both `▄` — one look for every section's bars. New
/// sections must build their bars from this, not raw span pairs.
pub(super) fn bar_track(filled: usize, width: usize, color: Color) -> [Span<'static>; 2] {
    let filled = filled.min(width);
    [
        Span::styled("▄".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "▄".repeat(width - filled),
            Style::default().fg(theme::FAINT),
        ),
    ]
}

/// The canonical horizontal stat row: 14-char label, shared bar track, bold
/// 8-char value, muted 7-char share (empty omits the column). List sections
/// (MODELS / SKILLS and future ones) use this shape as-is so the dashboard
/// stays visually uniform.
pub(super) fn stat_bar_line(
    label: &str,
    color: Color,
    filled: usize,
    bar_width: usize,
    value: &str,
    share: &str,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{:<14}", compact_label(label, 13)),
        Style::default().fg(theme::TEXT),
    )];
    spans.extend(bar_track(filled, bar_width, color));
    spans.push(Span::styled(
        format!(" {value:>8}"),
        Style::default()
            .fg(theme::TEXT)
            .add_modifier(Modifier::BOLD),
    ));
    if !share.is_empty() {
        spans.push(Span::styled(
            format!("{share:>7}"),
            Style::default().fg(theme::MUTED),
        ));
    }
    Line::from(spans)
}

pub(super) fn section_title(title: &'static str, annotation: &str) -> Line<'static> {
    let mut spans = vec![
        Span::styled("▍ ", Style::default().fg(theme::GOLD)),
        Span::styled(
            title,
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !annotation.is_empty() {
        spans.push(Span::styled(
            format!("  {annotation}"),
            Style::default().fg(theme::DIM),
        ));
    }
    Line::from(spans)
}

/// Bar track length for a column: fixed label (14) + count (8) columns,
/// the bar absorbs the rest.
pub(super) fn bar_width_for(width: u16) -> usize {
    usize::from(width).saturating_sub(22).clamp(8, 24)
}

pub(super) fn weekday_index(date: Date) -> u8 {
    match date.weekday() {
        Weekday::Monday => 0,
        Weekday::Tuesday => 1,
        Weekday::Wednesday => 2,
        Weekday::Thursday => 3,
        Weekday::Friday => 4,
        Weekday::Saturday => 5,
        Weekday::Sunday => 6,
    }
}

/// Compact credit formatting: two significant decimals under 10, one under
/// 100, whole numbers beyond ("0.35", "12.4", "180").
pub(super) fn format_credits(credits: f64) -> String {
    if credits < 10.0 {
        format!("{credits:.2}")
    } else if credits < 100.0 {
        format!("{credits:.1}")
    } else {
        format!("{credits:.0}")
    }
}

pub(super) fn month_abbrev(month: time::Month) -> &'static str {
    match month {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    }
}

pub(super) fn kv(label: &str, value: &str, label_width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<label_width$}"),
            Style::default().fg(theme::MUTED),
        ),
        Span::styled(value.to_owned(), Style::default().fg(theme::TEXT)),
    ])
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Bar width is a bounded terminal rendering concern."
)]
pub(super) fn count_bar_line(
    label: &str,
    value: usize,
    max: usize,
    width: usize,
    color: Color,
) -> Line<'static> {
    let filled = if max == 0 {
        0
    } else {
        ((value as f64 / max as f64) * width as f64).round() as usize
    }
    .min(width);
    let label = compact_label(label, 13);
    let mut spans = vec![Span::styled(
        format!("{label:<14}"),
        Style::default().fg(theme::TEXT),
    )];
    spans.extend(bar_track(filled, width, color));
    spans.push(Span::styled(
        format!(" {:>6}", format_count(value)),
        Style::default().fg(theme::MUTED),
    ));
    Line::from(spans)
}

/// Truncate keeping the END of the label — repository names differ at the
/// tail ("…-genkan-app"), not the head.
pub(super) fn compact_label_tail(label: &str, width: usize) -> String {
    let count = label.chars().count();
    if count <= width {
        return label.to_owned();
    }
    let keep = width.saturating_sub(1);
    let mut value = String::from("…");
    value.extend(label.chars().skip(count - keep));
    value
}

pub(super) fn compact_label(label: &str, width: usize) -> String {
    if label.chars().count() <= width {
        return label.to_owned();
    }
    let keep = width.saturating_sub(1);
    let mut value = label.chars().take(keep).collect::<String>();
    value.push('…');
    value
}

pub(super) fn token_usage_available(summary: &Summary) -> bool {
    summary.total_usage.token_volume() > 0
}
