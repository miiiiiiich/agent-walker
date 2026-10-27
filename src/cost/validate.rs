//! The bounds a rate must meet before it prices anything. The fetch applies
//! them field by field, and the stored snapshot gets them again when it is
//! read back, since that file can be edited or left behind by another build.
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{Deserialize, Deserializer, Error, MapAccess, Visitor};

use super::{LongContext, ModelPricing, Pricing};

/// USD per token. Far above every rate the table lists (`docs/claude.md`
/// has the margin), so it refuses no real model; what it stops is one entry
/// pricing a month of tokens in the millions.
const MAX_RATE_PER_TOKEN: f64 = 1e-2;

/// A lower threshold would put nearly every request on the tier's rates.
pub(super) const MIN_TIER_PROMPT_TOKENS: u64 = 1_000;

/// Entries in one table, filtered or not: about ten times what `LiteLLM`
/// lists (`docs/claude.md` has the count). Past it the whole table is
/// refused; keeping some entries would depend on their order.
pub(super) const MAX_TABLE_ENTRIES: usize = 50_000;

pub(super) fn in_bounds(rate: f64) -> bool {
    (0.0..=MAX_RATE_PER_TOKEN).contains(&rate)
}

fn rates_in_bounds(rates: &Pricing) -> bool {
    [
        rates.input,
        rates.output,
        rates.cache_read,
        rates.cache_write_5m,
        rates.cache_write_1h,
    ]
    .into_iter()
    .all(in_bounds)
}

fn tier_in_bounds(tier: &LongContext) -> bool {
    tier.above_prompt_tokens >= MIN_TIER_PROMPT_TOKENS && rates_in_bounds(&tier.rates)
}

/// The fetch's outcome for the same rates: a bad base rate drops the model,
/// which then shows as unpriced; a bad tier is dropped and the model prices
/// at its base rates.
pub(super) fn retain_in_bounds(models: &mut HashMap<String, ModelPricing>) {
    models.retain(|_, pricing| {
        if pricing
            .long_context
            .as_ref()
            .is_some_and(|tier| !tier_in_bounds(tier))
        {
            pricing.long_context = None;
        }
        rates_in_bounds(&pricing.base)
    });
}

/// `deserialize_with` for a stored snapshot's models: the same entry bound
/// as the fetch, checked before each entry is parsed.
pub(super) fn bounded_map<'de, D, V>(deserializer: D) -> Result<HashMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct Bounded<V>(PhantomData<V>);

    impl<'de, V: Deserialize<'de>> Visitor<'de> for Bounded<V> {
        type Value = HashMap<String, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(
                formatter,
                "an object of at most {MAX_TABLE_ENTRIES} entries"
            )
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut entries = HashMap::new();
            let mut seen = 0_usize;
            while let Some(key) = map.next_key::<String>()? {
                seen += 1;
                if seen > MAX_TABLE_ENTRIES {
                    return Err(A::Error::custom(format_args!(
                        "more than {MAX_TABLE_ENTRIES} entries"
                    )));
                }
                entries.insert(key, map.next_value()?);
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(Bounded(PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::parse_snapshot_json;

    #[test]
    fn bounds_are_inclusive_and_refuse_non_finite_rates() {
        assert!(in_bounds(0.0));
        assert!(in_bounds(MAX_RATE_PER_TOKEN));
        for rate in [
            -1e-12,
            MAX_RATE_PER_TOKEN * 1.000_001,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            assert!(!in_bounds(rate), "{rate}");
        }
    }

    /// A stored file is no more trusted than a fetch: the same bounds apply
    /// when it is read back.
    #[test]
    fn a_stored_snapshot_keeps_only_rates_in_bounds() {
        let raw = r#"{"_fetched":"2026-10-26","_format":2,"models":{
            "kept":{"input":5e-6,"output":2.5e-5},
            "at-ceiling":{"input":1e-2,"output":0.0},
            "negative":{"input":-5e-6,"output":0.0},
            "inflated":{"input":1e-6,"output":0.5},
            "huge":{"input":1e308,"output":1e308},
            "tier-at-zero":{"input":1e-6,"output":1e-6,
                "long_context":{"above_prompt_tokens":0,"rates":{"input":2e-6,"output":2e-6}}},
            "tier-below-floor":{"input":1e-6,"output":1e-6,
                "long_context":{"above_prompt_tokens":999,"rates":{"input":2e-6,"output":2e-6}}},
            "tier-negative":{"input":1e-6,"output":1e-6,
                "long_context":{"above_prompt_tokens":200000,"rates":{"input":-1.0,"output":0.0}}},
            "tier-at-floor":{"input":1e-6,"output":1e-6,
                "long_context":{"above_prompt_tokens":1000,"rates":{"input":2e-6,"output":2e-6}}}
        }}"#;
        let snapshot = parse_snapshot_json(raw).expect("in-bounds models remain");
        let mut kept: Vec<&str> = snapshot.models.keys().map(String::as_str).collect();
        kept.sort_unstable();
        assert_eq!(
            kept,
            [
                "at-ceiling",
                "kept",
                "tier-at-floor",
                "tier-at-zero",
                "tier-below-floor",
                "tier-negative"
            ]
        );
        for tierless in ["tier-at-zero", "tier-below-floor", "tier-negative"] {
            assert!(
                snapshot.models[tierless].long_context.is_none(),
                "{tierless}"
            );
        }
        let tier = snapshot.models["tier-at-floor"]
            .long_context
            .expect("a threshold at the floor stays");
        assert_eq!(tier.above_prompt_tokens, MIN_TIER_PROMPT_TOKENS);
    }

    /// Fields a stored model does not use are skipped unbuilt, and a stored
    /// table over the entry bound is no snapshot at all.
    #[test]
    fn a_stored_snapshot_is_read_within_the_entry_bound() {
        let padding = vec![serde_json::json!({ "": 0 }); 10_000];
        let padded = serde_json::json!({
            "_fetched": "2026-10-26",
            "_format": 2,
            "_padding": padding,
            "models": {"m": {"input": 1e-6, "output": 0.0, "padding": padding}}
        });
        let snapshot = parse_snapshot_json(&padded.to_string()).expect("padding is skipped");
        assert_eq!(snapshot.models.len(), 1);

        let models: Vec<String> = (0..=MAX_TABLE_ENTRIES)
            .map(|index| format!(r#""m{index}":{{"input":1e-6,"output":0.0}}"#))
            .collect();
        let over = format!(
            r#"{{"_fetched":"2026-10-26","models":{{{}}}}}"#,
            models.join(",")
        );
        assert!(parse_snapshot_json(&over).is_none());
    }

    /// Nothing left after the bounds counts as no snapshot, so it is
    /// refetched rather than pricing nothing.
    #[test]
    fn a_stored_snapshot_with_no_rate_in_bounds_is_absent() {
        let raw = r#"{"_fetched":"2026-10-26","_format":2,"models":{
            "negative":{"input":-5e-6,"output":0.0}
        }}"#;
        assert!(parse_snapshot_json(raw).is_none());
    }
}
