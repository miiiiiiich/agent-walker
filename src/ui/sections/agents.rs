use crate::format::format_tokens;
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

pub(in crate::ui) fn agent_lines(
    summary: &Summary,
    width: u16,
    limit: usize,
) -> Vec<Line<'static>> {
    let with_usage = utils::token_usage_available(summary);
    let show_calls = width >= 40;
    let name_width = usize::from(width)
        .saturating_sub(if show_calls { 20 } else { 10 })
        .clamp(10, 18);
    let mut lines = vec![utils::section_title("SUBAGENTS", "by token volume")];
    for agent in summary.agents.iter().take(limit) {
        let mut spans = vec![Span::styled(
            format!(
                "{:<width$}",
                utils::compact_label(&agent.name, name_width.saturating_sub(1)),
                width = name_width
            ),
            Style::default().fg(theme::TEXT),
        )];
        if with_usage {
            spans.push(Span::styled(
                format!("{:>8}", format_tokens(agent.usage.token_volume())),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if show_calls && agent.calls > 0 {
            spans.push(Span::styled(
                format!("  {} calls", agent.calls),
                Style::default().fg(theme::MUTED),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}
