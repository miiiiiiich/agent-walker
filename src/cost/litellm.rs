//! Map `LiteLLM`'s pricing table onto the rates `cost` prices with.
use std::collections::HashMap;

use super::validate::in_bounds;
use super::{LongContext, ModelPricing, Pricing};

mod table;

use table::{CACHE_READ, Entry, INPUT, OUTPUT, Rates, Table, WRITE_1H, WRITE_5M};

/// A listed rate out of bounds refuses its model (or tier) instead of
/// counting as unlisted: an unlisted output or cache-read rate prices at $0,
/// and the total would pass as complete.
fn refuses(rates: &Rates) -> bool {
    rates.iter().flatten().any(|rate| !in_bounds(*rate))
}

pub(super) fn models_from_table(
    raw: &[u8],
) -> Result<HashMap<String, ModelPricing>, serde_json::Error> {
    let Table(entries) = serde_json::from_slice(raw)?;
    Ok(entries
        .into_iter()
        .filter_map(|(id, entry)| Some((id, model_pricing(&entry)?)))
        .collect())
}

fn model_pricing(entry: &Entry) -> Option<ModelPricing> {
    if refuses(&entry.rates) {
        return None;
    }
    let listed = |slot: usize| entry.rates[slot];
    let input = listed(INPUT)?;
    let (write_short, write_long) = write_rates(input, listed(WRITE_5M), listed(WRITE_1H));
    let base = Pricing {
        input,
        output: listed(OUTPUT).unwrap_or(0.0),
        cache_read: listed(CACHE_READ).unwrap_or(0.0),
        cache_write_5m: write_short,
        cache_write_1h: write_long,
    };
    Some(ModelPricing {
        base,
        long_context: long_context(entry, &base),
    })
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
/// lowest at or over the floor is kept: one tier per model keeps pricing a
/// plain `Copy` value.
fn long_context(entry: &Entry, base: &Pricing) -> Option<LongContext> {
    if entry.too_many_tiers {
        return None;
    }
    let tier = entry
        .tiers
        .iter()
        .filter(|tier| tier.has_input)
        .min_by(|left, right| {
            (left.above_prompt_tokens, &left.label).cmp(&(right.above_prompt_tokens, &right.label))
        })?;
    if refuses(&tier.rates) {
        return None;
    }
    let input = tier.rates[INPUT]?;
    let write = |slot: usize| {
        tier.rates[slot]
            .filter(|rate| *rate > 0.0)
            .or(entry.rates[slot])
    };
    let (write_short, write_long) = write_rates(input, write(WRITE_5M), write(WRITE_1H));
    Some(LongContext {
        above_prompt_tokens: tier.above_prompt_tokens,
        rates: Pricing {
            input,
            output: tier.rates[OUTPUT].unwrap_or(base.output),
            cache_read: tier.rates[CACHE_READ].unwrap_or(base.cache_read),
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

    fn upstream(entries: &serde_json::Value) -> HashMap<String, ModelPricing> {
        models_from_table(entries.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn long_context_tiers_parse_with_every_rate() {
        let models = upstream(&serde_json::json!({
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
        let models = upstream(&serde_json::json!({
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

    /// Out of bounds, a base rate drops its model (unpriced, never $0) and a
    /// tier rate drops only the tier.
    #[test]
    fn rates_out_of_bounds_refuse_their_model_or_tier() {
        let models = upstream(&serde_json::json!({
            "o1-pro-like": {
                "mode": "chat",
                "input_cost_per_token": 1.5e-4,
                "output_cost_per_token": 6e-4
            },
            "inflated-input": {"mode": "chat", "input_cost_per_token": 0.5},
            "inflated-output": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "output_cost_per_token": 0.05
            },
            "negative-cache-read": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "cache_read_input_token_cost": -1e-7
            },
            "inflated-write": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "cache_creation_input_token_cost_above_1hr": 0.9
            },
            "inflated-tier-output": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "input_cost_per_token_above_200k_tokens": 2e-6,
                "output_cost_per_token_above_200k_tokens": 0.5
            }
        }));
        let mut kept: Vec<&str> = models.keys().map(String::as_str).collect();
        kept.sort_unstable();
        assert_eq!(kept, ["inflated-tier-output", "o1-pro-like"]);
        assert!(models["inflated-tier-output"].long_context.is_none());
    }

    /// A threshold of a few tokens would bill nearly every request at the
    /// tier; it is skipped and the next listed threshold is used.
    #[test]
    fn thresholds_below_the_floor_are_skipped() {
        let models = upstream(&serde_json::json!({
            "one-token": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "input_cost_per_token_above_1_tokens": 9e-3,
                "input_cost_per_token_above_200k_tokens": 2e-6
            },
            "below-floor-only": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "input_cost_per_token_above_999_tokens": 2e-6
            },
            "at-floor": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "input_cost_per_token_above_1k_tokens": 2e-6
            }
        }));
        let tier = models["one-token"].long_context.expect("200k tier");
        assert_eq!(tier.above_prompt_tokens, 200_000);
        assert!((tier.rates.input - 2e-6).abs() < 1e-15);
        assert!(models["below-floor-only"].long_context.is_none());
        let floor = models["at-floor"].long_context.expect("1k tier");
        assert_eq!(floor.above_prompt_tokens, 1_000);
    }

    /// Codex reports cache writes apart from input; a model with no write
    /// rate in `LiteLLM` must not price them at $0.
    #[test]
    fn missing_or_zero_write_rates_fall_back_to_input() {
        let models = upstream(&serde_json::json!({
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
