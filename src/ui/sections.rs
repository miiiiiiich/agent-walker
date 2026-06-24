use ratatui::prelude::*;
use time::Duration;

use crate::cost::usage_cost_usd;
use crate::format::{
    format_count, format_date, format_duration_ms, format_duration_secs, format_percent,
    format_tokens, format_usd, short_model_name,
};
use crate::model::Summary;

use super::theme;
use super::utils;

/// API-equivalent spend, cache-aware, that answers "is the subscription paying
/// for itself". Shows trailing windows (today / 7d / 30d) cut from the per-day,
/// per-model aggregates.
pub(super) fn cost_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let label_width = utils::kv_label_width(width);
    let annotation = if width < 44 {
        "api-equivalent".to_owned()
    } else {
        crate::cost::pricing_as_of().map_or_else(
            || "api-equivalent · cache-aware".to_owned(),
            |date| format!("api-equivalent · rates {date}"),
        )
    };
    let mut total = 0.0_f64;
    let mut per_model: Vec<(String, f64)> = Vec::new();
    for model in &summary.models {
        // Prefer the provider's own reported cost (Cursor: its models aren't in
        // LiteLLM); fall back to LiteLLM pricing for everyone else.
        if let Some(cost) = model
            .reported_cost_usd
            .or_else(|| usage_cost_usd(&model.name, &model.usage))
        {
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
        .filter_map(|entry| {
            entry
                .reported_cost_usd
                .or_else(|| usage_cost_usd(&entry.model, &entry.usage))
        })
        .sum()
}

/// Top repositories by token volume — where the AI time actually goes.
pub(super) fn project_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    if summary.projects.is_empty() {
        return Vec::new();
    }
    let max = summary
        .projects
        .iter()
        .map(|project| project.usage.token_volume())
        .max()
        .unwrap_or(0);
    if max == 0 {
        return Vec::new();
    }

    // Repository names need more label room than tool names; trade bar length.
    let label_width = 20_usize;
    let bar_width = usize::from(width)
        .saturating_sub(label_width + 9)
        .clamp(8, 24);
    let mut lines = vec![utils::section_title("PROJECTS", "by token volume")];
    for project in summary.projects.iter().take(6) {
        let label = utils::compact_label_tail(&project.name, label_width - 1);
        let value = project.usage.token_volume();
        let filled = utils::bar_fill(value, max, bar_width);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label:<label_width$}"),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled("▄".repeat(filled), Style::default().fg(theme::ACCENT)),
            Span::styled(
                "▄".repeat(bar_width - filled),
                Style::default().fg(theme::FAINT),
            ),
            Span::styled(
                format!(" {:>7}", format_tokens(value)),
                Style::default().fg(theme::MUTED),
            ),
        ]));
    }
    let hidden = summary.projects.len().saturating_sub(6);
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("+{hidden} more"),
            Style::default().fg(theme::DIM),
        )));
    }
    lines
}

pub(super) fn model_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
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
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{:<14}",
                    utils::compact_label(&short_model_name(&model.name), 13)
                ),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled(
                "▄".repeat(filled),
                Style::default().fg(theme::model_color(index)),
            ),
            Span::styled(
                "▄".repeat(bar_width - filled),
                Style::default().fg(theme::FAINT),
            ),
            Span::styled(
                format!(" {:>8}", format_tokens(volume)),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{share:>7}"), Style::default().fg(theme::MUTED)),
        ]));
    }
    lines
}

pub(super) fn tool_lines(summary: &Summary, width: u16, limit: usize) -> Vec<Line<'static>> {
    if summary.tools.is_empty() {
        return vec![
            utils::section_title("TOOLS", ""),
            Line::from(Span::styled(
                "No tool calls found",
                Style::default().fg(theme::MUTED),
            )),
        ];
    }
    let total_calls: usize = summary.tools.iter().map(|tool| tool.calls).sum();
    let max = summary
        .tools
        .iter()
        .map(|tool| tool.calls)
        .max()
        .unwrap_or(0);
    let mut lines = vec![utils::section_title(
        "TOOLS",
        &format!("{} calls", format_count(total_calls)),
    )];
    let bar_width = utils::bar_width_for(width);
    lines.extend(
        summary.tools.iter().take(limit).map(|tool| {
            utils::count_bar_line(&tool.name, tool.calls, max, bar_width, theme::GREEN)
        }),
    );
    let hidden = summary.tools.len().saturating_sub(limit);
    if hidden > 0 {
        let hidden_calls: usize = summary
            .tools
            .iter()
            .skip(limit)
            .map(|tool| tool.calls)
            .sum();
        lines.push(Line::from(Span::styled(
            format!("+{hidden} more · {} calls", format_count(hidden_calls)),
            Style::default().fg(theme::DIM),
        )));
    }
    lines
}

pub(super) fn signal_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let label_width = utils::kv_label_width(width);
    let most_active = summary.most_active_day.as_ref().map_or_else(
        || "—".to_owned(),
        |day| {
            format!(
                "{} · {}",
                format_date(day.date),
                format_tokens(day.usage.token_volume())
            )
        },
    );
    let busiest_hour = summary.busiest_hour.map_or_else(
        || "—".to_owned(),
        |(hour, usage)| format!("{hour:02}:00 · {}", format_tokens(usage)),
    );
    let longest_session = summary.longest_session.as_ref().map_or_else(
        || "—".to_owned(),
        |session| format_duration_secs(session.duration_secs()),
    );
    let streaks = format!(
        "{}d now · {}d best",
        summary.current_streak_days, summary.longest_streak_days
    );

    let mut lines = vec![utils::section_title("SIGNAL", "")];
    lines.push(utils::kv(
        "Favorite",
        &summary
            .favorite_model
            .as_deref()
            .map_or_else(|| "—".to_owned(), short_model_name),
        label_width,
    ));
    lines.push(utils::kv("Top day", &most_active, label_width));
    lines.push(utils::kv("Peak hour", &busiest_hour, label_width));
    lines.push(utils::kv("Longest", &longest_session, label_width));
    lines.push(utils::kv("Streak", &streaks, label_width));
    lines
}

pub(super) fn agent_lines(summary: &Summary, width: u16, limit: usize) -> Vec<Line<'static>> {
    let with_usage = utils::token_usage_available(summary);
    let show_calls = width >= 40;
    let name_width = usize::from(width)
        .saturating_sub(if show_calls { 20 } else { 10 })
        .clamp(10, 18);
    let mut lines = vec![utils::section_title("SUBAGENTS", "by token volume")];
    for agent in summary.agents.iter().take(limit) {
        let mut spans = vec![Span::styled(
            format!(
                "{:<width$}",
                utils::compact_label(&agent.name, name_width.saturating_sub(1)),
                width = name_width
            ),
            Style::default().fg(theme::TEXT),
        )];
        if with_usage {
            spans.push(Span::styled(
                format!("{:>8}", format_tokens(agent.usage.token_volume())),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if show_calls && agent.calls > 0 {
            spans.push(Span::styled(
                format!("  {} calls", agent.calls),
                Style::default().fg(theme::MUTED),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Concurrency distribution: how much active time ran 1 / 2 / 3 / 4–6 / 7+
/// sessions at once. Cool→hot per level (solo = cool, heavy parallel = bright).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Percentages are display-only terminal rendering."
)]
pub(super) fn parallel_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
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
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<14}", labels[index]),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled("▄".repeat(filled), Style::default().fg(colors[index])),
            Span::styled(
                "▄".repeat(bar_width - filled),
                Style::default().fg(theme::FAINT),
            ),
            Span::styled(
                format!(" {value:>6} {pct:>3}%"),
                Style::default().fg(theme::MUTED),
            ),
        ]));
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

pub(super) fn duration_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let Some(duration) = &summary.completion_duration else {
        return Vec::new();
    };
    let max = duration
        .buckets
        .iter()
        .map(|bucket| bucket.count)
        .max()
        .unwrap_or(0);
    // Autonomy signal: how often a turn ran 20+ minutes unattended.
    let autonomous: usize = duration
        .buckets
        .iter()
        .skip(3)
        .map(|bucket| bucket.count)
        .sum();
    let mut lines = vec![
        utils::section_title(
            "COMPLETION",
            &format!(
                "{} turns · {} ran ≥20m",
                format_count(duration.count),
                format_count(autonomous)
            ),
        ),
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
