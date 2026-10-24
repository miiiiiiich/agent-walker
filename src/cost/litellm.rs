//! Map `LiteLLM`'s pricing table onto the rates `cost` prices with.
use std::collections::HashMap;

use serde_json::Value;

use super::{LongContext, ModelPricing, Pricing};

pub(super) fn models_from_upstream(
    upstream: &HashMap<String, Value>,
) -> HashMap<String, ModelPricing> {
    let mut models = HashMap::new();
    for (key, entry) in upstream {
        // Provider/region variants are dropped in favor of bare model ids —
        // EXCEPT `xai/`: LiteLLM registers Grok models only under the
        // provider prefix (`xai/grok-4.5`, no bare key), so the prefix is
        // stripped so those models can resolve to a price.
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
        if !matches!(
            entry.get("mode").and_then(Value::as_str),
            Some("chat" | "completion" | "responses")
        ) {
            continue;
        }
        // Reject non-finite, negative, or absurd rates (> $1/token) so a bad
        // upstream entry cannot poison cached cost estimates.
        let cost = |field: &str| {
            entry
                .get(field)
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && (0.0..1.0).contains(value))
        };
        let Some(input) = cost("input_cost_per_token") else {
            continue;
        };
        let (write_short, write_long) = write_rates(
            input,
            cost("cache_creation_input_token_cost"),
            cost("cache_creation_input_token_cost_above_1hr"),
        );
        let base = Pricing {
            input,
            output: cost("output_cost_per_token").unwrap_or(0.0),
            cache_read: cost("cache_read_input_token_cost").unwrap_or(0.0),
            cache_write_5m: write_short,
            cache_write_1h: write_long,
        };
        models.insert(
            key.to_owned(),
            ModelPricing {
                base,
                long_context: long_context(entry, &base, &cost),
            },
        );
    }
    models
}

/// A write rate that is missing or 0 falls back to the input rate: write
/// tokens are prompt tokens kept out of `input_tokens`, so a 0 would price
/// them as free. A missing 1h rate falls back to the 5m rate, the nearest
/// listed write rate.
fn write_rates(input: f64, short: Option<f64>, long: Option<f64>) -> (f64, f64) {
    let listed = |rate: Option<f64>| rate.filter(|rate| *rate > 0.0);
    let short = listed(short).unwrap_or(input);
    (short, listed(long).unwrap_or(short))
}

/// A rate the tier leaves out keeps its listed base value, matching
/// `LiteLLM`'s own calculator; a write rate listed nowhere follows the tier's
/// input rate, by the `write_rates` rule. Of several thresholds only the
/// lowest is kept: one tier per model keeps pricing a plain `Copy` value.
fn long_context(
    entry: &Value,
    base: &Pricing,
    cost: &impl Fn(&str) -> Option<f64>,
) -> Option<LongContext> {
    let (above_prompt_tokens, label) = entry
        .as_object()?
        .keys()
        .filter_map(|key| {
            let label = key
                .strip_prefix("input_cost_per_token_above_")?
                .strip_suffix("_tokens")?;
            let tokens = match label.strip_suffix('k') {
                Some(thousands) => thousands.parse::<u64>().ok()?.checked_mul(1_000)?,
                None => label.parse::<u64>().ok()?,
            };
            (tokens > 0).then_some((tokens, label))
        })
        .min_by_key(|(tokens, _)| *tokens)?;
    let tier = |field: &str| cost(&format!("{field}_above_{label}_tokens"));
    let input = tier("input_cost_per_token")?;
    let write = |field: &str| {
        tier(field)
            .filter(|rate| *rate > 0.0)
            .or_else(|| cost(field))
    };
    let (write_short, write_long) = write_rates(
        input,
        write("cache_creation_input_token_cost"),
        write("cache_creation_input_token_cost_above_1hr"),
    );
    Some(LongContext {
        above_prompt_tokens,
        rates: Pricing {
            input,
            output: tier("output_cost_per_token").unwrap_or(base.output),
            cache_read: tier("cache_read_input_token_cost").unwrap_or(base.cache_read),
            cache_write_5m: write_short,
            cache_write_1h: write_long,
        },
    })
}

/// Apply the `write_rates` rule to rates stored by an older build, which kept
/// a missing write rate as 0.
pub(super) fn backfill_write_rates(models: &mut HashMap<String, ModelPricing>) {
    let backfill = |rates: &mut Pricing| {
        (rates.cache_write_5m, rates.cache_write_1h) = write_rates(
            rates.input,
            Some(rates.cache_write_5m),
            Some(rates.cache_write_1h),
        );
    };
    for pricing in models.values_mut() {
        backfill(&mut pricing.base);
        if let Some(tier) = &mut pricing.long_context {
            backfill(&mut tier.rates);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(entries: Value) -> HashMap<String, ModelPricing> {
        let upstream: HashMap<String, Value> = serde_json::from_value(entries).unwrap();
        models_from_upstream(&upstream)
    }

    #[test]
    fn long_context_tiers_parse_with_every_rate() {
        let models = upstream(serde_json::json!({
            "claude-haiku-5-5": {
                "mode": "chat",
                "input_cost_per_token": 1e-7,
                "output_cost_per_token": 5e-7,
                "cache_read_input_token_cost": 1e-8,
                "cache_creation_input_token_cost": 1.25e-7,
                "cache_creation_input_token_cost_above_1hr": 2e-7,
                "input_cost_per_token_above_100k_tokens": 5e-7,
                "input_cost_per_token_above_100k_tokens_batches": 2.5e-7,
                "output_cost_per_token_above_100k_tokens": 2.5e-6,
                "cache_read_input_token_cost_above_100k_tokens": 5e-8,
                "cache_creation_input_token_cost_above_100k_tokens": 6.25e-7,
                "cache_creation_input_token_cost_above_1hr_above_100k_tokens": 1e-6
            }
        }));
        let tier = models["claude-haiku-5-5"]
            .long_context
            .expect("tiered model keeps its tier");
        assert_eq!(tier.above_prompt_tokens, 100_000);
        let close = |left: f64, right: f64| (left - right).abs() < 1e-15;
        assert!(close(tier.rates.input, 5e-7));
        assert!(close(tier.rates.output, 2.5e-6));
        assert!(close(tier.rates.cache_read, 5e-8));
        assert!(close(tier.rates.cache_write_5m, 6.25e-7));
        assert!(close(tier.rates.cache_write_1h, 1e-6));
    }

    /// A rate the tier does not list stays at its base value; a tier whose own
    /// input rate is unusable, or that exists only for batch pricing, is no
    /// tier at all.
    #[test]
    fn partial_or_unusable_tiers_fall_back() {
        let models = upstream(serde_json::json!({
            "gpt-5.5": {
                "mode": "chat",
                "input_cost_per_token": 5e-6,
                "output_cost_per_token": 3e-5,
                "cache_read_input_token_cost": 5e-7,
                "input_cost_per_token_above_272k_tokens": 1e-5,
                "output_cost_per_token_above_272k_tokens": 4.5e-5
            },
            "poisoned": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "input_cost_per_token_above_200k_tokens": 5.0
            },
            "batch-only": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "input_cost_per_token_above_200k_tokens_batches": 5e-7
            }
        }));
        let tier = models["gpt-5.5"].long_context.expect("tier present");
        assert_eq!(tier.above_prompt_tokens, 272_000);
        assert!((tier.rates.cache_read - 5e-7).abs() < 1e-15);
        assert!(models["poisoned"].long_context.is_none());
        assert!(models["batch-only"].long_context.is_none());
    }

    /// Codex reports cache writes apart from input; a model with no write
    /// rate in `LiteLLM` must not price them at $0.
    #[test]
    fn missing_or_zero_write_rates_fall_back_to_input() {
        let models = upstream(serde_json::json!({
            "gpt-5.5": {
                "mode": "chat",
                "input_cost_per_token": 5e-6,
                "input_cost_per_token_above_272k_tokens": 1e-5
            },
            "deepseek-v4-pro": {
                "mode": "chat",
                "input_cost_per_token": 2e-6,
                "cache_creation_input_token_cost": 0.0
            },
            "claude-like": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "cache_creation_input_token_cost": 1.25e-6
            }
        }));
        let close = |left: f64, right: f64| (left - right).abs() < 1e-15;
        let gpt = models["gpt-5.5"];
        assert!(close(gpt.base.cache_write_5m, 5e-6));
        assert!(close(gpt.base.cache_write_1h, 5e-6));
        let tier = gpt.long_context.expect("tier present");
        assert!(close(tier.rates.cache_write_5m, 1e-5));
        assert!(close(tier.rates.cache_write_1h, 1e-5));
        assert!(close(models["deepseek-v4-pro"].base.cache_write_5m, 2e-6));
        let claude = models["claude-like"].base;
        assert!(close(claude.cache_write_5m, 1.25e-6));
        assert!(close(claude.cache_write_1h, 1.25e-6));
    }
}
