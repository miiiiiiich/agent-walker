use crate::cost::usage_cost_usd;
use crate::format::{format_usd, short_model_name};
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;
use time::Duration;

/// API-equivalent spend, cache-aware, that answers "is the subscription paying
/// for itself". Shows trailing windows (today / 7d / 30d) cut from the per-day,
/// per-model aggregates.
pub(in crate::ui) fn cost_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let label_width = utils::kv_label_width(width);
    // Cursor contributes its own reported cost (an actual charge, not a LiteLLM
    // estimate), so qualify the annotation when any of it is in the total.
    let has_reported = summary
        .models
        .iter()
        .any(|model| model.reported_cost_usd.is_some());
    let annotation = if width < 44 {
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
    };
    let mut total = 0.0_f64;
    let mut per_model: Vec<(String, f64)> = Vec::new();
    for model in &summary.models {
        // Provider-reported cost (Cursor) plus the LiteLLM price of the tokens
        // that had no reported cost — additive so a model name shared by a
        // reporting and a non-reporting provider prices both halves.
        let cost = model.reported_cost_usd.unwrap_or(0.0)
            + usage_cost_usd(&model.name, &model.unreported_usage).unwrap_or(0.0);
        if cost > 0.0 {
            total += cost;
            per_model.push((short_model_name(&model.name), cost));
        }
    }
    if total < 0.01 {
        return Vec::new();
    }
    per_model.sort_by(|left, right| right.1.total_cmp(&left.1));

    let mut lines = vec![utils::section_title("COST", &annotation)];
    for (label, window_days) in [("Today", 1_u16), ("7 days", 7), ("30 days", 30)] {
        if window_days >= summary.period_days {
            break;
        }
        lines.push(cost_row(
            label,
            window_cost_usd(summary, window_days),
            false,
            label_width,
        ));
    }
    lines.push(cost_row(
        &format!("{} days", summary.period_days),
        total,
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

fn cost_row(label: &str, cost: f64, emphasize: bool, label_width: usize) -> Line<'static> {
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
        Span::styled(format_usd(cost), value_style),
    ])
}

/// Cost over the trailing `days` ending at the period end, summed from the
/// per-day per-model usage (cache-aware per entry).
fn window_cost_usd(summary: &Summary, days: u16) -> f64 {
    let start = summary.period_end - Duration::days(i64::from(days) - 1);
    summary
        .model_daily
        .iter()
        .filter(|entry| entry.date >= start)
        .map(|entry| {
            entry.reported_cost_usd.unwrap_or(0.0)
                + usage_cost_usd(&entry.model, &entry.unreported_usage).unwrap_or(0.0)
        })
        .sum()
}
