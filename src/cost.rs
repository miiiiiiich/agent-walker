//! Pricing comes from `LiteLLM`'s community pricing database, so rate changes
//! are tracked upstream without a release.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::model::{CostTally, TokenUsage, UsageGranularity};

mod litellm;
mod remote;
mod validate;

pub use remote::spawn_pricing_refresh;

static LOADED: OnceLock<RwLock<Option<Snapshot>>> = OnceLock::new();

/// Stored snapshots carry the version of the rules that derived their rates;
/// bump it whenever those rules change. A file from an older build still
/// parses, and trusted for the rest of the day it would misprice until the
/// next day's fetch, so a mismatch refetches and keeps the file only as the
/// offline fallback.
const SNAPSHOT_FORMAT: u32 = 2;

/// Codex logs only the provider (`openai`, `codex`) for some sessions, and
/// `codex-auto-review` has no `LiteLLM` key; all of them price as the model
/// Codex runs by default, so this follows Codex's default.
const CODEX_DEFAULT_MODEL: &str = "gpt-6.1-sol";

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(from = "StoredModel")]
struct ModelPricing {
    #[serde(flatten)]
    base: Pricing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    long_context: Option<LongContext>,
}

/// A stored model as written, base rates inline. Read through this rather
/// than `flatten`, which buffers every field of the entry, unknown ones
/// included, before sorting them out.
#[derive(Deserialize)]
struct StoredModel {
    input: f64,
    output: f64,
    #[serde(default)]
    cache_read: f64,
    #[serde(default)]
    cache_write_5m: f64,
    #[serde(default)]
    cache_write_1h: f64,
    #[serde(default)]
    long_context: Option<LongContext>,
}

impl From<StoredModel> for ModelPricing {
    fn from(stored: StoredModel) -> Self {
        Self {
            base: Pricing {
                input: stored.input,
                output: stored.output,
                cache_read: stored.cache_read,
                cache_write_5m: stored.cache_write_5m,
                cache_write_1h: stored.cache_write_1h,
            },
            long_context: stored.long_context,
        }
    }
}

/// Rates for every token of a request whose prompt is over
/// `above_prompt_tokens`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct LongContext {
    above_prompt_tokens: u64,
    rates: Pricing,
}

impl ModelPricing {
    fn rates_for(&self, usage: &TokenUsage) -> Pricing {
        match self.long_context {
            Some(tier) if prompt_tokens(usage) > tier.above_prompt_tokens => tier.rates,
            _ => self.base,
        }
    }
}

/// The threshold measures the whole prompt. `input_tokens` is fresh input
/// only, and counting just that would leave a mostly-cached long call at
/// base rates.
fn prompt_tokens(usage: &TokenUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens)
}

#[derive(Deserialize)]
struct Snapshot {
    #[serde(rename = "_fetched", default)]
    fetched: Option<String>,
    #[serde(rename = "_format", default)]
    format: u32,
    #[serde(deserialize_with = "validate::bounded_map")]
    models: HashMap<String, ModelPricing>,
    /// Model name as logged -> its pricing, or `None` when nothing matches.
    #[serde(skip)]
    resolved: RwLock<HashMap<String, Option<ModelPricing>>>,
}

fn loaded() -> &'static RwLock<Option<Snapshot>> {
    LOADED.get_or_init(|| RwLock::new(None))
}

/// The fetched table and the stored snapshot both come through here, so a
/// file on disk meets the same bounds as a fresh fetch.
fn parse_snapshot_json(raw: &str) -> Option<Snapshot> {
    let mut snapshot = serde_json::from_str::<Snapshot>(raw).ok()?;
    validate::retain_in_bounds(&mut snapshot.models);
    // The stamp is printed in the COST panel; anything but a date would
    // reach the terminal as is. Without one the snapshot is refetched.
    snapshot.fetched = snapshot.fetched.filter(|stamp| {
        time::Date::parse(
            stamp,
            time::macros::format_description!("[year]-[month]-[day]"),
        )
        .is_ok()
    });
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

/// Base rates, the ones every request pays below any long-context threshold.
pub fn pricing_for(model_name: &str) -> Option<Pricing> {
    model_pricing(model_name).map(|pricing| pricing.base)
}

fn model_pricing(model_name: &str) -> Option<ModelPricing> {
    loaded().read().ok()?.as_ref()?.resolve(model_name)
}

impl Snapshot {
    /// Every usage event looks its model up, and a name with no exact key
    /// scans the whole table; each distinct name is resolved once per
    /// snapshot instead.
    fn resolve(&self, model_name: &str) -> Option<ModelPricing> {
        if let Some(resolved) = self
            .resolved
            .read()
            .ok()
            .and_then(|memo| memo.get(model_name).copied())
        {
            return resolved;
        }
        let resolved = self.match_name(model_name);
        if let Ok(mut memo) = self.resolved.write() {
            memo.insert(model_name.to_owned(), resolved);
        }
        resolved
    }

    /// Exact snapshot match first, then the longest snapshot key the name
    /// extends with a date/version suffix (`claude-sonnet-4-5-20250929` ->
    /// `claude-sonnet-4-5`) or the `-latest` alias (`claude-sonnet-4-5-latest`).
    /// A plain word suffix (`gemini-pro-default`) must not match a shorter
    /// base key. Retry dotted version segments as dashed only as a fallback,
    /// so ids whose pricing keys genuinely contain dots (`gpt-3.5-turbo`)
    /// resolve exactly first.
    fn match_name(&self, model_name: &str) -> Option<ModelPricing> {
        let mut name = normalize(model_name);
        if name == "codex" || name == "openai" || name == "codex-auto-review" {
            CODEX_DEFAULT_MODEL.clone_into(&mut name);
        }
        if let Some(pricing) = lookup(self, &name) {
            return Some(pricing);
        }
        let dashed = name.replace('.', "-");
        if dashed != name {
            return lookup(self, &dashed);
        }
        None
    }
}

fn lookup(snapshot: &Snapshot, name: &str) -> Option<ModelPricing> {
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

/// Price `usage` as one request, so a prompt over the model's long-context
/// threshold puts the whole request on the long-context rates.
pub fn usage_cost_usd(model_name: &str, usage: &TokenUsage) -> Option<f64> {
    let pricing = model_pricing(model_name)?;
    Some(cost_at(&pricing.rates_for(usage), usage))
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Token counts are far below 2^52; estimate precision is dominated by pricing anyway."
)]
fn cost_at(rates: &Pricing, usage: &TokenUsage) -> f64 {
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

    usage.input_tokens as f64 * rates.input
        + usage.output_tokens as f64 * rates.output
        + usage.cache_read_input_tokens as f64 * rates.cache_read
        + write_short as f64 * rates.cache_write_5m
        + write_long as f64 * rates.cache_write_1h
}

/// Price one usage event. Only a `Call` can cross a long-context threshold:
/// an aggregate stays at base rates rather than billing every request in it
/// at the premium because their sum is large.
pub fn estimate(model_name: &str, usage: &TokenUsage, granularity: UsageGranularity) -> CostTally {
    let volume = usage.token_volume();
    if volume == 0 {
        return CostTally::default();
    }
    let cost = match granularity {
        UsageGranularity::Call => usage_cost_usd(model_name, usage),
        UsageGranularity::Aggregate => pricing_for(model_name).map(|base| cost_at(&base, usage)),
    };
    match cost {
        Some(priced_usd) => CostTally {
            priced_usd,
            unpriced_volume: 0,
        },
        None => CostTally {
            priced_usd: 0.0,
            unpriced_volume: volume,
        },
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
        let flat = |base: Pricing| ModelPricing {
            base,
            long_context: None,
        };
        let openai = |input: f64, output: f64, cache_read: f64| Pricing {
            input: input / 1e6,
            output: output / 1e6,
            cache_read: cache_read / 1e6,
            cache_write_5m: 0.0,
            cache_write_1h: 0.0,
        };
        replace_loaded(Some(Snapshot {
            fetched: Some("test".to_owned()),
            format: SNAPSHOT_FORMAT,
            resolved: RwLock::default(),
            models: HashMap::from([
                (
                    "claude-opus-4-8".to_owned(),
                    flat(per_mtok(5.0, 25.0, 6.25, 10.0)),
                ),
                (
                    "claude-fable-5".to_owned(),
                    flat(per_mtok(8.0, 40.0, 10.0, 16.0)),
                ),
                (
                    "claude-sonnet-4-5".to_owned(),
                    flat(per_mtok(3.0, 15.0, 3.75, 6.0)),
                ),
                (
                    "claude-haiku-5-5".to_owned(),
                    ModelPricing {
                        base: per_mtok(0.1, 0.5, 0.125, 0.2),
                        long_context: Some(LongContext {
                            above_prompt_tokens: 100_000,
                            rates: per_mtok(0.5, 2.5, 0.625, 1.0),
                        }),
                    },
                ),
                (
                    "gpt-3.5-turbo".to_owned(),
                    flat(per_mtok(0.5, 1.5, 0.0, 0.0)),
                ),
                ("grok-4.5".to_owned(), flat(per_mtok(0.2, 1.5, 0.0, 0.0))),
                ("gpt-5.5".to_owned(), flat(openai(5.0, 30.0, 0.5))),
                (
                    "gpt-6.1-sol".to_owned(),
                    ModelPricing {
                        base: openai(2.0, 10.0, 0.1),
                        long_context: Some(LongContext {
                            above_prompt_tokens: 272_000,
                            rates: openai(4.0, 15.0, 0.2),
                        }),
                    },
                ),
                ("gemini-3.5-flash".to_owned(), flat(openai(1.5, 9.0, 0.15))),
                (
                    "tiny-input".to_owned(),
                    flat(Pricing {
                        input: 1e-300,
                        output: 1e-6,
                        cache_read: 1e-2,
                        cache_write_5m: 1e-2,
                        cache_write_1h: 1e-2,
                    }),
                ),
            ]),
        }));
    }

    /// A hit is answered from the memo, not the table: clearing the table
    /// afterwards changes nothing for names already resolved.
    #[test]
    fn each_model_name_is_resolved_once_per_snapshot() {
        let mut snapshot = Snapshot {
            fetched: None,
            format: SNAPSHOT_FORMAT,
            models: HashMap::from([(
                "claude-sonnet-4-5".to_owned(),
                ModelPricing {
                    base: Pricing {
                        input: 3e-6,
                        output: 1.5e-5,
                        cache_read: 3e-7,
                        cache_write_5m: 3.75e-6,
                        cache_write_1h: 6e-6,
                    },
                    long_context: None,
                },
            )]),
            resolved: RwLock::default(),
        };
        let dated = "claude-sonnet-4-5-20250929";
        assert!(snapshot.resolve(dated).is_some());
        assert!(snapshot.resolve("unknown-model").is_none());
        snapshot.models.clear();
        assert!(snapshot.resolve(dated).is_some());
        assert!(snapshot.resolve("unknown-model").is_none());
        assert_eq!(snapshot.resolved.read().unwrap().len(), 2);
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
    }

    #[test]
    fn provider_only_names_price_as_the_codex_default() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 100_000,
            ..TokenUsage::default()
        };
        for name in ["codex", "openai", "codex-auto-review"] {
            let cost = usage_cost_usd(name, &usage).expect("provider-only name should price");
            assert!((cost - 0.2).abs() < 1e-9, "{name}: {cost}");
        }
    }

    /// Cache reads and writes count toward the threshold, and every rate of
    /// the request switches, the 1h write included.
    #[test]
    fn long_context_rates_cover_the_whole_request_over_the_threshold() {
        install_test_pricing();
        let at_threshold = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 1_000,
            cache_read_input_tokens: 99_000,
            ..TokenUsage::default()
        };
        let base = usage_cost_usd("claude-haiku-5-5", &at_threshold).expect("haiku prices");
        assert!((base - (1_000.0 * 0.1 + 1_000.0 * 0.5 + 99_000.0 * 0.01) / 1e6).abs() < 1e-12);

        let over = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 1_000,
            cache_read_input_tokens: 90_000,
            cache_creation_input_tokens: 10_000,
            cache_creation_ephemeral_5m_input_tokens: 4_000,
            cache_creation_ephemeral_1h_input_tokens: 6_000,
            ..TokenUsage::default()
        };
        let long = usage_cost_usd("claude-haiku-5-5", &over).expect("haiku prices");
        let expected =
            (1_000.0 * 0.5 + 1_000.0 * 2.5 + 90_000.0 * 0.05 + 4_000.0 * 0.625 + 6_000.0 * 1.0)
                / 1e6;
        assert!((long - expected).abs() < 1e-12, "{long} vs {expected}");
    }

    #[test]
    fn openai_long_context_counts_cached_input() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 2_001,
            output_tokens: 1_000,
            cache_read_input_tokens: 270_000,
            ..TokenUsage::default()
        };
        let cost = usage_cost_usd("gpt-6.1-sol", &usage).expect("sol prices");
        let expected = (2_001.0 * 4.0 + 1_000.0 * 15.0 + 270_000.0 * 0.2) / 1e6;
        assert!((cost - expected).abs() < 1e-12, "{cost} vs {expected}");
    }

    /// An event that sums several requests stays at base rates even when the
    /// sum is past the threshold; a single request of the same size does not.
    #[test]
    fn aggregate_events_never_cross_a_long_context_threshold() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let aggregate = estimate("claude-haiku-5-5", &usage, UsageGranularity::Aggregate);
        assert!((aggregate.priced_usd - 0.1).abs() < 1e-12);
        let call = estimate("claude-haiku-5-5", &usage, UsageGranularity::Call);
        assert!((call.priced_usd - 0.5).abs() < 1e-12);
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
    /// records the gap and the sum leaves the model out.
    #[test]
    fn tally_keeps_unpriced_volume_out_of_the_sum() {
        install_test_pricing();
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            ..TokenUsage::default()
        };

        let priced = estimate("claude-opus-4-8", &usage, UsageGranularity::Call);
        assert!(priced.is_complete());
        assert!((priced.usd().unwrap_or(0.0) - 5.0).abs() < 1e-9);

        let unpriced = estimate("model-nobody-priced", &usage, UsageGranularity::Call);
        let mut mixed = CostTally::default();
        mixed.add(&priced);
        mixed.add(&unpriced);
        assert!((mixed.priced_usd - 5.0).abs() < 1e-9);
        assert_eq!(mixed.unpriced_volume, 1_000_000);
        assert!((mixed.usd().unwrap_or(0.0) - 5.0).abs() < 1e-9);
        assert_eq!(
            unpriced.usd(),
            None,
            "nothing priced reads as unknown, not $0"
        );

        let empty = estimate(
            "model-nobody-priced",
            &TokenUsage::default(),
            UsageGranularity::Call,
        );
        assert!(empty.is_complete());
    }
}
