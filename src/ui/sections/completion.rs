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
    let title = utils::section_title("COMPLETION", &completion_annotation(duration, width));
    // An interrupt-only window (no completed turn) keeps the count visible
    // but has no percentiles or buckets worth drawing.
    if duration.count == 0 {
        return vec![title];
    }
    let mut lines = vec![
        title,
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
    let budget = usize::from(width).saturating_sub("▍ COMPLETION  ".chars().count());
    let fitted = |candidates: [String; 2], fallback: String| {
        candidates
            .into_iter()
            .find(|text| text.chars().count() <= budget)
            .unwrap_or(fallback)
    };
    let interrupted = format_count(duration.interrupted);
    if duration.count == 0 {
        return fitted(
            [
                format!("{interrupted} interrupted"),
                format!("{interrupted} esc"),
            ],
            String::new(),
        );
    }
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
    fitted(
        [
            format!("{base} · {interrupted} interrupted"),
            format!("{base} · {interrupted} esc"),
        ],
        base,
    )
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

    /// An interrupt-only window (every turn aborted, none completed) keeps
    /// the count visible as a bare title — no zero percentiles or empty bars.
    #[test]
    fn interrupt_only_window_renders_title_only() {
        let mut summary = summary_with_interrupts(3);
        if let Some(duration) = summary.completion_duration.as_mut() {
            duration.count = 0;
            duration.buckets.iter_mut().for_each(|b| b.count = 0);
        }

        let lines = duration_lines(&summary, 100);

        assert_eq!(lines.len(), 1);
        let title = rendered(&lines[0]);
        assert!(title.contains("3 interrupted"), "{title:?}");
        assert!(!title.contains("turns"), "{title:?}");

        // The width ladder applies here too — no mid-word clipping on
        // rails narrower than the long form.
        let narrow = duration_lines(&summary, 20);
        let title = rendered(&narrow[0]);
        assert!(title.chars().count() <= 20, "{title:?}");
        assert!(!title.contains("interrupted"), "{title:?}");
    }
}
