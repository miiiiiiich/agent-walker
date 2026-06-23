use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};

use crate::format::format_count;
use crate::model::Summary;

use super::hero;
use super::modal;
use super::page;
use super::state::UiState;
use super::theme::{BLACK, DIM, HOT, MUTED, TEXT};

pub(super) fn draw(frame: &mut Frame<'_>, state: &UiState) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(TEXT).bg(BLACK)),
        area,
    );
    let padded = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let summary = state.current_summary();
    let width = padded.width;
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .split(padded);

    // Only the tab bar and footer stay fixed; everything from the hero line
    // down lives in one scrollable page.
    frame.render_widget(Paragraph::new(hero::header_line(state, width)), rows[0]);
    // The orchestration tier (Scout/Tools/Parallel/Apex) is a whole-person trait,
    // so every tab takes it from the combined summary — parallelism and tooling
    // are measured across all agents at once. The row/animal still vary per tab.
    let lines = page::page_lines(summary, &state.report.combined, width);
    let scroll = clamp_scroll(state, lines.len(), rows[1].height);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), rows[1]);
    draw_footer(frame, rows[2], state, summary);

    if state.share.is_some() {
        modal::draw_share_modal(frame, padded, state);
    }
}

/// Record how far the sections can scroll and clamp the current offset.
fn clamp_scroll(state: &UiState, content_lines: usize, viewport_height: u16) -> u16 {
    let max_scroll = u16::try_from(content_lines)
        .unwrap_or(u16::MAX)
        .saturating_sub(viewport_height);
    state.max_scroll.set(max_scroll);
    state.scroll.min(max_scroll)
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, state: &UiState, summary: &Summary) {
    let mut key_spans = vec![
        Span::styled("←→", Style::default().fg(MUTED)),
        Span::styled(" provider   ", Style::default().fg(DIM)),
    ];
    if state.max_scroll.get() > 0 {
        key_spans.push(Span::styled("↑↓", Style::default().fg(MUTED)));
        key_spans.push(Span::styled(" scroll   ", Style::default().fg(DIM)));
    }
    key_spans.extend([
        Span::styled("s", Style::default().fg(MUTED)),
        Span::styled(" share   ", Style::default().fg(DIM)),
        Span::styled("r", Style::default().fg(MUTED)),
        Span::styled(" reload   ", Style::default().fg(DIM)),
        Span::styled("q", Style::default().fg(MUTED)),
        Span::styled(" quit", Style::default().fg(DIM)),
    ]);
    let keys = Line::from(key_spans);

    let scan = if state.status.is_empty() {
        Line::from(Span::styled(
            format!(
                "{} files · {} lines · loaded in {}ms",
                format_count(summary.scan_stats.files_seen),
                format_count(summary.scan_stats.lines_seen),
                state.report.load_duration_ms
            ),
            Style::default().fg(DIM),
        ))
    } else {
        Line::from(Span::styled(state.status.clone(), Style::default().fg(HOT)))
    };

    frame.render_widget(Paragraph::new(keys.clone()), area);
    // Right-side stats only when they fit beside the key hints.
    if keys.width() + scan.width() + 3 <= usize::from(area.width) {
        frame.render_widget(Paragraph::new(scan).alignment(Alignment::Right), area);
    }
}
