//! Read `LiteLLM`'s table straight into the few fields pricing uses. The
//! table is fetched from a repository this tool does not control, so
//! nothing else in it is built in memory: a field, an entry or a whole value
//! that pricing does not read is skipped while it is parsed.
use std::fmt;

use serde::de::{Deserialize, Deserializer, Error, IgnoredAny, MapAccess, SeqAccess, Visitor};

use crate::cost::validate::{MAX_TABLE_ENTRIES, MIN_TIER_PROMPT_TOKENS};

pub(in crate::cost) const INPUT: usize = 0;
pub(in crate::cost) const OUTPUT: usize = 1;
pub(in crate::cost) const CACHE_READ: usize = 2;
pub(in crate::cost) const WRITE_5M: usize = 3;
pub(in crate::cost) const WRITE_1H: usize = 4;

/// Longest name first: the 1h write field extends the 5m one.
const RATE_FIELDS: [(&str, usize); 5] = [
    ("cache_creation_input_token_cost_above_1hr", WRITE_1H),
    ("cache_creation_input_token_cost", WRITE_5M),
    ("cache_read_input_token_cost", CACHE_READ),
    ("output_cost_per_token", OUTPUT),
    ("input_cost_per_token", INPUT),
];

/// Well over the thresholds any model lists; past it the entry's tiers are
/// dropped rather than kept growing.
const MAX_TIERS_PER_ENTRY: usize = 4;

/// Each slot holds the listed number, in bounds or not; a value that is not
/// a number counts as unlisted.
pub(in crate::cost) type Rates = [Option<f64>; 5];

pub(in crate::cost) struct Tier {
    pub(in crate::cost) label: String,
    pub(in crate::cost) above_prompt_tokens: u64,
    /// Only a label with an input key is a threshold, whatever that key's
    /// value.
    pub(in crate::cost) has_input: bool,
    pub(in crate::cost) rates: Rates,
}

#[derive(Default)]
pub(in crate::cost) struct Entry {
    pub(in crate::cost) chat: bool,
    pub(in crate::cost) rates: Rates,
    pub(in crate::cost) tiers: Vec<Tier>,
    pub(in crate::cost) too_many_tiers: bool,
}

/// Chat-like entries under bare model ids, in table order.
pub(in crate::cost) struct Table(pub(in crate::cost) Vec<(String, Entry)>);

/// Provider/region variants are dropped in favor of bare model ids, EXCEPT
/// `xai/`: `LiteLLM` registers Grok models only under the provider prefix
/// (`xai/grok-4.5`, no bare key), so the prefix is stripped so those models
/// can resolve to a price.
fn bare_id(key: &str) -> Option<&str> {
    let key = key.strip_prefix("xai/").unwrap_or(key);
    let variant = key.contains('/')
        || ["anthropic.", "global.", "us.", "eu.", "au.", "apac."]
            .iter()
            .any(|prefix| key.starts_with(prefix));
    (!variant).then_some(key)
}

enum Slot<'k> {
    Base(usize),
    Tier {
        slot: usize,
        label: &'k str,
        above_prompt_tokens: u64,
    },
}

fn slot(key: &str) -> Option<Slot<'_>> {
    for (field, slot) in RATE_FIELDS {
        if key == field {
            return Some(Slot::Base(slot));
        }
        let Some(label) = key
            .strip_prefix(field)
            .and_then(|rest| rest.strip_prefix("_above_"))
            .and_then(|rest| rest.strip_suffix("_tokens"))
        else {
            continue;
        };
        let above_prompt_tokens = match label.strip_suffix('k') {
            Some(thousands) => thousands
                .parse::<u64>()
                .ok()
                .and_then(|tokens| tokens.checked_mul(1_000)),
            None => label.parse::<u64>().ok(),
        };
        if let Some(above_prompt_tokens) = above_prompt_tokens {
            return (above_prompt_tokens >= MIN_TIER_PROMPT_TOKENS).then_some(Slot::Tier {
                slot,
                label,
                above_prompt_tokens,
            });
        }
    }
    None
}

impl Entry {
    fn set_tier(&mut self, slot: usize, label: &str, above_prompt_tokens: u64, rate: Option<f64>) {
        let index = match self.tiers.iter().position(|tier| tier.label == label) {
            Some(index) => index,
            None if self.tiers.len() < MAX_TIERS_PER_ENTRY => {
                self.tiers.push(Tier {
                    label: label.to_owned(),
                    above_prompt_tokens,
                    has_input: false,
                    rates: [None; 5],
                });
                self.tiers.len() - 1
            }
            None => {
                self.too_many_tiers = true;
                return;
            }
        };
        let tier = &mut self.tiers[index];
        tier.has_input |= slot == INPUT;
        tier.rates[slot] = rate;
    }
}

/// A field value read as `serde_json::Value::as_f64` / `as_str` would read
/// it, without building anything else.
enum Scalar {
    Number(f64),
    ChatMode,
    Other,
}

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ScalarVisitor)
    }
}

struct ScalarVisitor;

impl<'de> Visitor<'de> for ScalarVisitor {
    type Value = Scalar;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "Same conversion as `serde_json::Value::as_f64`."
    )]
    fn visit_u64<E: Error>(self, value: u64) -> Result<Scalar, E> {
        Ok(Scalar::Number(value as f64))
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "Same conversion as `serde_json::Value::as_f64`."
    )]
    fn visit_i64<E: Error>(self, value: i64) -> Result<Scalar, E> {
        Ok(Scalar::Number(value as f64))
    }

    fn visit_f64<E: Error>(self, value: f64) -> Result<Scalar, E> {
        Ok(Scalar::Number(value))
    }

    fn visit_str<E: Error>(self, value: &str) -> Result<Scalar, E> {
        Ok(match value {
            "chat" | "completion" | "responses" => Scalar::ChatMode,
            _ => Scalar::Other,
        })
    }

    fn visit_bool<E: Error>(self, _: bool) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }

    fn visit_unit<E: Error>(self) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Scalar, A::Error> {
        IgnoredAny.visit_seq(seq).map(|_| Scalar::Other)
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Scalar, A::Error> {
        IgnoredAny.visit_map(map).map(|_| Scalar::Other)
    }
}

/// An entry, or `None` for a value that is not an object.
struct MaybeEntry(Option<Entry>);

impl<'de> Deserialize<'de> for MaybeEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(EntryVisitor).map(MaybeEntry)
    }
}

struct EntryVisitor;

impl<'de> Visitor<'de> for EntryVisitor {
    type Value = Option<Entry>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_u64<E: Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E: Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E: Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_str<E: Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_bool<E: Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        IgnoredAny.visit_seq(seq).map(|_| None)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entry = Entry::default();
        while let Some(key) = map.next_key::<String>()? {
            if key == "mode" {
                entry.chat = matches!(map.next_value::<Scalar>()?, Scalar::ChatMode);
                continue;
            }
            let Some(slot) = slot(&key) else {
                map.next_value::<IgnoredAny>()?;
                continue;
            };
            let rate = match map.next_value::<Scalar>()? {
                Scalar::Number(rate) => Some(rate),
                Scalar::ChatMode | Scalar::Other => None,
            };
            match slot {
                Slot::Base(slot) => entry.rates[slot] = rate,
                Slot::Tier {
                    slot,
                    label,
                    above_prompt_tokens,
                } => entry.set_tier(slot, label, above_prompt_tokens, rate),
            }
        }
        Ok(Some(entry))
    }
}

impl<'de> Deserialize<'de> for Table {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(TableVisitor)
    }
}

struct TableVisitor;

impl<'de> Visitor<'de> for TableVisitor {
    type Value = Table;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(
            formatter,
            "an object of at most {MAX_TABLE_ENTRIES} entries"
        )
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Table, A::Error> {
        let mut entries = Vec::new();
        let mut seen = 0_usize;
        while let Some(key) = map.next_key::<String>()? {
            seen += 1;
            if seen > MAX_TABLE_ENTRIES {
                return Err(A::Error::custom(format_args!(
                    "more than {MAX_TABLE_ENTRIES} entries"
                )));
            }
            let Some(id) = bare_id(&key) else {
                map.next_value::<IgnoredAny>()?;
                continue;
            };
            if let MaybeEntry(Some(entry)) = map.next_value()?
                && entry.chat
            {
                entries.push((id.to_owned(), entry));
            }
        }
        Ok(Table(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &str) -> Result<Table, serde_json::Error> {
        serde_json::from_str(raw)
    }

    #[test]
    fn a_table_over_the_entry_bound_is_refused() {
        let table = |entries: usize| {
            let body: Vec<String> = (0..entries)
                .map(|index| format!(r#""k{index}":0"#))
                .collect();
            format!("{{{}}}", body.join(","))
        };
        assert!(read(&table(MAX_TABLE_ENTRIES)).is_ok());
        let refused = read(&table(MAX_TABLE_ENTRIES + 1))
            .err()
            .expect("over the bound");
        assert!(refused.to_string().contains("entries"), "{refused}");
    }

    /// Padding a hostile table could carry is consumed without being kept:
    /// only the chat entry under a bare id comes back.
    #[test]
    fn padding_and_non_object_values_are_skipped() {
        let padding = vec![serde_json::json!({ "": 0 }); 10_000];
        let raw = serde_json::json!({
            "padding": padding,
            "a-number": 5,
            "a-string": "x",
            "null": null,
            "embedding-model": {"mode": "embedding", "input_cost_per_token": 1e-7},
            "openai/gpt-x": {"mode": "chat", "input_cost_per_token": 1e-6},
            "gpt-x": {
                "mode": "chat",
                "input_cost_per_token": 1e-6,
                "notes": padding,
                "output_cost_per_token": "free",
                "cache_read_input_token_cost": null,
                "supports_vision": true
            }
        })
        .to_string();
        let Table(entries) = read(&raw).expect("table parses");
        assert_eq!(entries.len(), 1);
        let (id, entry) = &entries[0];
        assert_eq!(id, "gpt-x");
        assert_eq!(entry.rates, [Some(1e-6), None, None, None, None]);
    }

    #[test]
    fn rate_and_tier_keys_land_in_their_slots() {
        let Table(entries) = read(
            r#"{"xai/grok-x": {
                "mode": "chat",
                "input_cost_per_token": 1,
                "cache_creation_input_token_cost_above_1hr": 2e-6,
                "cache_creation_input_token_cost_above_1hr_above_200k_tokens": 4e-6,
                "input_cost_per_token_above_200k_tokens": "n/a",
                "output_cost_per_token_above_200k_tokens": 3e-6,
                "input_cost_per_token_above_200k_tokens_batches": 1e-6,
                "input_cost_per_token_above_999_tokens": 1e-6
            }}"#,
        )
        .expect("table parses");
        let (id, entry) = &entries[0];
        assert_eq!(id, "grok-x");
        assert_eq!(entry.rates, [Some(1.0), None, None, None, Some(2e-6)]);
        assert_eq!(entry.tiers.len(), 1);
        let tier = &entry.tiers[0];
        assert_eq!(
            (tier.label.as_str(), tier.above_prompt_tokens),
            ("200k", 200_000)
        );
        assert!(tier.has_input);
        assert_eq!(tier.rates, [None, Some(3e-6), None, None, Some(4e-6)]);
    }

    #[test]
    fn tiers_past_the_cap_are_marked_not_kept() {
        let mut entry = serde_json::Map::new();
        entry.insert("mode".into(), "chat".into());
        for thousands in 1..=MAX_TIERS_PER_ENTRY + 1 {
            entry.insert(
                format!("input_cost_per_token_above_{thousands}k_tokens"),
                1e-6.into(),
            );
        }
        let raw = serde_json::json!({ "m": entry }).to_string();
        let Table(entries) = read(&raw).expect("table parses");
        assert_eq!(entries[0].1.tiers.len(), MAX_TIERS_PER_ENTRY);
        assert!(entries[0].1.too_many_tiers);
    }
}
