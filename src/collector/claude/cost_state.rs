//! Claude Code's `cost-state` rows: each process's running total per model.
//! It also counts calls that never get an `assistant` row of their own, so
//! the part over a session's transcript is usage the transcript misses.

mod fork;
mod spread;
#[cfg(test)]
mod tests;
mod transcript;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use crate::collector::FileEvents;
use crate::collector::ident::ident;
use crate::model::TokenUsage;

use super::{session_id_field, u64_field};

pub(in crate::collector::claude) use spread::uncounted_calls;

/// Each model of a total is spread over every hour its session ran: a row
/// naming thousands of models, which the line limits allow, would expand
/// into millions of rows. Only Claude Code writes these rows, so this is a
/// cheap bound, not a defense against a crafted store.
const MAX_MODELS: usize = 64;

/// A main transcript's `cost-state` rows, in file order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostStateLog {
    rows: Vec<CostStateRow>,
    #[serde(skip)]
    latest: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CostStateRow {
    session_id: String,
    start_time: Option<i64>,
    /// The row is undated: these are the latest stamp of the rows before it
    /// and the stamp of the first dated row after it.
    before: Option<OffsetDateTime>,
    after: Option<OffsetDateTime>,
    /// A usage row follows it: the process that wrote those rows recorded no
    /// total of its own.
    stale: bool,
    models: BTreeMap<String, TokenUsage>,
}

impl CostStateRow {
    fn volume(&self) -> u64 {
        self.models
            .values()
            .fold(0, |sum, usage| sum.saturating_add(usage.token_volume()))
    }

    /// Whether a row stamped `at` was written before this total. A row may be
    /// stamped later than one written after it, so everything up to the latest
    /// stamp before the total counts as before it too.
    fn covers(&self, at: OffsetDateTime) -> bool {
        self.after.is_none_or(|after| at < after) || self.before.is_some_and(|before| at <= before)
    }
}

impl CostStateLog {
    /// Records the row if it is a `cost-state` row, and says whether it was.
    pub(in crate::collector::claude) fn observe(
        &mut self,
        value: &Value,
        events: &mut FileEvents,
    ) -> bool {
        if value.get("type").and_then(Value::as_str) != Some("cost-state") {
            return false;
        }
        let Some(session_id) = session_id_field(value) else {
            return true;
        };
        let Some(model_usage) = value.get("modelUsage").and_then(Value::as_object) else {
            return true;
        };
        if model_usage.len() > MAX_MODELS {
            events.parse_errors += 1;
            return true;
        }
        let mut models: BTreeMap<String, TokenUsage> = BTreeMap::new();
        for (model, counters) in model_usage {
            let Some(model) = ident(model) else {
                continue;
            };
            models
                .entry(transcript_model(model).to_owned())
                .or_default()
                .add_assign(&counters_usage(counters));
        }
        self.rows.push(CostStateRow {
            session_id,
            start_time: value.get("startTime").and_then(Value::as_i64),
            before: self.latest,
            after: None,
            stale: false,
            models,
        });
        true
    }

    /// The earliest `startTime` among the processes whose totals it holds.
    pub(in crate::collector::claude) fn earliest_start(&self) -> Option<i64> {
        self.rows.iter().filter_map(|row| row.start_time).min()
    }

    /// Every other row of the transcript, in order.
    pub(in crate::collector::claude) fn note(
        &mut self,
        timestamp: Option<OffsetDateTime>,
        counted_usage: bool,
    ) {
        if counted_usage {
            for row in self.rows.iter_mut().rev().take_while(|row| !row.stale) {
                row.stale = true;
            }
        }
        if let Some(timestamp) = timestamp {
            for row in self
                .rows
                .iter_mut()
                .rev()
                .take_while(|row| row.after.is_none())
            {
                row.after = Some(timestamp);
            }
            self.latest = Some(
                self.latest
                    .map_or(timestamp, |latest| latest.max(timestamp)),
            );
        }
    }
}

/// The 1M-context variant is keyed `claude-opus-5[1m]` here, while its calls
/// are logged under the plain id; kept apart, its whole total read as uncounted.
fn transcript_model(model: &str) -> &str {
    model
        .strip_suffix(']')
        .and_then(|rest| rest.rsplit_once('['))
        .map_or(model, |(base, _)| base)
}

fn counters_usage(counters: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: u64_field(counters, "inputTokens"),
        output_tokens: u64_field(counters, "outputTokens"),
        cache_creation_input_tokens: u64_field(counters, "cacheCreationInputTokens"),
        cache_read_input_tokens: u64_field(counters, "cacheReadInputTokens"),
        cache_creation_ephemeral_5m_input_tokens: u64_field(
            counters,
            "cacheCreationEphemeral5mInputTokens",
        ),
        cache_creation_ephemeral_1h_input_tokens: u64_field(
            counters,
            "cacheCreationEphemeral1hInputTokens",
        ),
        ..TokenUsage::default()
    }
}
