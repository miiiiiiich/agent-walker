use std::collections::HashMap;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::collector::{FileEvents, KeyedUsageEvent};
use crate::model::TokenUsage;

use super::{RawCounters, helper_event};

/// Key prefix of the event carrying a model's first exit read in a journal
/// that lost its oldest segment: the session, model, time and cumulative.
const TRUNCATED_PREFIX: &str = "copilot-truncated:";

/// Key prefix of the event carrying an exit's cumulative, for the exit keyed
/// by the rest of the key: the exit itself counts only what's new.
const CUMULATIVE_PREFIX: &str = "copilot-cumulative:";

pub(super) fn first_exit(
    session: &str,
    model: String,
    at: OffsetDateTime,
    counters: RawCounters,
) -> KeyedUsageEvent {
    let mut event = helper_event(
        TRUNCATED_PREFIX.to_owned(),
        Some(session.to_owned()),
        Some(model),
        at,
    );
    event.event.usage = carried(counters);
    event
}

pub(super) fn cumulative(exit: &KeyedUsageEvent, counters: RawCounters) -> Option<KeyedUsageEvent> {
    let key = exit.key.as_ref()?;
    let mut event = exit.event.clone();
    event.usage = carried(counters);
    Some(KeyedUsageEvent {
        key: Some(format!("{CUMULATIVE_PREFIX}{key}")),
        event,
    })
}

/// Raw, unlike a counted usage, so the reset rule reads what Copilot wrote.
fn carried(counters: RawCounters) -> TokenUsage {
    TokenUsage {
        input_tokens: counters.input,
        output_tokens: counters.output,
        reasoning_output_tokens: counters.reasoning,
        cache_read_input_tokens: counters.cache_read,
        cache_creation_input_tokens: counters.cache_write,
        ..TokenUsage::default()
    }
}

fn held(usage: &TokenUsage) -> RawCounters {
    RawCounters {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_input_tokens,
        cache_write: usage.cache_creation_input_tokens,
        reasoning: usage.reasoning_output_tokens,
    }
}

#[derive(Default)]
pub(super) struct Truncated {
    /// Per session and model, the first exit read in the session's
    /// truncated journal.
    first_exits: HashMap<String, HashMap<String, (OffsetDateTime, RawCounters)>>,
    cumulatives: HashMap<String, RawCounters>,
}

/// Takes both kinds of event out before anything counts them.
pub(super) fn take(per_file: &mut [(PathBuf, Option<FileEvents>)]) -> Truncated {
    let mut truncated = Truncated::default();
    for events in per_file
        .iter_mut()
        .filter_map(|(_, events)| events.as_mut())
    {
        events.usage_events.retain(|keyed| {
            let Some(key) = keyed.key.as_deref() else {
                return true;
            };
            let event = &keyed.event;
            if let Some(exit) = key.strip_prefix(CUMULATIVE_PREFIX) {
                truncated
                    .cumulatives
                    .insert(exit.to_owned(), held(&event.usage));
                return false;
            }
            if !key.starts_with(TRUNCATED_PREFIX) {
                return true;
            }
            if let (Some(session), Some(model), Some(at)) =
                (&event.session_id, &event.model, event.timestamp)
            {
                truncated
                    .first_exits
                    .entry(session.clone())
                    .or_default()
                    .insert(model.clone(), (at, held(&event.usage)));
            }
            false
        });
    }
    truncated
}

impl Truncated {
    /// A journal missing its oldest segment counts everything of a model
    /// before that model's first exit read in that exit, so copies of the
    /// session's history held by other journals (a fork's) and dated up to
    /// it give way: kept, a fork's copy of a receipt or exit from the missing
    /// segment counted again. The session's own records stay; its receipts
    /// already came off that exit. A copied exit whose cumulative the first
    /// exit's falls below is from an accounting epoch before a reset (the
    /// rule of `collect_shutdown_usage`), so the first exit doesn't hold it.
    pub(super) fn drop_covered_copies(&self, per_file: &mut [(PathBuf, Option<FileEvents>)]) {
        if self.first_exits.is_empty() {
            return;
        }
        for (path, events) in per_file.iter_mut() {
            let Some(events) = events.as_mut() else {
                continue;
            };
            let journal = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str());
            events
                .usage_events
                .retain(|keyed| !self.covers(journal, keyed));
        }
    }

    fn covers(&self, journal: Option<&str>, keyed: &KeyedUsageEvent) -> bool {
        let event = &keyed.event;
        let (Some(session), Some(model), Some(time)) = (
            event.session_id.as_deref(),
            event.model.as_deref(),
            event.timestamp,
        ) else {
            return false;
        };
        if journal == Some(session) {
            return false;
        }
        let Some((at, counters)) = self
            .first_exits
            .get(session)
            .and_then(|models| models.get(model))
        else {
            return false;
        };
        let earlier_epoch = keyed
            .key
            .as_ref()
            .and_then(|key| self.cumulatives.get(key))
            .is_some_and(|copied| counters.any_less_than(*copied));
        time <= *at && !earlier_epoch
    }
}
