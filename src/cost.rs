//! API-equivalent cost estimation.
//!
//! Pricing comes from `LiteLLM`'s community pricing database — the same
//! source ccusage uses, so rate changes are tracked upstream without a
//! release. Resolution order: local cache (refreshed in the background at
//! most once a day) -> compile-time snapshot (`assets/pricing.json`,
//! refreshed at release time via `--update-pricing-snapshot`) ->
//! per-family fallback table. Only pricing metadata is ever fetched; no
//! usage data leaves the machine, and `--offline` disables the fetch.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::model::TokenUsage;

const PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

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

fn pricing_cache_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("agentwalker")
            .join("pricing.json"),
    )
}

fn loaded() -> &'static Snapshot {
    static LOADED: OnceLock<Snapshot> = OnceLock::new();
    LOADED.get_or_init(|| {
        // Prefer the locally refreshed cache; fall back to the snapshot
        // embedded at compile time.
        if let Some(path) = pricing_cache_path()
            && let Ok(raw) = fs::read_to_string(&path)
            && let Ok(snapshot) = serde_json::from_str::<Snapshot>(&raw)
            && !snapshot.models.is_empty()
        {
            return snapshot;
        }
        // The vendored snapshot is a build asset we own; failing to parse it
        // is a packaging bug that must be loud, not an empty COST panel.
        serde_json::from_str::<Snapshot>(include_str!("../assets/pricing.json"))
            .expect("invariant: vendored assets/pricing.json must parse")
    })
}

fn snapshot() -> &'static HashMap<String, Pricing> {
    &loaded().models
}

/// Date the active pricing table was fetched from `LiteLLM`, if known.
pub fn pricing_as_of() -> Option<&'static str> {
    loaded().fetched.as_deref()
}

/// Fetch the upstream `LiteLLM` database and reduce it to the snapshot
/// format: bare claude-*/gpt-* model ids with per-token costs. Single source
/// of truth for both the daily cache refresh and the vendored
/// `assets/pricing.json` (see `--update-pricing-snapshot`).
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
        if !(key.starts_with("claude-") || key.starts_with("gpt-")) {
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

/// Refresh the vendored snapshot on disk (release tooling — the hidden
/// `--update-pricing-snapshot` flag).
pub fn update_snapshot_file(path: &std::path::Path) -> anyhow::Result<()> {
    let json = fetch_snapshot_json()
        .ok_or_else(|| anyhow::anyhow!("failed to fetch or parse the LiteLLM pricing database"))?;
    fs::write(path, json)?;
    Ok(())
}

/// Refresh the pricing cache from `LiteLLM` in a detached background thread.
/// The running process keeps its already-loaded table; the next launch picks
/// up the fresh rates. No-op when offline or when the cache is under a day
/// old. Only pricing metadata is fetched — nothing is ever uploaded.
pub fn refresh_pricing_in_background(offline: bool) {
    if offline {
        return;
    }
    let Some(path) = pricing_cache_path() else {
        return;
    };
    let fresh = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < REFRESH_INTERVAL);
    if fresh {
        return;
    }
    std::thread::spawn(move || {
        let Some(serialized) = fetch_snapshot_json() else {
            debug!("pricing refresh skipped: fetch or parse failed");
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let temp = path.with_extension(format!("tmp.{}", std::process::id()));
        if fs::write(&temp, serialized).is_ok() {
            let _ = fs::rename(&temp, &path);
        }
    });
}

/// Strip deployment decorations the logs add to model ids
/// ("claude-opus-4-8[1m]" -> "claude-opus-4-8").
fn normalize(model_name: &str) -> String {
    let lower = model_name.to_ascii_lowercase();
    lower
        .split_once('[')
        .map_or(lower.clone(), |(head, _)| head.to_owned())
        .trim()
        .to_owned()
}

/// Look up pricing: exact snapshot match first, then the longest snapshot
/// key the name starts with (date-suffixed ids), then a family fallback.
pub fn pricing_for(model_name: &str) -> Option<Pricing> {
    let mut name = normalize(model_name);
    if name == "codex" || name == "openai" {
        // Codex sessions occasionally log only the provider name; price them
        // as the current Codex default model.
        "gpt-5.5".clone_into(&mut name);
    }

    let models = snapshot();
    if let Some(pricing) = models.get(&name) {
        return Some(*pricing);
    }
    if let Some(pricing) = models
        .iter()
        .filter(|(key, _)| name.starts_with(key.as_str()))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, pricing)| *pricing)
    {
        return Some(pricing);
    }
    family_fallback(&name)
}

/// Last-resort per-family rates (USD/MTok as of 2026-06, vendor pages).
fn family_fallback(name: &str) -> Option<Pricing> {
    let per_mtok = |input: f64, output: f64| -> Pricing {
        Pricing {
            input: input / 1e6,
            output: output / 1e6,
            cache_read: input * 0.1 / 1e6,
            cache_write_5m: input * 1.25 / 1e6,
            cache_write_1h: input * 2.0 / 1e6,
        }
    };
    if name.contains("fable") {
        Some(per_mtok(10.0, 50.0))
    } else if name.contains("opus") {
        Some(per_mtok(5.0, 25.0))
    } else if name.contains("sonnet") {
        Some(per_mtok(3.0, 15.0))
    } else if name.contains("haiku") {
        Some(per_mtok(1.0, 5.0))
    } else if name.contains("gpt") {
        Some(Pricing {
            input: 5.0 / 1e6,
            output: 30.0 / 1e6,
            cache_read: 0.5 / 1e6,
            cache_write_5m: 0.0,
            cache_write_1h: 0.0,
        })
    } else {
        None
    }
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
    use super::*;

    #[test]
    fn prices_opus_usage_cache_aware() {
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
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let cost =
            usage_cost_usd("claude-sonnet-4-5-20250929", &usage).expect("sonnet should be priced");
        assert!((cost - 3.0).abs() < 1e-9);
    }

    #[test]
    fn falls_back_to_short_rate_when_split_overflows() {
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
        let usage = TokenUsage {
            input_tokens: 2_000_000,
            output_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let cost = usage_cost_usd("gpt-5.5", &usage).expect("gpt should be priced");
        assert!((cost - 40.0).abs() < 1e-9);
        assert!(usage_cost_usd("Gemini 3.5 Flash (High)", &usage).is_none());
    }
}
