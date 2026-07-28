use crate::format::{format_percent, format_tokens, short_model_name};
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

pub(in crate::ui) fn model_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let top_models = summary.models.iter().take(6).collect::<Vec<_>>();
    if top_models.is_empty() {
        return vec![
            utils::section_title("MODELS", ""),
            Line::from(Span::styled(
                "No model usage found",
                Style::default().fg(theme::MUTED),
            )),
        ];
    }

    if !utils::token_usage_available(summary) {
        let mut lines = vec![utils::section_title(
            "MODELS",
            "observed in logs — no token volume",
        )];
        for model in top_models {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "{:<22}",
                        utils::compact_label(&short_model_name(&model.name), 21)
                    ),
                    Style::default().fg(theme::TEXT),
                ),
                Span::styled(
                    format!("{:>6} events", model.events),
                    Style::default().fg(theme::MUTED),
                ),
            ]));
        }
        return lines;
    }

    // Claude-usage-style horizontal bars, one color per model.
    let total_volume = summary.total_usage.token_volume();
    let max_volume = top_models
        .first()
        .map_or(0, |model| model.usage.token_volume());
    let bar_width = usize::from(width).saturating_sub(31).clamp(8, 24);
    let mut lines = vec![utils::section_title("MODELS", "share of period")];
    for (index, model) in top_models.into_iter().enumerate() {
        let volume = model.usage.token_volume();
        let share = if total_volume == 0 {
            String::new()
        } else {
            format_percent(volume, total_volume)
        };
        let filled = utils::bar_fill(volume, max_volume, bar_width);
        lines.push(utils::stat_bar_line(
            &short_model_name(&model.name),
            theme::model_color(index),
            filled,
            bar_width,
            &format_tokens(volume),
            &share,
        ));
    }
    lines
}
