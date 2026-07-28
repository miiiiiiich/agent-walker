use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

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

mod model_label;
mod snapshot;

pub use model_label::short_model_name;
pub use snapshot::snapshot_app;

#[cfg(test)]
mod tests {
    use super::snapshot::snapshot;
    use super::*;

    #[test]
    fn snapshot_carries_the_rank_line() {
        // 250M/day over the 30-day window → A band; the fixture itself
        // (≈700K/day) stays unranked.
        let mut summary = crate::share::fixtures::sample_summary();
        summary.recent_window_volume = 7_500_000_000;
        summary.recent_window_active_days = 29;
        assert!(snapshot(&summary).contains("rank: A"));
        assert!(snapshot(&crate::share::fixtures::sample_summary()).contains("rank: unranked"));
    }

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
