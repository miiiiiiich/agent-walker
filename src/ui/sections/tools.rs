use crate::format::format_count;
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

pub(in crate::ui) fn tool_lines(summary: &Summary, width: u16, limit: usize) -> Vec<Line<'static>> {
    if summary.tools.is_empty() {
        return vec![
            utils::section_title("TOOLS", ""),
            Line::from(Span::styled(
                "No tool calls found",
                Style::default().fg(theme::MUTED),
            )),
        ];
    }
    let total_calls: usize = summary.tools.iter().map(|tool| tool.calls).sum();
    let max = summary
        .tools
        .iter()
        .map(|tool| tool.calls)
        .max()
        .unwrap_or(0);
    let mut lines = vec![utils::section_title(
        "TOOLS",
        &format!("{} calls", format_count(total_calls)),
    )];
    let bar_width = utils::bar_width_for(width);
    lines.extend(
        summary.tools.iter().take(limit).map(|tool| {
            utils::count_bar_line(&tool.name, tool.calls, max, bar_width, theme::GREEN)
        }),
    );
    let hidden = summary.tools.len().saturating_sub(limit);
    if hidden > 0 {
        let hidden_calls: usize = summary
            .tools
            .iter()
            .skip(limit)
            .map(|tool| tool.calls)
            .sum();
        lines.push(Line::from(Span::styled(
            format!("+{hidden} more · {} calls", format_count(hidden_calls)),
            Style::default().fg(theme::DIM),
        )));
    }
    lines
}
