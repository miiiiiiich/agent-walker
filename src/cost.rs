//! API-equivalent cost estimation.
//!
//! Pricing comes from `LiteLLM`'s community pricing database, so rate changes
//! are tracked upstream without a release. Normal runs fetch current pricing
//! in parallel with report loading
//! and apply it before rendering or share actions. Only pricing metadata is
//! ever fetched; no usage data leaves the machine.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::model::TokenUsage;

const PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

static LOADED: OnceLock<RwLock<Option<Snapshot>>> = OnceLock::new();

/// USD per single token for each billing class. Missing classes are zero
/// (e.g. `OpenAI` does not bill cache writes).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write_5m: f64,
    #[serde(default)]
    pub cache_write_1h: f64,
}

#[derive(Deserialize)]
struct Snapshot {
    #[serde(rename = "_fetched", default)]
    fetched: Option<String>,
    models: HashMap<String, Pricing>,
}

fn loaded() -> &'static RwLock<Option<Snapshot>> {
    LOADED.get_or_init(|| RwLock::new(None))
}

fn parse_snapshot_json(raw: &str) -> Option<Snapshot> {
    let snapshot = serde_json::from_str::<Snapshot>(raw).ok()?;
    (!snapshot.models.is_empty()).then_some(snapshot)
}

/// Date the active pricing table was fetched from `LiteLLM`, if known.
pub fn pricing_as_of() -> Option<String> {
    loaded().read().ok()?.as_ref()?.fetched.clone()
}

/// Fetch the upstream `LiteLLM` database and reduce it to the snapshot
/// format: bare model ids (any provider) with per-token costs. No provider
/// allowlist — a model is priced if its id is in the table; unknown ids stay
/// $0. Provider/region duplicates and absurd rates are the actual guards.
fn fetch_snapshot_json() -> Option<String> {
    let response = ureq::get(PRICING_URL)
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?;
    let raw = response.into_string().ok()?;
    let upstream: HashMap<String, serde_json::Value> = serde_json::from_str(&raw).ok()?;

    let mut models = HashMap::new();
    for (key, entry) in &upstream {
        if key.contains('/')
            || key.starts_with("anthropic.")
            || key.starts_with("global.")
            || key.starts_with("us.")
            || key.starts_with("eu.")
            || key.starts_with("au.")
            || key.starts_with("apac.")
        {
            continue; // provider/region variants; keep bare model ids only
        }
        // Keep only conversational models. This drops embedding / image / audio /
        // rerank entries and non-model spec rows (e.g. `sample_spec`) that would
        // otherwise enter the table once the provider allowlist is gone.
        if !matches!(
            entry.get("mode").and_then(serde_json::Value::as_str),
            Some("chat" | "completion" | "responses")
        ) {
            continue;
        }
        // Reject non-finite, negative, or absurd rates (> $1/token) so a bad
        // upstream entry cannot poison cached cost estimates.
        let cost = |field: &str| {
            entry
                .get(field)
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite() && (0.0..1.0).contains(value))
        };
        let Some(input) = cost("input_cost_per_token") else {
            continue;
        };
        models.insert(
            key.clone(),
            Pricing {
                input,
                output: cost("output_cost_per_token").unwrap_or(0.0),
                cache_read: cost("cache_read_input_token_cost").unwrap_or(0.0),
                cache_write_5m: cost("cache_creation_input_token_cost").unwrap_or(0.0),
                cache_write_1h: cost("cache_creation_input_token_cost_above_1hr").unwrap_or(0.0),
            },
        );
    }
    if models.is_empty() {
        return None;
    }

    let fetched = time::OffsetDateTime::now_utc().date().to_string();
    serde_json::to_string_pretty(&serde_json::json!({
        "_source": PRICING_URL,
        "_fetched": fetched,
        "models": models,
    }))
    .ok()
}

/// Refresh active pricing from `LiteLLM`. A failed fetch or parse leaves the
/// last good snapshot in place — a transient network blip on a reload must not
/// blank the cost panel, zero out share cards, or flip provider ordering.
pub fn refresh_pricing() {
    let Some(serialized) = fetch_snapshot_json() else {
        debug!("pricing refresh skipped: fetch or parse failed; keeping last snapshot");
        return;
    };
    let Some(snapshot) = parse_snapshot_json(&serialized) else {
        debug!("pricing refresh skipped: generated snapshot did not parse; keeping last snapshot");
        return;
    };
    replace_loaded(Some(snapshot));
}

pub fn spawn_pricing_refresh() -> std::thread::JoinHandle<()> {
    std::thread::spawn(refresh_pricing)
}

fn replace_loaded(snapshot: Option<Snapshot>) {
    if let Some(loaded) = LOADED.get() {
        if let Ok(mut current) = loaded.write() {
            *current = snapshot;
        }
        return;
    }
    if let Err(snapshot) = LOADED.set(RwLock::new(snapshot))
        && let Some(loaded) = LOADED.get()
        && let Ok(snapshot) = snapshot.into_inner()
        && let Ok(mut current) = loaded.write()
    {
        *current = snapshot;
    }
}

/// Reduce a logged model label to a `LiteLLM`-style id:
/// strip deployment decorations ("claude-opus-4-8[1m]" -> "claude-opus-4-8"),
/// drop a trailing tier annotation ("Gemini 3.5 Flash (High)" -> "gemini 3.5
/// flash"), and hyphenate spaces so a display name maps to its bare id
/// ("gemini 3.5 flash" -> "gemini-3.5-flash").
fn normalize(model_name: &str) -> String {
    let lower = model_name.to_ascii_lowercase();
    let no_deploy = lower
        .split_once('[')
        .map_or(lower.as_str(), |(head, _)| head);
    let no_tier = no_deploy
        .split_once('(')
        .map_or(no_deploy, |(head, _)| head);
    let mut normalized = String::new();
    for word in no_tier.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push('-');
        }
        normalized.push_str(word);
    }
    normalized
}

/// Look up pricing: exact snapshot match first, then the longest snapshot key
/// the name extends with a date/version suffix (`claude-sonnet-4-5-20250929` ->
/// `claude-sonnet-4-5`). The suffix must be `-<digit>…` so a plain word suffix
/// (`gemini-pro-default`) never collides with a shorter base key — without that
/// guard, dropping the provider allowlist would let unrelated ids misprice.
pub fn pricing_for(model_name: &str) -> Option<Pricing> {
    let mut name = normalize(model_name);
    if name == "codex" || name == "openai" {
        // Codex sessions occasionally log only the provider name; price them
        // as the current Codex default model.
        "gpt-5.5".clone_into(&mut name);
    }

    let loaded = loaded().read().ok()?;
    let snapshot = loaded.as_ref()?;
    if let Some(pricing) = snapshot.models.get(&name) {
        return Some(*pricing);
    }
    if let Some(pricing) = snapshot
        .models
        .iter()
        .filter(|(key, _)| {
            name.strip_prefix(key.as_str()).is_some_and(|rest| {
                rest.strip_prefix('-')
                    .and_then(|s| s.chars().next())
                    .is_some_and(|c| c.is_ascii_digit())
            })
        })
        .max_by_key(|(key, _)| key.len())
        .map(|(_, pricing)| *pricing)
    {
        return Some(pricing);
    }
    None
}

/// API-equivalent USD cost of an aggregated usage block for a model.
/// Cache-aware; when the ephemeral 5m/1h split is unknown, all cache writes
/// are priced at the 5m rate.
#[allow(
    clippy::cast_precision_loss,
    reason = "Token counts are far below 2^52; estimate precision is dominated by pricing anyway."
)]
pub fn usage_cost_usd(model_name: &str, usage: &TokenUsage) -> Option<f64> {
    let pricing = pricing_for(model_name)?;

    // The 5m/1h split comes from untrusted logs; if it is absent, broken, or
    // would overflow, fall back to pricing every write at the 5m rate.
    let split = usage
        .cache_creation_ephemeral_5m_input_tokens
        .checked_add(usage.cache_creation_ephemeral_1h_input_tokens);
    let (write_short, write_long) = match split {
        Some(split) if split > 0 && split <= usage.cache_creation_input_tokens => (
            usage.cache_creation_ephemeral_5m_input_tokens
                + (usage.cache_creation_input_tokens - split),
            usage.cache_creation_ephemeral_1h_input_tokens,
        ),
        _ => (usage.cache_creation_input_tokens, 0),
    };

    Some(
        usage.input_tokens as f64 * pricing.input
            + usage.output_tokens as f64 * pricing.output
            + usage.cache_read_input_tokens as f64 * pricing.cache_read
            + write_short as f64 * pricing.cache_write_5m
            + write_long as f64 * pricing.cache_write_1h,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn install_test_pricing() {
        let per_mtok =
            |input: f64, output: f64, cache_write_5m: f64, cache_write_1h: f64| Pricing {
                input: input / 1e6,
                output: output / 1e6,
                cache_read: input * 0.1 / 1e6,
                cache_write_5m: cache_write_5m / 1e6,
                cache_write_1h: cache_write_1h / 1e6,
            };
        replace_loaded(Some(Snapshot {
            fetched: Some("test".to_owned()),
            models: HashMap::from([
                (
                    "claude-opus-4-8".to_owned(),
                    per_mtok(5.0, 25.0, 6.25, 10.0),
                ),
                (
                    "claude-sonnet-4-5".to_owned(),
                    per_mtok(3.0, 15.0, 3.75, 6.0),
                ),
                (
                    "gpt-5.5".to_owned(),
                    Pricing {
                        input: 5.0 / 1e6,
                        output: 30.0 / 1e6,
                        cache_read: 0.5 / 1e6,
                        cache_write_5m: 0.0,
                        cache_write_1h: 0.0,
                    },
                ),
                (
                    "gemini-3.5-flash".to_owned(),
                    Pricing {
                        input: 1.5 / 1e6,
                        output: 9.0 / 1e6,
                        cache_read: 0.15 / 1e6,
                        cache_write_5m: 0.0,
                        cache_write_1h: 0.0,
                    },
                ),
            ]),
        }));
    }

    #[test]
    fn prices_opus_usage_cache_aware() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: 10_000_000,
            cache_creation_input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let cost = usage_cost_usd("claude-opus-4-8[1m]", &usage).expect("opus should be priced");
        // 5 + 25 + 10 * 0.5 + 6.25 = 41.25
        assert!((cost - 41.25).abs() < 1e-9);
    }

    #[test]
    fn prices_one_hour_cache_writes_when_split_known() {
        install_test_pricing();
        let usage = TokenUsage {
            cache_creation_input_tokens: 1_000_000,
            cache_creation_ephemeral_1h_input_tokens: 1_000_000,
            output_tokens: 1,
            ..TokenUsage::default()
        };
        let cost = usage_cost_usd("claude-opus-4-8", &usage).expect("opus should be priced");
        // 1h writes bill at $10/MTok for Opus.
        assert!((cost - 10.0).abs() < 1e-3);
    }

    #[test]
    fn matches_date_suffixed_ids_by_prefix() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let cost =
            usage_cost_usd("claude-sonnet-4-5-20250929", &usage).expect("sonnet should be priced");
        assert!((cost - 3.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_word_suffix_prefix_match() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        // A non-date suffix must not collide with the shorter base key — only
        // `-<digit>…` (date/version) extends a prefix match.
        assert!(usage_cost_usd("claude-sonnet-4-5-experimental", &usage).is_none());
    }

    #[test]
    fn falls_back_to_short_rate_when_split_overflows() {
        install_test_pricing();
        let usage = TokenUsage {
            cache_creation_input_tokens: 1_000_000,
            cache_creation_ephemeral_5m_input_tokens: u64::MAX,
            cache_creation_ephemeral_1h_input_tokens: u64::MAX,
            output_tokens: 1,
            ..TokenUsage::default()
        };
        let cost = usage_cost_usd("claude-opus-4-8", &usage).expect("opus should be priced");
        // Overflowing split is ignored; all writes price at the 5m rate.
        assert!((cost - 6.25).abs() < 1e-3);
    }

    #[test]
    fn prices_gpt_and_skips_unknown() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 2_000_000,
            output_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let cost = usage_cost_usd("gpt-5.5", &usage).expect("gpt should be priced");
        assert!((cost - 40.0).abs() < 1e-9);
        assert!(usage_cost_usd("totally-unknown-model", &usage).is_none());
    }

    #[test]
    fn prices_antigravity_display_name() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        // Antigravity's display name normalizes (drop tier, hyphenate spaces) to
        // the bare LiteLLM id `gemini-3.5-flash`.
        let cost = usage_cost_usd("Gemini 3.5 Flash (High)", &usage)
            .expect("gemini display name should price");
        // 1.5 (input) + 9.0 (output) = 10.5
        assert!((cost - 10.5).abs() < 1e-9);
        // Versionless fallback id stays unpriced — too ambiguous to map.
        assert!(usage_cost_usd("gemini-pro-default", &usage).is_none());
    }
}
