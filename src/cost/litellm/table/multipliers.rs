//! Read the multipliers an entry's `provider_specific_entry` lists for
//! Anthropic's fast mode and US-only inference. Other providers keep strings
//! and nested objects under the same key; those are skipped unbuilt.
use std::fmt;

use serde::de::{Deserialize, Deserializer, Error, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::Scalar;

/// As listed, in bounds or not; anything but a number counts as unlisted.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(in crate::cost) struct Multipliers {
    pub(in crate::cost) fast: Option<f64>,
    pub(in crate::cost) us: Option<f64>,
}

impl<'de> Deserialize<'de> for Multipliers {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(MultipliersVisitor)
    }
}

struct MultipliersVisitor;

impl<'de> Visitor<'de> for MultipliersVisitor {
    type Value = Multipliers;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_u64<E: Error>(self, _: u64) -> Result<Multipliers, E> {
        Ok(Multipliers::default())
    }

    fn visit_i64<E: Error>(self, _: i64) -> Result<Multipliers, E> {
        Ok(Multipliers::default())
    }

    fn visit_f64<E: Error>(self, _: f64) -> Result<Multipliers, E> {
        Ok(Multipliers::default())
    }

    fn visit_str<E: Error>(self, _: &str) -> Result<Multipliers, E> {
        Ok(Multipliers::default())
    }

    fn visit_bool<E: Error>(self, _: bool) -> Result<Multipliers, E> {
        Ok(Multipliers::default())
    }

    fn visit_unit<E: Error>(self) -> Result<Multipliers, E> {
        Ok(Multipliers::default())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Multipliers, A::Error> {
        IgnoredAny.visit_seq(seq).map(|_| Multipliers::default())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Multipliers, A::Error> {
        let mut multipliers = Multipliers::default();
        while let Some(key) = map.next_key::<String>()? {
            let slot = match key.as_str() {
                "fast" => &mut multipliers.fast,
                "us" => &mut multipliers.us,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
            };
            *slot = match map.next_value::<Scalar>()? {
                Scalar::Number(multiplier) => Some(multiplier),
                Scalar::ChatMode | Scalar::Other => None,
            };
        }
        Ok(multipliers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &str) -> Multipliers {
        serde_json::from_str(raw).expect("any JSON value reads")
    }

    #[test]
    fn numeric_fast_and_us_are_read_and_the_rest_skipped() {
        assert_eq!(
            read(r#"{"us": 1.1, "fast": 2, "realtime_ga_only": 1}"#),
            Multipliers {
                fast: Some(2.0),
                us: Some(1.1)
            }
        );
        assert_eq!(
            read(r#"{"us": "1.1", "fast": {"x": 2}, "volcengine": {"480p": {"1:1": "640x640"}}}"#),
            Multipliers::default()
        );
        for other in [r#""first_frame""#, "5", "null", "[1, 2]", "true"] {
            assert_eq!(read(other), Multipliers::default(), "{other}");
        }
    }
}
