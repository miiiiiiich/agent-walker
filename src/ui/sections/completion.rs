use crate::format::{format_count, format_duration_ms};
use crate::model::{DurationSummary, Summary};
use crate::ui::{theme, utils};
use ratatui::prelude::*;

/// The COMPLETION section: duration stats when a turn completed, plus the
/// window's interruption count in the title. A window with interruptions
/// but no completed turn renders the title alone — the count stays visible
/// without zero percentiles or empty bars.
pub(in crate::ui) fn duration_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let duration = summary.completion_duration.as_ref();
    if duration.is_none() && summary.interrupted == 0 {
        return Vec::new();
    }
    let title = utils::section_title(
        "COMPLETION",
        &completion_annotation(duration, summary.interrupted, width),
    );
    let Some(duration) = duration else {
        return vec![title];
    };
    let max = duration
        .buckets
        .iter()
        .map(|bucket| bucket.count)
        .max()
        .unwrap_or(0);
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
fn completion_annotation(
    duration: Option<&DurationSummary>,
    interrupted: usize,
    width: u16,
) -> String {
    let budget = usize::from(width).saturating_sub("▍ COMPLETION  ".chars().count());
    let fitted = |candidates: [String; 2], fallback: String| {
        candidates
            .into_iter()
            .find(|text| text.chars().count() <= budget)
            .unwrap_or(fallback)
    };
    let interrupted_label = format_count(interrupted);
    let Some(duration) = duration else {
        return fitted(
            [
                format!("{interrupted_label} interrupted"),
                format!("{interrupted_label} esc"),
            ],
            String::new(),
        );
    };
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
    if interrupted == 0 {
        return base;
    }
    fitted(
        [
            format!("{base} · {interrupted_label} interrupted"),
            format!("{base} · {interrupted_label} esc"),
        ],
        base,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_with_interrupts(interrupted: usize) -> Summary {
        let mut summary = crate::share::fixtures::sample_summary();
        summary.interrupted = interrupted;
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
        summary.completion_duration = None;

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

        // Nothing at all to say → no section.
        let mut silent = summary_with_interrupts(0);
        silent.completion_duration = None;
        assert!(duration_lines(&silent, 100).is_empty());
    }
}
