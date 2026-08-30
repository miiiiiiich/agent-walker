use crate::format::format_tokens;
use crate::model::{ContextReason, ContextSummary, Summary};
use crate::ui::{theme, utils};
use ratatui::prelude::*;

/// The CONTEXT section: how much of the window's input the prompt cache
/// served (event totals, every usage row), and where the input-equivalent
/// volume went — re-reading long contexts (by size band), resuming sessions
/// after the cache expired, starting sessions, ordinary new input, and an
/// `other` row for volume with no call to attribute it to (subagent rows,
/// aggregate-logging providers). Call-level rows carry a per-call average
/// so "keep going" and "start fresh" sit on one scale; `other` shows `—`.
pub(in crate::ui) fn context_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let Some(context) = &summary.context else {
        return Vec::new();
    };
    // Raw cache data is worth a title even when nothing priced (effective
    // == 0); aggregate-only providers (calls == 0) still get their headline
    // and the `other` row.
    if context.context_tokens == 0 {
        return Vec::new();
    }
    let total = context.effective_tokens;
    // Rows carry a value AND a share column; the shared `bar_width_for`
    // budgets only a count column, so size the bar for label + value + share.
    let bar_width = usize::from(width).saturating_sub(14 + 9 + 7).max(4);
    if total == 0 {
        return vec![utils::section_title(
            "CONTEXT",
            &context_annotation(context, width),
        )];
    }
    let mut lines = vec![
        utils::section_title("CONTEXT", &context_annotation(context, width)),
        // Column legend: the value is input-equivalent tokens per call (so
        // "keep going" and "start fresh" compare directly), the share is the
        // row's slice of the effective total. Without it the values read as
        // raw totals.
        Line::from(Span::styled(
            format!(
                "{:>width$}",
                "per call  share",
                width = 14 + bar_width + 9 + 7
            ),
            Style::default().fg(theme::MUTED),
        )),
    ];
    let max = context
        .bands
        .iter()
        .map(|band| band.cached_effective)
        .chain(context.expired.iter().map(|r| r.effective))
        .chain(context.cold_start.iter().map(|r| r.effective))
        .chain(std::iter::once(context.uncached.effective))
        .chain(std::iter::once(context.unclassified_effective))
        .max()
        .unwrap_or(0);
    for band in context.bands.iter().filter(|band| band.calls > 0) {
        lines.push(utils::stat_bar_line(
            &band.label,
            theme::BLUE,
            utils::bar_fill(band.cached_effective, max, bar_width),
            bar_width,
            &per_call(band.cached_effective, band.calls),
            &share_label(band.cached_effective, total),
        ));
    }
    let uncached = (context.uncached.effective > 0).then_some(context.uncached.clone());
    // Volume from non-call events (sidechains, aggregate providers): part of
    // the share, no call to divide by.
    let unclassified = (context.unclassified_effective > 0).then_some(ContextReason {
        calls: 0,
        effective: context.unclassified_effective,
    });
    for (label, reason) in [
        ("expired", &context.expired),
        ("cold start", &context.cold_start),
        ("uncached", &uncached),
        ("other", &unclassified),
    ] {
        let Some(reason) = reason else { continue };
        lines.push(utils::stat_bar_line(
            label,
            theme::GOLD,
            utils::bar_fill(reason.effective, max, bar_width),
            bar_width,
            &per_call_reason(reason),
            &share_label(reason.effective, total),
        ));
    }
    lines
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Display-only share of a u64 total."
)]
fn share_label(part: u64, total: u64) -> String {
    format!("{:.0}%", part as f64 / total.max(1) as f64 * 100.0)
}

/// Input-equivalent tokens per call — the bold value column. Bands and
/// reason rows use the same scale, so "keep going" and "start fresh"
/// compare directly.
fn per_call(effective: u64, calls: usize) -> String {
    let calls = u64::try_from(calls).unwrap_or(u64::MAX).max(1);
    let value = format_tokens(effective / calls);
    // The value column is 8 wide; a poisoned counter (saturated u64 →
    // "18446744073.7B") must not push the share column off the rail.
    if value.chars().count() > 8 {
        ">999B".to_owned()
    } else {
        value
    }
}

fn per_call_reason(reason: &ContextReason) -> String {
    if reason.calls == 0 {
        return "—".to_owned();
    }
    per_call(reason.effective, reason.calls)
}

/// Width-fitted title annotation: the fixed window, the cached share, and
/// the effective volume, dropping from the right until it fits.
fn context_annotation(context: &ContextSummary, width: u16) -> String {
    let budget = usize::from(width).saturating_sub("▍ CONTEXT  ".chars().count());
    let cached = format!("{:.0}% cached", context.cached_share() * 100.0);
    // Fixed 30-day window, labelled like SKILLS / MODES so the page header's
    // `--days` is never mistaken for this panel's period.
    let effective = format_tokens(context.effective_tokens);
    [
        format!("30d · {cached} · {effective} effective input"),
        format!("30d · {cached} · {effective} effective"),
        format!("30d · {cached}"),
        "30d".to_owned(),
    ]
    .into_iter()
    .find(|text| text.chars().count() <= budget)
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Empty bands are skipped, both reason rows render, and the title never
    /// overflows the rail.
    #[test]
    fn renders_populated_bands_and_reasons_within_width() {
        let summary = crate::share::fixtures::sample_summary();
        let lines = context_lines(&summary, 100);
        let text: Vec<String> = lines.iter().map(rendered).collect();
        assert!(text[0].contains("30d · 95% cached"), "{:?}", text[0]);
        assert!(text[0].contains("effective input"), "{:?}", text[0]);
        // title + legend + 3 populated bands (500K+ is empty) + expired + cold start + uncached + other.
        assert_eq!(lines.len(), 2 + 3 + 4);
        assert!(
            text.iter()
                .any(|line| line.starts_with("other") && line.trim_end().ends_with('%'))
        );
        assert!(text.iter().any(|line| line.starts_with("uncached")));
        assert!(
            text[1].trim_start().starts_with("per call"),
            "{:?}",
            text[1]
        );
        assert!(text.iter().any(|line| line.starts_with("200-500K")));
        assert!(!text.iter().any(|line| line.starts_with("500K+")));
        assert!(text.iter().any(|line| line.starts_with("expired")));
        assert!(text.iter().any(|line| line.starts_with("cold start")));

        for width in [44_u16, 60, 80, 100] {
            let lines = context_lines(&summary, width);
            for line in &lines {
                let text = rendered(line);
                assert!(
                    text.chars().count() <= usize::from(width),
                    "{width}: {text:?}"
                );
            }
            assert!(rendered(&lines[0]).contains("30d"));
        }
    }

    /// A saturated counter from a poisoned log still renders inside the rail.
    #[test]
    fn saturated_values_stay_within_width() {
        let mut summary = crate::share::fixtures::sample_summary();
        if let Some(context) = summary.context.as_mut() {
            context.effective_tokens = u64::MAX;
            context.bands[0].cached_effective = u64::MAX;
            context.bands[0].calls = 1;
            if let Some(expired) = context.expired.as_mut() {
                expired.effective = u64::MAX;
                expired.calls = 1;
            }
        }
        for width in [44_u16, 100] {
            for line in context_lines(&summary, width) {
                let text = rendered(&line);
                assert!(
                    text.chars().count() <= usize::from(width),
                    "{width}: {text:?}"
                );
            }
        }
    }

    /// An aggregate-only provider (Copilot / Grok: totals, no calls) still
    /// shows the headline and its `other` row; nothing priced → title only.
    #[test]
    fn aggregate_only_and_unpriced_shapes() {
        let mut summary = crate::share::fixtures::sample_summary();
        if let Some(context) = summary.context.as_mut() {
            context.calls = 0;
            context.bands.iter_mut().for_each(|b| {
                b.calls = 0;
                b.cached_effective = 0;
            });
            context.expired = None;
            context.cold_start = None;
            context.uncached = ContextReason::default();
            context.unclassified_effective = 7_000_000;
            context.effective_tokens = 7_000_000;
        }
        let lines = context_lines(&summary, 100);
        let text: Vec<String> = lines.iter().map(rendered).collect();
        assert_eq!(lines.len(), 2 + 1, "{text:?}");
        assert!(
            text[2].starts_with("other") && text[2].contains('—'),
            "{:?}",
            text[2]
        );

        if let Some(context) = summary.context.as_mut() {
            context.unclassified_effective = 0;
            context.effective_tokens = 0;
        }
        assert_eq!(context_lines(&summary, 100).len(), 1);
    }

    /// No context data → no section; session-less providers keep the bands
    /// but have no reason rows.
    #[test]
    fn absent_and_sessionless_shapes() {
        let mut summary = crate::share::fixtures::sample_summary();
        summary.context = None;
        assert!(context_lines(&summary, 100).is_empty());

        let mut summary = crate::share::fixtures::sample_summary();
        if let Some(context) = summary.context.as_mut() {
            context.expired = None;
            context.cold_start = None;
        }
        let lines = context_lines(&summary, 100);
        assert_eq!(lines.len(), 2 + 3 + 2);
    }
}
