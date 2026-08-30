//! Plain-text snapshot rendering for --snapshot: the report as stable,
//! greppable text.
use crate::model::{AppSummary, Summary};

use super::{
    format_date, format_duration_ms, format_percent, format_timestamp, format_tokens,
    short_model_name,
};

pub fn snapshot_app(report: &AppSummary) -> String {
    let mut lines = vec![
        "Agent Walker snapshot".to_owned(),
        format!(
            "generated: {} period: {}d",
            format_timestamp(report.generated_at),
            report.period_days
        ),
        String::new(),
        "combined:".to_owned(),
    ];
    lines.extend(indent_lines(&snapshot(&report.combined), "  "));
    lines.push(String::new());
    lines.push("providers:".to_owned());
    for provider in &report.providers {
        lines.push(format!(
            "- {} volume:{} sessions:{} tools:{} durations:{} interrupted:{} files:{} lines:{} parse_errors:{}",
            provider.provider.label(),
            format_tokens(provider.total_usage.token_volume()),
            provider.sessions,
            provider.tools.iter().map(|tool| tool.calls).sum::<usize>(),
            provider
                .completion_duration
                .as_ref()
                .map_or(0, |duration| duration.count),
            provider.interrupted,
            provider.scan_stats.files_seen,
            provider.scan_stats.lines_seen,
            provider.scan_stats.parse_errors,
        ));
        if let Some(model) = &provider.favorite_model {
            lines.push(format!("  favorite_model: {}", short_model_name(model)));
        }
        if let Some(duration) = &provider.completion_duration {
            lines.push(format!(
                "  completion: p50:{} p90:{} p95:{} max:{}",
                format_duration_ms(duration.p50_ms),
                format_duration_ms(duration.p90_ms),
                format_duration_ms(duration.p95_ms),
                format_duration_ms(duration.max_ms)
            ));
        }
        // Per-provider cache reuse: the retention rule differs by provider,
        // so the combined record alone would hide which side paid.
        lines.extend(context_line(provider).map(|line| format!("  {line}")));
    }
    lines.join("\n")
}

pub fn snapshot(summary: &Summary) -> String {
    let subagent_volume = summary
        .agents
        .iter()
        .map(|agent| agent.usage.token_volume())
        .fold(0u64, u64::saturating_add);
    let subagent_share = format_percent(subagent_volume, summary.total_usage.token_volume());

    let mut lines = vec![
        "Agent Walker snapshot".to_owned(),
        format!(
            "provider: {} period: {}..{} ({}d)",
            summary.provider.label(),
            format_date(summary.period_start),
            format_date(summary.period_end),
            summary.period_days
        ),
        format!("root: {}", summary.root.display()),
        format!(
            "files: {} lines: {} parse_errors: {}",
            summary.scan_stats.files_seen,
            summary.scan_stats.lines_seen,
            summary.scan_stats.parse_errors
        ),
        format!(
            "token_volume: {} subagent_share: {}",
            format_tokens(summary.total_usage.token_volume()),
            subagent_share
        ),
        format!(
            "sessions: {} active_days: {} longest_streak: {} current_streak: {}",
            summary.sessions,
            summary.active_days,
            summary.longest_streak_days,
            summary.current_streak_days
        ),
        {
            let codename = crate::codename::for_summary(summary);
            format!(
                "codename: {} rank: {}",
                codename.title(),
                codename.rank.letters().unwrap_or("unranked")
            )
        },
    ];

    if let Some(model) = &summary.favorite_model {
        lines.push(format!("favorite_model: {}", short_model_name(model)));
    }
    if let Some(day) = &summary.most_active_day {
        lines.push(format!(
            "most_active_day: {} {}",
            format_date(day.date),
            format_tokens(day.usage.token_volume())
        ));
    }
    if let Some((hour, usage)) = summary.busiest_hour {
        lines.push(format!(
            "busiest_hour: {hour:02}:00 {}",
            format_tokens(usage)
        ));
    }
    lines.extend(completion_lines(summary));
    lines.extend(context_line(summary));

    lines.push("models:".to_owned());
    for model in summary.models.iter().take(5) {
        lines.push(format!(
            "- {} {} in:{} out:{} cache:{}",
            short_model_name(&model.name),
            format_tokens(model.usage.token_volume()),
            format_tokens(model.usage.input_tokens),
            format_tokens(model.usage.output_tokens),
            format_tokens(model.usage.cache_read_input_tokens)
        ));
    }

    lines.push("agents:".to_owned());
    for agent in summary.agents.iter().take(5) {
        lines.push(format!(
            "- {} {} calls:{}",
            agent.name,
            format_tokens(agent.usage.token_volume()),
            agent.calls
        ));
    }

    lines.push("tools:".to_owned());
    for tool in summary.tools.iter().take(8) {
        lines.push(format!("- {} {}", tool.name, tool.calls));
    }

    lines.join("\n")
}

/// The completion records: duration stats when a turn completed, and the
/// interruption count on its own line (independent metrics — a window can
/// hold one without the other). A summary with neither emits nothing, so
/// the stable snapshot shape for silent windows is unchanged.
fn completion_lines(summary: &Summary) -> Vec<String> {
    let mut lines = Vec::new();
    if summary.completion_duration.is_none() && summary.interrupted == 0 {
        return lines;
    }
    if let Some(duration) = &summary.completion_duration {
        lines.push(format!(
            "completion_duration: count:{} p50:{} p90:{} p95:{} max:{}",
            duration.count,
            format_duration_ms(duration.p50_ms),
            format_duration_ms(duration.p90_ms),
            format_duration_ms(duration.p95_ms),
            format_duration_ms(duration.max_ms)
        ));
    }
    lines.push(format!("completion_interrupted: {}", summary.interrupted));
    lines
}

/// The cache-reuse record over the fixed 30-day window (hence the `_30d`
/// key — the rest of the snapshot follows `--days`): cached share,
/// input-equivalent volume, and the two behaviours that pay full price for a
/// prefix. `-` when the provider has no session notion.
fn context_line(summary: &Summary) -> Option<String> {
    let context = summary.context.as_ref()?;
    let reason = |reason: &Option<crate::model::ContextReason>| {
        reason
            .as_ref()
            .map_or_else(|| "-".to_owned(), |r| format_tokens(r.effective))
    };
    Some(format!(
        "context_30d: cached:{:.1}% effective:{} calls:{} expired:{} cold_start:{}",
        context.cached_share() * 100.0,
        format_tokens(context.effective_tokens),
        context.calls,
        reason(&context.expired),
        reason(&context.cold_start),
    ))
}

fn indent_lines(value: &str, indent: &str) -> Vec<String> {
    value
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect()
}
