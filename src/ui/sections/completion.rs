use crate::format::{format_count, format_duration_ms};
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

pub(in crate::ui) fn duration_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let Some(duration) = &summary.completion_duration else {
        return Vec::new();
    };
    let max = duration
        .buckets
        .iter()
        .map(|bucket| bucket.count)
        .max()
        .unwrap_or(0);
    // Autonomy signal: how often a turn ran 20+ minutes unattended.
    let autonomous: usize = duration
        .buckets
        .iter()
        .skip(3)
        .map(|bucket| bucket.count)
        .sum();
    let mut lines = vec![
        utils::section_title(
            "COMPLETION",
            &format!(
                "{} turns · {} ran ≥20m",
                format_count(duration.count),
                format_count(autonomous)
            ),
        ),
        Line::from(vec![
            Span::styled("p50 ", Style::default().fg(theme::MUTED)),
            Span::styled(
                format_duration_ms(duration.p50_ms),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("   p90 ", Style::default().fg(theme::MUTED)),
            Span::styled(
                format_duration_ms(duration.p90_ms),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled("   max ", Style::default().fg(theme::MUTED)),
            Span::styled(
                format_duration_ms(duration.max_ms),
                Style::default().fg(theme::TEXT),
            ),
        ]),
    ];
    let bar_width = utils::bar_width_for(width);
    for bucket in &duration.buckets {
        lines.push(utils::count_bar_line(
            &bucket.label,
            bucket.count,
            max,
            bar_width,
            theme::BLUE,
        ));
    }
    lines
}
