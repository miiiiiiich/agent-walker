//! Pricing comes from `LiteLLM`'s community pricing database, so rate changes
//! are tracked upstream without a release.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::model::TokenUsage;

mod remote;

pub use remote::spawn_pricing_refresh;

static LOADED: OnceLock<RwLock<Option<Snapshot>>> = OnceLock::new();

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

pub fn pricing_as_of() -> Option<String> {
    loaded().read().ok()?.as_ref()?.fetched.clone()
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
/// `claude-sonnet-4-5`) or the `-latest` alias (`claude-sonnet-4-5-latest`). A
/// plain word suffix (`gemini-pro-default`) must not match a shorter base
/// key. Retry dotted version segments as dashed only as a fallback, so ids whose pricing
/// keys genuinely contain dots (`gpt-3.5-turbo`) resolve exactly first.
pub fn pricing_for(model_name: &str) -> Option<Pricing> {
    let mut name = normalize(model_name);
    if name == "codex" || name == "openai" || name == "codex-auto-review" {
        "gpt-5.5".clone_into(&mut name);
    }

    let loaded = loaded().read().ok()?;
    let snapshot = loaded.as_ref()?;
    if let Some(pricing) = lookup(snapshot, &name) {
        return Some(pricing);
    }
    let dashed = name.replace('.', "-");
    if dashed != name {
        return lookup(snapshot, &dashed);
    }
    None
}

fn lookup(snapshot: &Snapshot, name: &str) -> Option<Pricing> {
    if let Some(pricing) = snapshot.models.get(name) {
        return Some(*pricing);
    }
    snapshot
        .models
        .iter()
        .filter(|(key, _)| {
            name.strip_prefix(key.as_str())
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|suffix| {
                    suffix == "latest" || suffix.starts_with(|c: char| c.is_ascii_digit())
                })
        })
        .max_by_key(|(key, _)| key.len())
        .map(|(_, pricing)| *pricing)
}

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

/// Track unpriced volume separately so incomplete pricing cannot be reported
/// as a complete zero-dollar total.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostTally {
    pub priced_usd: f64,
    pub unpriced_volume: u64,
}

impl CostTally {
    pub fn add(
        &mut self,
        model_name: &str,
        unreported: &TokenUsage,
        reported_cost_usd: Option<f64>,
    ) {
        self.priced_usd += reported_cost_usd.unwrap_or(0.0);
        let volume = unreported.token_volume();
        if volume == 0 {
            return;
        }
        match usage_cost_usd(model_name, unreported) {
            Some(cost) => self.priced_usd += cost,
            None => self.unpriced_volume = self.unpriced_volume.saturating_add(volume),
        }
    }

    pub fn is_complete(&self) -> bool {
        self.unpriced_volume == 0
    }

    /// The total, only when it is the whole picture — a partial sum is not
    /// a total.
    pub fn complete_usd(&self) -> Option<f64> {
        self.is_complete().then_some(self.priced_usd)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn grok_models_resolve_from_bare_keys() {
        install_test_pricing();
        let pricing = pricing_for("grok-4.5").expect("grok model should resolve");
        assert!((pricing.input - 0.2 / 1e6).abs() < f64::EPSILON);
    }

    #[test]
    fn dotted_model_names_resolve_to_dashed_pricing_keys() {
        install_test_pricing();
        let dotted = pricing_for("claude-sonnet-4.5").expect("dotted name should resolve");
        let dashed = pricing_for("claude-sonnet-4-5").expect("dashed name should resolve");
        assert!((dotted.input - dashed.input).abs() < f64::EPSILON);
        let real_dot = pricing_for("gpt-3.5-turbo").expect("dotted key should resolve");
        assert!((real_dot.input - 0.5 / 1e6).abs() < f64::EPSILON);
    }

    pub(crate) fn install_test_pricing() {
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
                ("claude-fable-5".to_owned(), per_mtok(8.0, 40.0, 10.0, 16.0)),
                (
                    "claude-sonnet-4-5".to_owned(),
                    per_mtok(3.0, 15.0, 3.75, 6.0),
                ),
                ("gpt-3.5-turbo".to_owned(), per_mtok(0.5, 1.5, 0.0, 0.0)),
                ("grok-4.5".to_owned(), per_mtok(0.2, 1.5, 0.0, 0.0)),
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
        assert!(usage_cost_usd("claude-sonnet-4-5-experimental", &usage).is_none());
    }

    #[test]
    fn prices_latest_alias_via_prefix() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let cost = usage_cost_usd("claude-sonnet-4-5-latest", &usage)
            .expect("-latest alias should price off the base key");
        assert!((cost - 3.0).abs() < 1e-9);
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
    fn prices_current_model_ids() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let fable = usage_cost_usd("claude-fable-5", &usage).expect("fable should be priced");
        assert!((fable - 8.0).abs() < 1e-9);
        let review =
            usage_cost_usd("codex-auto-review", &usage).expect("auto-review should be priced");
        assert!((review - 5.0).abs() < 1e-9);
    }

    #[test]
    fn prices_antigravity_display_name() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let cost = usage_cost_usd("Gemini 3.5 Flash (High)", &usage)
            .expect("gemini display name should price");
        assert!((cost - 10.5).abs() < 1e-9);
        assert!(usage_cost_usd("gemini-pro-default", &usage).is_none());
    }

    /// An unpriced model's volume never lands in the sum as $0: the tally
    /// records the gap and refuses to call the partial sum a total.
    #[test]
    fn tally_keeps_unpriced_volume_out_of_the_sum() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };

        let mut priced = CostTally::default();
        priced.add("claude-opus-4-8", &usage, None);
        assert!(priced.is_complete());
        assert!((priced.complete_usd().unwrap_or(0.0) - 5.0).abs() < 1e-9);

        let mut mixed = CostTally::default();
        mixed.add("claude-opus-4-8", &usage, None);
        mixed.add("model-nobody-priced", &usage, Some(0.25));
        assert!((mixed.priced_usd - 5.25).abs() < 1e-9);
        assert_eq!(mixed.unpriced_volume, 1_000_000);
        assert_eq!(mixed.complete_usd(), None);

        let mut empty = CostTally::default();
        empty.add("model-nobody-priced", &TokenUsage::default(), None);
        assert!(empty.is_complete());
    }
}
