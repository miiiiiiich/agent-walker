use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

use crate::model::{AppSummary, Summary};

#[allow(
    clippy::cast_precision_loss,
    reason = "Human-facing compact units only need approximate decimal formatting."
)]
pub fn format_tokens(tokens: u64) -> String {
    const BILLION: u64 = 1_000_000_000;
    const MILLION: u64 = 1_000_000;
    const THOUSAND: u64 = 1_000;

    if tokens >= BILLION {
        format!("{:.1}B", tokens as f64 / BILLION as f64)
    } else if tokens >= MILLION {
        format!("{:.1}M", tokens as f64 / MILLION as f64)
    } else if tokens >= THOUSAND {
        format!("{:.1}K", tokens as f64 / THOUSAND as f64)
    } else {
        tokens.to_string()
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Percentages are display-only and do not feed back into calculations."
)]
pub fn format_percent(numerator: u64, denominator: u64) -> String {
    if denominator == 0 {
        return "0.0%".to_owned();
    }
    format!("{:.1}%", numerator as f64 / denominator as f64 * 100.0)
}

pub fn format_count(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(character);
    }
    out
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Display-only rounding of non-negative cost estimates."
)]
pub fn format_usd(value: f64) -> String {
    if !value.is_finite() {
        return "$—".to_owned();
    }
    if value >= 1000.0 {
        format!("${}", format_count(value.round() as usize))
    } else if value >= 100.0 {
        format!("${value:.0}")
    } else {
        // A sum that rounds to zero cents is "$0.00", never a "-0.00" artifact
        // from negative zero or a sub-cent residue.
        let value = if (value * 100.0).round() == 0.0 {
            0.0
        } else {
            value
        };
        format!("${value:.2}")
    }
}

pub fn format_duration_secs(seconds: i64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

pub fn format_duration_ms(duration_ms: u64) -> String {
    let seconds = duration_ms / 1_000;
    format_duration_secs(i64::try_from(seconds).unwrap_or(i64::MAX))
}

pub fn format_date(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

pub fn format_timestamp(timestamp: OffsetDateTime) -> String {
    timestamp
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_owned())
}

pub fn short_model_name(name: &str) -> String {
    sanitize_label(&short_model_name_raw(name))
}

/// Clamp a model label to a safe shape. Model names come from untrusted logs, so
/// a crafted one could smuggle a repo name, an absolute path, or a token-like
/// string onto the shareable card / clipboard — artifacts meant to carry none.
///
/// Stripping disallowed characters is not enough: it would still publish the
/// readable fragments of a smuggled path (`gemini/Users/alice/secret` →
/// `geminiUsersalicesecret`). So a label containing anything outside the
/// model-name character set is treated as suspicious and collapsed to a generic
/// value. Legitimate names (which only use that set) pass through unchanged,
/// capped for layout; well-known families have already collapsed to a constant
/// upstream, so this only ever judges an unrecognized passthrough name.
fn sanitize_label(label: &str) -> String {
    const MAX: usize = 24;
    // Bound the scan: an untrusted name could be megabytes long, and there's no
    // need to examine past the first SCAN characters to make this decision.
    const SCAN: usize = 128;
    let label = label.trim();
    // `:` is allowed so local-model ids keep their tag (Ollama / OpenCode use
    // `qwen3:8b`-style names); path separators (`/`, `\`) stay out, so a smuggled
    // absolute path is still collapsed rather than published.
    let allowed = |ch: char| {
        ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '.' | '-' | '_' | '(' | ')' | '+' | ':')
    };
    // Validate on the iterator (no temp allocation): a label is suspicious if
    // it's empty or any of its first SCAN characters falls outside the set.
    if label.is_empty() || label.chars().take(SCAN).any(|ch| !allowed(ch)) {
        return "Other".to_owned();
    }
    // The label is already end-trimmed; only the MAX cut can leave a trailing
    // space, so trim that in place instead of allocating a second string.
    let mut capped: String = label.chars().take(MAX).collect();
    let trimmed_len = capped.trim_end().len();
    if trimmed_len == 0 {
        return "Other".to_owned();
    }
    capped.truncate(trimmed_len);
    capped
}

/// Strip a *known* `<provider>/` namespace from a gateway/proxy model id (agent
/// runners and routers use `provider/model` ids), so the model shows by its real
/// name. An *unknown* prefix is left intact — the surviving `/` then makes
/// `sanitize_label` collapse it to "Other", which is what keeps an arbitrary
/// `org/repo` (or a path) from reaching the card. A stale list degrades safely: a
/// new provider's model just shows as "Other" until it's added here.
fn strip_known_provider_prefix(name: &str) -> &str {
    const PROVIDERS: &[&str] = &[
        "openai",
        "anthropic",
        "google",
        "vertex_ai",
        "vertex",
        "meta-llama",
        "meta",
        "mistralai",
        "mistral",
        "x-ai",
        "xai",
        "deepseek",
        "qwen",
        "cohere",
        "perplexity",
        "azure",
        "bedrock",
        "openrouter",
        "together",
        "fireworks",
        "groq",
        "ollama",
    ];
    if let Some((prefix, rest)) = name.split_once('/')
        && !rest.is_empty()
        && PROVIDERS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(prefix))
    {
        return rest;
    }
    name
}

fn short_model_name_raw(name: &str) -> String {
    let name = strip_known_provider_prefix(name);
    let lower = name.to_ascii_lowercase();
    let family = if lower.contains("opus") {
        "Opus"
    } else if lower.contains("sonnet") {
        "Sonnet"
    } else if lower.contains("haiku") {
        "Haiku"
    } else if lower.contains("fable") {
        "Fable"
    } else if lower.contains("gemini") {
        "Gemini"
    } else if lower.contains("gpt") {
        return name.replace("gpt-", "GPT ");
    } else if lower == "openai" || lower == "codex" {
        return "Codex".to_owned();
    } else {
        return name.to_owned();
    };

    if family == "Gemini" {
        return name.to_owned();
    }

    for version in ["4-8", "4-7", "4-6", "4-5", "4-1", "5", "4"] {
        if lower.contains(version) {
            return format!("{family} {}", version.replace('-', "."));
        }
    }

    family.to_owned()
}

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
            "- {} volume:{} sessions:{} tools:{} durations:{} files:{} lines:{} parse_errors:{}",
            provider.provider.label(),
            format_tokens(provider.total_usage.token_volume()),
            provider.sessions,
            provider.tools.iter().map(|tool| tool.calls).sum::<usize>(),
            provider
                .completion_duration
                .as_ref()
                .map_or(0, |duration| duration.count),
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
        format!(
            "codename: {}",
            crate::codename::for_summary(summary).title()
        ),
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

fn indent_lines(value: &str, indent: &str) -> Vec<String> {
    value
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_tokens_with_compact_units() {
        assert_eq!(format_tokens(900), "900");
        assert_eq!(format_tokens(1_200), "1.2K");
        assert_eq!(format_tokens(1_200_000), "1.2M");
        assert_eq!(format_tokens(1_200_000_000), "1.2B");
    }

    #[test]
    fn usd_rounding_to_zero_is_never_negative() {
        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(-0.0), "$0.00");
        // A sub-cent residue rounds to a clean zero, not "-0.00".
        assert_eq!(format_usd(-0.0001), "$0.00");
        assert_eq!(format_usd(12.5), "$12.50");
    }

    #[test]
    fn formats_short_durations_as_seconds() {
        assert_eq!(format_duration_ms(12_345), "12s");
        assert_eq!(format_duration_ms(65_000), "1m");
    }

    #[test]
    fn shortens_claude_model_names() {
        assert_eq!(short_model_name("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(short_model_name("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(short_model_name("gpt-5.5"), "GPT 5.5");
        assert_eq!(short_model_name("custom-model"), "custom-model");
    }

    #[test]
    fn collapses_suspicious_model_names_for_the_card() {
        // A crafted name containing a path / newline / angle brackets is
        // collapsed to a generic label, NOT stripped-and-kept — so no readable
        // repo or path fragment ("Users", "secret") reaches the card.
        assert_eq!(
            short_model_name("gemini/Users/secret/repo\n<script>"),
            "Other"
        );
        // The gpt-prefixed passthrough is judged the same way.
        assert_eq!(short_model_name("gpt-/etc/passwd"), "Other");
        // Non-ASCII / control-only names collapse rather than yielding an empty
        // chip.
        assert_eq!(short_model_name("名前/\t"), "Other");
        // A legitimate display name with spaces and parens is preserved verbatim.
        assert_eq!(
            short_model_name("Gemini 3.5 Flash (High)"),
            "Gemini 3.5 Flash (High)"
        );
        // A local-model id keeps its `:tag` (Ollama / OpenCode) — not collapsed.
        assert_eq!(short_model_name("qwen3:8b"), "qwen3:8b");
    }

    #[test]
    fn strips_known_provider_namespace_but_collapses_unknown() {
        // Gateway / OpenCode `provider/model` ids show by their real name.
        assert_eq!(short_model_name("openai/gpt-4o"), "GPT 4o");
        assert_eq!(short_model_name("google/gemini-2.5-pro"), "gemini-2.5-pro");
        assert_eq!(short_model_name("anthropic/claude-opus-4-8"), "Opus 4.8");
        assert_eq!(short_model_name("mistralai/mistral-large"), "mistral-large");
        // An UNKNOWN prefix is not stripped, so the surviving '/' collapses it —
        // a `org/repo` (or path) can't smuggle a repo name onto the card.
        assert_eq!(short_model_name("myorg/secret-repo"), "Other");
        assert_eq!(short_model_name("openai/secret/repo"), "Other");
    }
}
