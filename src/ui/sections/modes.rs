use crate::format::format_percent;
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

/// MODES: how the model is allowed to think, per provider dial — Claude
/// shows the thinking fire rate (plus fast mode once it has data) and its
/// reasoning-effort mix, Codex the reasoning-effort mix alone. Deliberately
/// small (a couple of rows): the dials are asymmetric across providers, so
/// each tab renders only its own rows.
pub(in crate::ui) fn modes_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let modes = &summary.modes;
    if modes.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![utils::section_title("MODES", "30d")];
    let bar_width = usize::from(width).saturating_sub(30).clamp(6, 16);

    if modes.assistant_turns > 0 {
        let thinking = u64::try_from(modes.thinking_turns).unwrap_or(0);
        let turns = u64::try_from(modes.assistant_turns).unwrap_or(1).max(1);
        let filled = utils::bar_fill(thinking, turns, bar_width);
        let mut spans = vec![Span::styled("thinking  ", Style::default().fg(theme::TEXT))];
        spans.extend(utils::bar_track(filled, bar_width, theme::PURPLE));
        spans.push(Span::styled(
            format!(" {:>6}", format_percent(thinking, turns)),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" of turns", Style::default().fg(theme::MUTED)));
        lines.push(Line::from(spans));
    }

    if modes.fast_turns > 0 && modes.assistant_turns > 0 {
        let fast = u64::try_from(modes.fast_turns).unwrap_or(0);
        let turns = u64::try_from(modes.assistant_turns).unwrap_or(1).max(1);
        let filled = utils::bar_fill(fast, turns, bar_width);
        let mut spans = vec![Span::styled("fast      ", Style::default().fg(theme::TEXT))];
        spans.extend(utils::bar_track(filled, bar_width, theme::GOLD));
        spans.push(Span::styled(
            format!(" {:>6}", format_percent(fast, turns)),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" of turns", Style::default().fg(theme::MUTED)));
        lines.push(Line::from(spans));
    }

    if !modes.efforts.is_empty() {
        let total: usize = modes.efforts.iter().map(|(_, count)| count).sum();
        let total = u64::try_from(total).unwrap_or(1).max(1);
        let mut spans = vec![Span::styled("effort    ", Style::default().fg(theme::TEXT))];
        for (index, (label, count)) in modes.efforts.iter().take(3).enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", Style::default().fg(theme::DIM)));
            }
            let style = if index == 0 {
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::MUTED)
            };
            spans.push(Span::styled(
                format!(
                    "{label} {}",
                    format_percent(u64::try_from(*count).unwrap_or(0), total)
                ),
                style,
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}
