use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

/// Concurrency distribution: how much active time ran 1 / 2 / 3 / 4–6 / 7+
/// sessions at once. Cool→hot per level (solo = cool, heavy parallel = bright).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Percentages are display-only terminal rendering."
)]
pub(in crate::ui) fn parallel_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let orchestration = &summary.orchestration;
    let total: u64 = orchestration.time_by_level.iter().sum();
    if total == 0 {
        return Vec::new();
    }
    let four_plus = orchestration.time_by_level[3]
        + orchestration.time_by_level[4]
        + orchestration.time_by_level[5];
    let four_plus_pct = (four_plus as f64 / total as f64 * 100.0).round() as u64;
    let max = orchestration
        .time_by_level
        .iter()
        .copied()
        .max()
        .unwrap_or(0);
    // Weighted avg concurrency (computed once in the analyzer) = parallelism stat.
    let avg = orchestration.avg_concurrency;
    let labels = ["1", "2", "3", "4-6", "7-9", "10+"];
    let colors = [
        theme::BLUE,
        theme::TEAL,
        theme::GREEN,
        theme::GOLD,
        theme::ACCENT,
        theme::HOT,
    ];
    // Each bar carries both the concrete time and its share ("  6d 20h  30%"),
    // so reserve extra room after the track.
    let bar_width = usize::from(width).saturating_sub(30).clamp(6, 22);
    // Title + a single avg line (mirrors COMPLETION's stat row for alignment).
    let mut lines = vec![
        utils::section_title(
            "PARALLEL AGENTS",
            &format!(
                "{four_plus_pct}% at 4+ · peak {}",
                orchestration.peak_concurrency
            ),
        ),
        Line::from(vec![
            Span::styled("avg ", Style::default().fg(theme::MUTED)),
            Span::styled(
                format!("{avg:.1}"),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" concurrent", Style::default().fg(theme::MUTED)),
        ]),
    ];
    for (index, secs) in orchestration.time_by_level.iter().enumerate() {
        let filled = utils::bar_fill(*secs, max, bar_width);
        let pct = (*secs as f64 / total as f64 * 100.0).round() as u64;
        let value = if *secs == 0 {
            String::new()
        } else {
            compact_duration(*secs)
        };
        let mut spans = vec![Span::styled(
            format!("{:<14}", labels[index]),
            Style::default().fg(theme::TEXT),
        )];
        spans.extend(utils::bar_track(filled, bar_width, colors[index]));
        spans.push(Span::styled(
            format!(" {value:>6} {pct:>3}%"),
            Style::default().fg(theme::MUTED),
        ));
        lines.push(Line::from(spans));
    }
    lines
}

/// Compact duration for inline bar labels: at most the two largest units.
fn compact_duration(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = secs % 86_400 / 3_600;
    let minutes = secs % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}
