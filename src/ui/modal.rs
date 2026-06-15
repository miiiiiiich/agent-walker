use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::share::Variant;

use super::state::{SHARE_ACTIONS, UiState};
use super::theme::{BLACK, DIM, GOLD, MUTED, TEXT};

/// Centered share modal: pick a target, toggle project visibility.
pub(super) fn draw_share_modal(frame: &mut Frame<'_>, area: Rect, state: &UiState) {
    let Some(modal) = state.share.as_ref() else {
        return;
    };
    let width = 46_u16.min(area.width.saturating_sub(4));
    let height = 12_u16.min(area.height.saturating_sub(2));
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
    let full = modal.variant == Variant::Full;
    lines.push(Line::from(vec![
        Span::styled("   card: ", Style::default().fg(MUTED)),
        Span::styled(
            "summary",
            Style::default()
                .fg(if full { DIM } else { GOLD })
                .add_modifier(if full {
                    Modifier::empty()
                } else {
                    Modifier::BOLD
                }),
        ),
        Span::styled(" / ", Style::default().fg(DIM)),
        Span::styled(
            "full",
            Style::default()
                .fg(if full { GOLD } else { DIM })
                .add_modifier(if full {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::styled("  (←→ switch)", Style::default().fg(DIM)),
    ]));
    lines.push(Line::from(Span::styled(
        if full {
            "   full adds your project names"
        } else {
            "   summary hides project names"
        },
        Style::default().fg(DIM),
    )));
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
