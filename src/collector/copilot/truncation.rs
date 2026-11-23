use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use serde_json::Value;

use crate::collector::{FileEvents, KeyedUsageEvent};
use crate::model::TokenUsage;

use super::receipts::identity_of;
use super::{RawCounters, helper_event};

/// Key prefix of the event carrying a model's first exit read in a journal
/// that lost its oldest segment: the session, model, time and cumulative,
/// and after the prefix the exit's watermark for the session's own receipts
/// and the exit's key.
const TRUNCATED_PREFIX: &str = "copilot-truncated:";

/// Key prefix of the event carrying an exit's cumulative, for the exit keyed
/// by the rest of the key: the exit itself counts only what's new.
const CUMULATIVE_PREFIX: &str = "copilot-cumulative:";

pub(super) struct FirstExit {
    pub(super) at: OffsetDateTime,
    pub(super) counters: RawCounters,
    /// The highest sequence of the session's own receipts it holds.
    pub(super) mark: Option<u64>,
    pub(super) key: String,
}

/// Without watermarks the exit predates receipts and holds all of them; with
/// watermarks naming other sessions only, none of its own.
pub(super) fn own_mark(exit: &Value, session_id: Option<&String>) -> Option<u64> {
    match exit
        .pointer("/data/usageAccountingWatermarks")
        .and_then(Value::as_object)
    {
        None => Some(u64::MAX),
        Some(marks) => marks.get(session_id?.as_str())?.as_u64(),
    }
}

pub(super) fn first_exit(session: &str, model: String, exit: &FirstExit) -> KeyedUsageEvent {
    let mark = exit.mark.map(|mark| mark.to_string()).unwrap_or_default();
    let mut event = helper_event(
        format!("{TRUNCATED_PREFIX}{mark}:{}", exit.key),
        Some(session.to_owned()),
        Some(model),
        exit.at,
    );
    event.event.usage = carried(exit.counters);
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
    first_exits: HashMap<String, HashMap<String, FirstExit>>,
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
            let Some(rest) = key.strip_prefix(TRUNCATED_PREFIX) else {
                return true;
            };
            if let (Some(session), Some(model), Some(at), Some((mark, exit_key))) = (
                &event.session_id,
                &event.model,
                event.timestamp,
                rest.split_once(':'),
            ) {
                let exit = FirstExit {
                    at,
                    counters: held(&event.usage),
                    mark: mark.parse().ok(),
                    key: exit_key.to_owned(),
                };
                truncated
                    .first_exits
                    .entry(session.clone())
                    .or_default()
                    .insert(model.clone(), exit);
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
    /// A receipt goes by the session its accounting names, not the one it's
    /// filed under: a fork of a fork made before the first fork's marker
    /// files the copy under the first fork. One the exit's watermark holds
    /// is inside it however it's dated.
    ///
    /// Run once the store has settled which exits count: an exit its rows
    /// superseded holds nothing, and its copies dropped against it lost the
    /// usage from before the store.
    pub(super) fn drop_covered_copies(mut self, per_file: &mut [(PathBuf, Option<FileEvents>)]) {
        let mut standing: HashSet<String> = HashSet::new();
        for (path, events) in per_file.iter() {
            let (Some(models), Some(events)) = (
                journal(path).and_then(|session| self.first_exits.get(session)),
                events,
            ) else {
                continue;
            };
            for key in events
                .usage_events
                .iter()
                .filter_map(|keyed| keyed.key.as_ref())
            {
                if models.values().any(|exit| exit.key == *key) {
                    standing.insert(key.clone());
                }
            }
        }
        for models in self.first_exits.values_mut() {
            models.retain(|_, exit| standing.contains(&exit.key));
        }
        if standing.is_empty() {
            return;
        }
        for (path, events) in per_file.iter_mut() {
            let Some(events) = events.as_mut() else {
                continue;
            };
            let journal = journal(path);
            events
                .usage_events
                .retain(|keyed| !self.covers(journal, keyed));
        }
    }

    fn covers(&self, journal: Option<&str>, keyed: &KeyedUsageEvent) -> bool {
        let event = &keyed.event;
        let identity = keyed.key.as_deref().and_then(identity_of);
        let (Some(session), Some(model), Some(time)) = (
            identity
                .map(|(source, _)| source)
                .or(event.session_id.as_deref()),
            event.model.as_deref(),
            event.timestamp,
        ) else {
            return false;
        };
        if journal == Some(session) {
            return false;
        }
        let Some(exit) = self
            .first_exits
            .get(session)
            .and_then(|models| models.get(model))
        else {
            return false;
        };
        let held =
            identity.is_some_and(|(_, sequence)| exit.mark.is_some_and(|mark| sequence <= mark));
        let earlier_epoch = keyed
            .key
            .as_ref()
            .and_then(|key| self.cumulatives.get(key))
            .is_some_and(|copied| exit.counters.any_less_than(*copied));
        (time <= exit.at || held) && !earlier_epoch
    }
}

fn journal(path: &Path) -> Option<&str> {
    path.parent()?.file_name()?.to_str()
}
