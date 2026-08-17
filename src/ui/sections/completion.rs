use crate::format::{format_count, format_duration_ms};
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

pub(in crate::ui) fn duration_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let Some(duration) = &summary.completion_duration else {
        return Vec::new();
    };
    let max = duration
        .buckets
        .iter()
        .map(|bucket| bucket.count)
        .max()
        .unwrap_or(0);
    let mut lines = vec![
        utils::section_title("COMPLETION", &completion_annotation(duration, width)),
        Line::from(vec![
            Span::styled("p50 ", Style::default().fg(theme::MUTED)),
            Span::styled(
                format_duration_ms(duration.p50_ms),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("   p90 ", Style::default().fg(theme::MUTED)),
            Span::styled(
                format_duration_ms(duration.p90_ms),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled("   max ", Style::default().fg(theme::MUTED)),
            Span::styled(
                format_duration_ms(duration.max_ms),
                Style::default().fg(theme::TEXT),
            ),
        ]),
    ];
    let bar_width = utils::bar_width_for(width);
    for bucket in &duration.buckets {
        lines.push(utils::count_bar_line(
            &bucket.label,
            bucket.count,
            max,
            bar_width,
            theme::BLUE,
        ));
    }
    lines
}

/// The section annotation, width-fitted: the interruption count appends in
/// its long form when the rail has room, falls back to a compact "esc"
/// label, and drops entirely on rails too narrow for either — never
/// clipped mid-word. The prefix budget covers "▍ COMPLETION  ".
fn completion_annotation(duration: &crate::model::DurationSummary, width: u16) -> String {
    let autonomous: usize = duration
        .buckets
        .iter()
        .skip(3)
        .map(|bucket| bucket.count)
        .sum();
    let base = format!(
        "{} turns · {} ran ≥20m",
        format_count(duration.count),
        format_count(autonomous)
    );
    if duration.interrupted == 0 {
        return base;
    }
    let budget = usize::from(width).saturating_sub("▍ COMPLETION  ".chars().count());
    let long = format!(
        "{base} · {} interrupted",
        format_count(duration.interrupted)
    );
    if long.chars().count() <= budget {
        return long;
    }
    let compact = format!("{base} · {} esc", format_count(duration.interrupted));
    if compact.chars().count() <= budget {
        return compact;
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_with_interrupts(interrupted: usize) -> Summary {
        let mut summary = crate::share::fixtures::sample_summary();
        if let Some(duration) = summary.completion_duration.as_mut() {
            duration.interrupted = interrupted;
        }
        summary
    }

    fn rendered(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The 80-column layout's rail must not clip the title mid-word: the
    /// interruption count falls back to "esc" and then drops entirely.
    #[test]
    fn completion_title_fits_narrow_rails() {
        let summary = summary_with_interrupts(12);

        let wide = duration_lines(&summary, 100);
        let title = rendered(&wide[0]);
        assert!(title.contains("12 interrupted"), "{title:?}");

        let narrow = duration_lines(&summary, 44);
        let title = rendered(&narrow[0]);
        assert!(title.chars().count() <= 44, "{title:?}");
        assert!(!title.contains("interrupted"), "{title:?}");

        let zero = duration_lines(&summary_with_interrupts(0), 100);
        let title = rendered(&zero[0]);
        assert!(!title.contains("esc") && !title.contains("interrupted"));
    }
}
