//! The rates of the options a call was billed with (`model::Billing`).
use super::{ModelPricing, RateCard};
use crate::model::{Billing, ServiceTier};

impl ModelPricing {
    /// A service tier the table lists no rates for prices at the standard
    /// ones.
    pub(super) fn card(&self, tier: ServiceTier) -> RateCard {
        let listed = match tier {
            ServiceTier::Priority => self.priority,
            ServiceTier::Ultrafast => self.ultrafast,
            ServiceTier::Standard | ServiceTier::Fast => None,
        };
        listed.unwrap_or(RateCard {
            base: self.base,
            long_context: self.long_context,
        })
    }

    /// One factor on the whole request: both multipliers scale every rate,
    /// long-context ones included, and stack (`docs/claude.md`).
    pub(super) fn multiplier(&self, billing: Billing) -> f64 {
        let fast = match billing.tier {
            ServiceTier::Fast => self.fast,
            ServiceTier::Standard | ServiceTier::Priority | ServiceTier::Ultrafast => None,
        };
        let us = if billing.us_only { self.us } else { None };
        fast.unwrap_or(1.0) * us.unwrap_or(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::{SNAPSHOT_FORMAT, Snapshot, litellm, parse_snapshot_json};
    use crate::model::{TokenUsage, UsageGranularity};

    /// Through the stored form, as every fetch is: the table is filtered,
    /// written as the snapshot and read back before it prices anything.
    fn stored(table: &serde_json::Value) -> Snapshot {
        let models = litellm::models_from_table(table.to_string().as_bytes()).unwrap();
        let serialized = serde_json::json!({
            "_fetched": "2026-11-22",
            "_format": SNAPSHOT_FORMAT,
            "models": models,
        })
        .to_string();
        parse_snapshot_json(&serialized).expect("the stored snapshot parses")
    }

    fn cost(snapshot: &Snapshot, model: &str, usage: &TokenUsage, billing: Billing) -> f64 {
        let pricing = snapshot.resolve(model).expect("model priced");
        super::super::priced(&pricing, usage, UsageGranularity::Call, billing)
    }

    fn table() -> serde_json::Value {
        serde_json::json!({
            "claude-opus-5-5": {
                "mode": "chat",
                "input_cost_per_token": 4e-6,
                "output_cost_per_token": 2e-5,
                "cache_read_input_token_cost": 2e-7,
                "cache_creation_input_token_cost": 5e-6,
                "cache_creation_input_token_cost_above_1hr": 8e-6,
                "provider_specific_entry": {"us": 1.1, "fast": 2}
            },
            "claude-haiku-5-5": {
                "mode": "chat",
                "input_cost_per_token": 1e-7,
                "output_cost_per_token": 5e-7,
                "input_cost_per_token_above_100k_tokens": 5e-7,
                "output_cost_per_token_above_100k_tokens": 2.5e-6,
                "provider_specific_entry": {"us": 1.1}
            },
            "claude-unlisted": {"mode": "chat", "input_cost_per_token": 1e-6},
            "claude-poisoned": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "provider_specific_entry": {"fast": 0, "us": 11}
            }
        })
    }

    const FAST: Billing = Billing {
        tier: ServiceTier::Fast,
        us_only: false,
    };
    const US: Billing = Billing {
        tier: ServiceTier::Standard,
        us_only: true,
    };
    const FAST_US: Billing = Billing {
        tier: ServiceTier::Fast,
        us_only: true,
    };

    /// Every rate scales, cache reads and both write rates included, and the
    /// two multipliers stack.
    #[test]
    fn fast_and_us_scale_every_rate_and_stack() {
        let snapshot = stored(&table());
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 1_000,
            cache_read_input_tokens: 100_000,
            cache_creation_input_tokens: 10_000,
            cache_creation_ephemeral_1h_input_tokens: 6_000,
            ..TokenUsage::default()
        };
        let standard = cost(&snapshot, "claude-opus-5-5", &usage, Billing::default());
        let expected =
            1_000.0 * 4e-6 + 1_000.0 * 2e-5 + 100_000.0 * 2e-7 + 4_000.0 * 5e-6 + 6_000.0 * 8e-6;
        assert!(
            (standard - expected).abs() < 1e-12,
            "{standard} vs {expected}"
        );
        let close = |billing: Billing, factor: f64| {
            let priced = cost(&snapshot, "claude-opus-5-5", &usage, billing);
            assert!(
                (priced - expected * factor).abs() < 1e-12,
                "x{factor}: {priced}"
            );
        };
        close(FAST, 2.0);
        close(US, 1.1);
        close(FAST_US, 2.2);
    }

    fn openai_table() -> serde_json::Value {
        serde_json::json!({
            "gpt-6-astra": {
                "mode": "chat",
                "input_cost_per_token": 1e-5,
                "output_cost_per_token": 5e-5,
                "cache_read_input_token_cost": 1e-6,
                "input_cost_per_token_above_272k_tokens": 2e-5,
                "output_cost_per_token_above_272k_tokens": 7.5e-5,
                "cache_read_input_token_cost_above_272k_tokens": 2e-6,
                "input_cost_per_token_priority": 2e-5,
                "output_cost_per_token_priority": 1e-4,
                "cache_read_input_token_cost_priority": 2e-6,
                "input_cost_per_token_above_272k_tokens_priority": 4e-5,
                "output_cost_per_token_above_272k_tokens_priority": 1.5e-4,
                "cache_read_input_token_cost_above_272k_tokens_priority": 4e-6,
                "input_cost_per_token_ultrafast": 6e-5,
                "cache_read_input_token_cost_ultrafast": 6e-6
            },
            "gpt-standard-only": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "output_cost_per_token_priority": 9e-6
            },
            "gpt-poisoned-tier": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "input_cost_per_token_priority": 2e-6,
                "output_cost_per_token_priority": 0.5
            }
        })
    }

    const PRIORITY: Billing = Billing {
        tier: ServiceTier::Priority,
        us_only: false,
    };
    const ULTRAFAST: Billing = Billing {
        tier: ServiceTier::Ultrafast,
        us_only: false,
    };

    /// A tier's calls pay its own rates, its own long-context rates past the
    /// threshold, and the standard rate for what the tier does not list.
    #[test]
    fn service_tiers_price_at_their_own_rates() {
        let snapshot = stored(&openai_table());
        let short = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 1_000,
            cache_read_input_tokens: 100_000,
            ..TokenUsage::default()
        };
        let priced =
            |billing: Billing, usage: &TokenUsage| cost(&snapshot, "gpt-6-astra", usage, billing);
        let close = |left: f64, right: f64| (left - right).abs() < 1e-12;
        assert!(close(
            priced(Billing::default(), &short),
            1_000.0 * 1e-5 + 1_000.0 * 5e-5 + 100_000.0 * 1e-6
        ));
        assert!(close(
            priced(PRIORITY, &short),
            1_000.0 * 2e-5 + 1_000.0 * 1e-4 + 100_000.0 * 2e-6
        ));
        assert!(close(
            priced(ULTRAFAST, &short),
            1_000.0 * 6e-5 + 1_000.0 * 5e-5 + 100_000.0 * 6e-6
        ));
        let long = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 1_000,
            cache_read_input_tokens: 300_000,
            ..TokenUsage::default()
        };
        assert!(close(
            priced(PRIORITY, &long),
            1_000.0 * 4e-5 + 1_000.0 * 1.5e-4 + 300_000.0 * 4e-6
        ));
        let pricing = snapshot.resolve("gpt-6-astra").expect("model priced");
        let aggregate =
            super::super::priced(&pricing, &long, UsageGranularity::Aggregate, PRIORITY);
        assert!(close(
            aggregate,
            1_000.0 * 2e-5 + 1_000.0 * 1e-4 + 300_000.0 * 2e-6
        ));
    }

    /// No input rate of its own, or a rate out of bounds, is no tier: the
    /// calls pay the standard rates.
    #[test]
    fn a_missing_or_refused_service_tier_prices_at_standard_rates() {
        let snapshot = stored(&openai_table());
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        for model in ["gpt-standard-only", "gpt-poisoned-tier"] {
            let standard = cost(&snapshot, model, &usage, Billing::default());
            for billing in [PRIORITY, ULTRAFAST] {
                let priced = cost(&snapshot, model, &usage, billing);
                assert!(
                    (priced - standard).abs() < 1e-12,
                    "{model}: {priced} vs {standard}"
                );
            }
        }
    }

    /// The US multiplier applies on top of the long-context rates, and a
    /// model without a fast multiplier keeps its rates in fast mode.
    #[test]
    fn us_scales_long_context_rates_and_unlisted_multipliers_change_nothing() {
        let snapshot = stored(&table());
        let long = TokenUsage {
            input_tokens: 200_000,
            output_tokens: 1_000,
            ..TokenUsage::default()
        };
        let expected = 200_000.0 * 5e-7 + 1_000.0 * 2.5e-6;
        let us = cost(&snapshot, "claude-haiku-5-5", &long, US);
        assert!((us - expected * 1.1).abs() < 1e-12, "{us}");
        let fast = cost(&snapshot, "claude-haiku-5-5", &long, FAST);
        assert!((fast - expected).abs() < 1e-12, "{fast}");
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        for model in ["claude-unlisted", "claude-poisoned"] {
            let priced = cost(&snapshot, model, &usage, FAST_US);
            assert!((priced - 1.0).abs() < 1e-12, "{model}: {priced}");
        }
    }
}
