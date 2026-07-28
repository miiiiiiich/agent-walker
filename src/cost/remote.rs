//! The ONLY network egress in the cost pipeline (and, besides the opt-in
//! Cursor collector, in the whole binary): fetching LiteLLM's community
//! pricing table. Anything that changes what leaves the machine or where it
//! goes lives in this file — a diff touching `cost/remote.rs` is an egress
//! change by definition. Only pricing metadata is fetched; no usage data is
//! ever sent.
use std::collections::HashMap;
use std::time::Duration;

use tracing::debug;

use super::{Pricing, parse_snapshot_json, replace_loaded};

const PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

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
        // Provider/region variants are dropped in favor of bare model ids —
        // EXCEPT `xai/`: LiteLLM registers Grok models only under the
        // provider prefix (`xai/grok-4.5`, no bare key), so the prefix is
        // stripped instead, or Grok Build usage would price at $0.
        let key = key.strip_prefix("xai/").unwrap_or(key);
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
            key.to_owned(),
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
pub(super) fn refresh_pricing() {
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
