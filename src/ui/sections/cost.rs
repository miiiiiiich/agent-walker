use crate::cost::CostTally;
use crate::format::{format_tokens, format_usd, short_model_name};
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;
use time::Duration;

/// API-equivalent spend, cache-aware, that answers "is the subscription paying
/// for itself". Shows trailing windows (today / 7d / 30d) cut from the per-day,
/// per-model aggregates. Tokens with no known price (LiteLLM unreachable and
/// uncached, or a model id the table lacks) are never summed as $0: a fully
/// unpriced window renders "—", a partially priced one flags the gap.
pub(in crate::ui) fn cost_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let label_width = utils::kv_label_width(width);
    let mut total = CostTally::default();
    let mut per_model: Vec<(String, f64)> = Vec::new();
    for model in &summary.models {
        // Provider-reported cost (Cursor) plus the LiteLLM price of the tokens
        // that had no reported cost — additive so a model name shared by a
        // reporting and a non-reporting provider prices both halves.
        let mut tally = CostTally::default();
        tally.add(
            &model.name,
            &model.unreported_usage,
            model.reported_cost_usd,
        );
        total.priced_usd += tally.priced_usd;
        total.unpriced_volume = total.unpriced_volume.saturating_add(tally.unpriced_volume);
        if tally.priced_usd > 0.0 {
            per_model.push((short_model_name(&model.name), tally.priced_usd));
        }
    }
    if total.priced_usd < 0.01 && total.is_complete() {
        return Vec::new();
    }
    per_model.sort_by(|left, right| right.1.total_cmp(&left.1));

    let mut lines = vec![utils::section_title(
        "COST",
        &annotation(summary, &total, width),
    )];
    if total.priced_usd < 0.01 {
        // Nothing could be priced: one honest "—" row instead of a vanished
        // section (which reads as "no cost").
        lines.push(cost_row(
            &format!("{} days", summary.period_days),
            None,
            true,
            label_width,
        ));
        return lines;
    }
    for (label, window_days) in [("Today", 1_u16), ("7 days", 7), ("30 days", 30)] {
        if window_days >= summary.period_days {
            break;
        }
        lines.push(cost_row(
            label,
            window_cost(summary, window_days).complete_usd(),
            false,
            label_width,
        ));
    }
    lines.push(cost_row(
        &format!("{} days", summary.period_days),
        total.complete_usd(),
        true,
        label_width,
    ));
    for (name, cost) in per_model.iter().take(3) {
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{:<label_width$}",
                    utils::compact_label(name, label_width.saturating_sub(1))
                ),
                Style::default().fg(theme::MUTED),
            ),
            Span::styled(format_usd(*cost), Style::default().fg(theme::TEXT)),
        ]));
    }
    lines
}

/// The title annotation: rate provenance, plus the unpriced gap when any
/// token in the window had no price.
fn annotation(summary: &Summary, total: &CostTally, width: u16) -> String {
    // Cursor contributes its own reported cost (an actual charge, not a LiteLLM
    // estimate), so qualify the annotation when any of it is in the total.
    let has_reported = summary
        .models
        .iter()
        .any(|model| model.reported_cost_usd.is_some());
    if !total.is_complete() {
        // Width-fitted like the COMPLETION title: longest form that fits the
        // rail after "▍ COST  ", falling back to the bare gap.
        let budget = usize::from(width).saturating_sub("▍ COST  ".chars().count());
        let unpriced = format_tokens(total.unpriced_volume);
        return [
            format!("{unpriced} tokens unpriced · rates unavailable"),
            format!("{unpriced} tokens unpriced"),
        ]
        .into_iter()
        .find(|text| text.chars().count() <= budget)
        .unwrap_or_else(|| format!("{unpriced} unpriced"));
    }
    if width < 44 {
        if has_reported {
            "incl. reported".to_owned()
        } else {
            "api-equivalent".to_owned()
        }
    } else {
        let base = crate::cost::pricing_as_of().map_or_else(
            || "api-equivalent · cache-aware".to_owned(),
            |date| format!("api-equivalent · rates {date}"),
        );
        if has_reported {
            format!("{base} · incl. provider-reported")
        } else {
            base
        }
    }
}

/// A label/value row; `None` renders "—" for a value that could not be
/// fully priced.
fn cost_row(label: &str, cost: Option<f64>, emphasize: bool, label_width: usize) -> Line<'static> {
    let value_style = if emphasize {
        Style::default()
            .fg(theme::GOLD)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::TEXT)
    };
    Line::from(vec![
        Span::styled(
            format!("{label:<label_width$}"),
            Style::default().fg(theme::MUTED),
        ),
        Span::styled(cost.map_or_else(|| "—".to_owned(), format_usd), value_style),
    ])
}

/// Cost over the trailing `days` ending at the period end, summed from the
/// per-day per-model usage (cache-aware per entry), unpriced volume kept
/// separate.
fn window_cost(summary: &Summary, days: u16) -> CostTally {
    let start = summary.period_end - Duration::days(i64::from(days) - 1);
    let mut tally = CostTally::default();
    for entry in summary
        .model_daily
        .iter()
        .filter(|entry| entry.date >= start)
    {
        tally.add(
            &entry.model,
            &entry.unreported_usage,
            entry.reported_cost_usd,
        );
    }
    tally
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelDailyStat, TokenUsage};

    fn rendered(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// An unpriced model never sums as $0: the total row shows "—" and the
    /// title annotation names the gap, fitted to the rail at every width.
    #[test]
    fn unpriced_usage_shows_dash_and_fits_narrow_rails() {
        let mut summary = crate::share::fixtures::sample_summary();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        summary.models[0].name = "model-nobody-priced".to_owned();
        summary.models[0].unreported_usage = usage.clone();
        summary.model_daily.push(ModelDailyStat {
            date: summary.period_end,
            model: "model-nobody-priced".to_owned(),
            usage: usage.clone(),
            unreported_usage: usage,
            reported_cost_usd: None,
        });

        let wide = cost_lines(&summary, 100);
        assert!(!wide.is_empty(), "an unpriced window still renders COST");
        let title = rendered(&wide[0]);
        assert!(title.contains("unpriced"), "{title:?}");
        assert!(!wide.iter().any(|line| rendered(line).contains("$0")));
        assert!(wide.iter().any(|line| rendered(line).contains('—')));

        for width in [44_u16, 60, 80] {
            let lines = cost_lines(&summary, width);
            let title = rendered(&lines[0]);
            assert!(
                title.chars().count() <= usize::from(width),
                "width {width}: {title:?}"
            );
            assert!(title.contains("unpriced"), "{title:?}");
        }
    }
}
