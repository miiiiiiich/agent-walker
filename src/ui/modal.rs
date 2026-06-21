use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph};

use super::state::{SHARE_ACTIONS, UiState};
use super::theme::{BLACK, DIM, GOLD, MUTED, TEXT};

/// Centered share modal: pick a target.
pub(super) fn draw_share_modal(frame: &mut Frame<'_>, area: Rect, state: &UiState) {
    let Some(modal) = state.share.as_ref() else {
        return;
    };
    let width = 46_u16.min(area.width.saturating_sub(4));
    let height = 8_u16.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, modal_area);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("▌", Style::default().fg(GOLD)),
            Span::styled(
                " SHARE",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  your last stats card", Style::default().fg(MUTED)),
        ]),
        Line::default(),
    ];
    for (index, label) in SHARE_ACTIONS.iter().enumerate() {
        let selected = index == modal.selected;
        let marker = if selected { "›" } else { " " };
        let style = if selected {
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), Style::default().fg(GOLD)),
            Span::styled((*label).to_owned(), style),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "   ↑↓ pick · enter · esc close",
        Style::default().fg(DIM),
    )));

    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(GOLD))
        .style(Style::default().bg(BLACK));
    frame.render_widget(Paragraph::new(lines).block(block), modal_area);
}
