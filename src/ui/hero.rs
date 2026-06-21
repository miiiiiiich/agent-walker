use ratatui::prelude::*;

use crate::format::{format_count, format_tokens};
use crate::model::Summary;

use super::state::UiState;
use super::theme;

pub(super) fn header_line(state: &UiState, width: u16) -> Line<'static> {
    let mut spans = vec![
        Span::styled("▌ ", Style::default().fg(theme::GOLD)),
        Span::styled(
            "Agent Walker",
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    // Tabs always fit before decoration: title 14 + tabs ≈ 4 labels × ~12.
    if width >= 84 {
        spans.push(Span::styled(
            format!("  last {} days", state.report.period_days),
            Style::default().fg(theme::MUTED),
        ));
        spans.push(Span::raw("  "));
    }

    // Provider tabs: provider-colored bar + underlined name marks the selection.
    for (index, (label, color)) in state.tabs().into_iter().enumerate() {
        spans.push(Span::raw("   "));
        if index == state.tab_index {
            spans.push(Span::styled("▍", Style::default().fg(color)));
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        } else {
            spans.push(Span::styled("▍", Style::default().fg(theme::FAINT)));
            spans.push(Span::styled(label, Style::default().fg(theme::MUTED)));
        }
    }
    Line::from(spans)
}

/// Progressively shed decoration, then secondary metrics, until the line fits.
pub(super) fn hero_line(summary: &Summary, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let full = build_hero(summary, "   ·   ", false);
    if full.width() <= width {
        return full;
    }
    build_hero(summary, " · ", true)
}

fn build_hero(summary: &Summary, separator: &'static str, compact: bool) -> Line<'static> {
    let mut spans = Vec::new();
    if summary.total_usage.token_volume() > 0 {
        push_hero(
            &mut spans,
            format_tokens(summary.total_usage.token_volume()),
            if compact { "tok" } else { "tokens" },
            theme::GOLD,
            separator,
        );
        if !compact
            && let Some(span) = delta_span(
                summary.total_usage.token_volume(),
                summary.previous_total_volume,
            )
        {
            spans.push(span);
        }
    }
    push_hero(
        &mut spans,
        format_count(summary.sessions),
        if compact { "sess" } else { "sessions" },
        theme::TEXT,
        separator,
    );
    push_hero(
        &mut spans,
        format!("{}/{}", summary.active_days, summary.period_days),
        if compact { "days" } else { "days active" },
        theme::TEXT,
        separator,
    );
    Line::from(spans)
}

/// Period-over-period delta badge ("↑12%"). None when there is no previous
/// data to compare against.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "Display-only percentage."
)]
fn delta_span(current: u64, previous: u64) -> Option<Span<'static>> {
    if previous == 0 {
        return None;
    }
    let percent = ((current as f64 - previous as f64) / previous as f64 * 100.0).round() as i64;
    if percent == 0 {
        return None;
    }
    let (arrow, color) = if percent > 0 {
        ("↑", theme::GREEN)
    } else {
        ("↓", theme::HOT)
    };
    Some(Span::styled(
        format!(" {arrow}{}%", percent.abs()),
        Style::default().fg(color),
    ))
}

fn push_hero(
    spans: &mut Vec<Span<'static>>,
    value: String,
    label: &'static str,
    color: Color,
    separator: &'static str,
) {
    if !spans.is_empty() {
        spans.push(Span::styled(separator, Style::default().fg(theme::FAINT)));
    }
    spans.push(Span::styled(
        value,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!(" {label}"),
        Style::default().fg(theme::MUTED),
    ));
}
