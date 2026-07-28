use crate::format::format_tokens;
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

/// Top repositories by token volume — where the AI time actually goes.
pub(in crate::ui) fn project_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    if summary.projects.is_empty() {
        return Vec::new();
    }
    let max = summary
        .projects
        .iter()
        .map(|project| project.usage.token_volume())
        .max()
        .unwrap_or(0);
    if max == 0 {
        return Vec::new();
    }

    // Repository names need more label room than tool names; trade bar length.
    let label_width = 20_usize;
    let bar_width = usize::from(width)
        .saturating_sub(label_width + 9)
        .clamp(8, 24);
    let mut lines = vec![utils::section_title("PROJECTS", "by token volume")];
    for project in summary.projects.iter().take(6) {
        let label = utils::compact_label_tail(&project.name, label_width - 1);
        let value = project.usage.token_volume();
        let filled = utils::bar_fill(value, max, bar_width);
        let mut spans = vec![Span::styled(
            format!("{label:<label_width$}"),
            Style::default().fg(theme::TEXT),
        )];
        spans.extend(utils::bar_track(filled, bar_width, theme::ACCENT));
        spans.push(Span::styled(
            format!(" {:>7}", format_tokens(value)),
            Style::default().fg(theme::MUTED),
        ));
        lines.push(Line::from(spans));
    }
    let hidden = summary.projects.len().saturating_sub(6);
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("+{hidden} more"),
            Style::default().fg(theme::DIM),
        )));
    }
    lines
}
