use crate::format::format_percent;
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

/// MODES: how the model is allowed to think and act, per provider dial —
/// Claude shows the thinking fire rate (plus fast mode once it has data),
/// and both Claude and Codex show their reasoning-effort mix and their
/// granted-autonomy mix (the `autonomy` row: `permissionMode` /
/// `approval_policy`). Deliberately
/// small (a few rows): the dials are asymmetric across providers, so each
/// tab renders only its own rows.
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
        lines.push(mix_line("effort    ", &modes.efforts, 3, width));
    }

    if !modes.permissions.is_empty() {
        let display: Vec<(String, usize)> = modes
            .permissions
            .iter()
            .map(|(label, count)| {
                let display = match label.as_str() {
                    "acceptEdits" => "edits",
                    "bypassPermissions" => "bypass",
                    other => other,
                };
                (display.to_owned(), *count)
            })
            .collect();
        lines.push(mix_line("autonomy  ", &display, 2, width));
    }

    lines
}

/// One label-plus-entries row (`effort    xhigh 98.2% · max 0.3%`): top
/// `max_entries` labels with their share, fitted to the rail — an entry that
/// would overflow `width` is dropped whole, never clipped mid-label.
fn mix_line(
    label: &'static str,
    entries: &[(String, usize)],
    max_entries: usize,
    width: u16,
) -> Line<'static> {
    let total: usize = entries.iter().map(|(_, count)| count).sum();
    let total = u64::try_from(total).unwrap_or(1).max(1);
    let mut spans = vec![Span::styled(label, Style::default().fg(theme::TEXT))];
    let mut used = label.len();
    for (index, (name, count)) in entries.iter().take(max_entries).enumerate() {
        let separator = if index == 0 { "" } else { " · " };
        let entry = format!(
            "{name} {}",
            format_percent(u64::try_from(*count).unwrap_or(0), total)
        );
        if index > 0 && used + separator.len() + entry.len() > usize::from(width) {
            break;
        }
        used += separator.len() + entry.len();
        if !separator.is_empty() {
            spans.push(Span::styled(
                separator.to_owned(),
                Style::default().fg(theme::DIM),
            ));
        }
        let style = if index == 0 {
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::MUTED)
        };
        spans.push(Span::styled(entry, style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModesSummary, Summary};

    fn summary_with_permissions(permissions: Vec<(String, usize)>) -> Summary {
        let mut summary = crate::share::fixtures::sample_summary();
        summary.modes = ModesSummary {
            assistant_turns: 0,
            thinking_turns: 0,
            fast_turns: 0,
            efforts: Vec::new(),
            permissions,
        };
        summary
    }

    fn rendered(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The 80-column layout gives the right rail ~34 columns; long Codex
    /// labels must drop the second entry instead of clipping mid-label.
    #[test]
    fn permission_row_fits_a_narrow_rail_by_dropping_entries() {
        let summary = summary_with_permissions(vec![
            ("on-request".to_owned(), 50),
            ("never".to_owned(), 50),
        ]);
        let narrow = modes_lines(&summary, 34);
        let row = rendered(narrow.last().expect("permission row should render"));
        assert!(row.len() <= 34, "row overflows the rail: {row:?}");
        assert!(row.contains("on-request"));
        assert!(!row.contains("never"));

        let wide = modes_lines(&summary, 80);
        let row = rendered(wide.last().expect("permission row should render"));
        assert!(row.contains("on-request") && row.contains("never"));
    }
}
