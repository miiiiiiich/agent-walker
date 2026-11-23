use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use crate::collector::{FileEvents, KeyedUsageEvent};

use super::RawCounters;
use super::receipts::identity_of;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Default)]
pub(super) struct Truncated {
    /// Per session and model, the first exit read in the session's
    /// truncated journal.
    first_exits: HashMap<String, HashMap<String, FirstExit>>,
    cumulatives: HashMap<String, RawCounters>,
}

/// A journal missing its oldest segment counts everything of a model
/// before that model's first exit read in that exit, so copies of the
/// session's history held by other journals (a fork's) and dated up to
/// it give way: kept, a fork's copy of a receipt or exit from the missing
/// segment counted again. The session's own records stay; its receipts
/// already came off that exit. A copied exit whose cumulative the first
/// exit's falls below is from an accounting epoch before a reset (the
/// rule `read_exit` counts exits by), so the first exit doesn't hold it.
/// A receipt also goes by the session its accounting names: a fork of a
/// fork made before the first fork's marker files the copy under the
/// first fork. One the exit's watermark holds is inside it however it's
/// dated.
///
/// Run once the store has settled which exits count: an exit its rows
/// superseded holds nothing, and its copies dropped against it lost the
/// usage from before the store.
pub(super) fn drop_covered_copies(per_file: &mut [(PathBuf, Option<FileEvents>)]) {
    let mut truncated = Truncated::default();
    // Pooled across journals: a first exit standing in any journal that
    // read it as its first stands for every one of them.
    let mut standing: HashSet<String> = HashSet::new();
    for (path, events) in per_file.iter() {
        let Some((events, summary)) = events
            .as_ref()
            .and_then(|events| Some((events, events.copilot.as_ref()?)))
        else {
            continue;
        };
        // In the order read: a copy of an exit cut from its journal no
        // longer sets the exit's cumulative.
        truncated.cumulatives.extend(
            summary
                .exits
                .iter()
                .map(|exit| (exit.key.clone(), exit.counters)),
        );
        let Some(session) = journal(path).filter(|_| !summary.first_exits.is_empty()) else {
            continue;
        };
        let keys: HashSet<&str> = events
            .usage_events
            .iter()
            .filter_map(|keyed| keyed.key.as_deref())
            .collect();
        standing.extend(
            summary
                .first_exits
                .iter()
                .filter(|(_, exit)| keys.contains(exit.key.as_str()))
                .map(|(_, exit)| exit.key.clone()),
        );
        let exits = summary.first_exits.iter().cloned().collect();
        truncated.first_exits.insert(session.to_owned(), exits);
    }
    if standing.is_empty() {
        return;
    }
    for models in truncated.first_exits.values_mut() {
        models.retain(|_, exit| standing.contains(&exit.key));
    }
    for (path, events) in per_file.iter_mut() {
        let Some(events) = events.as_mut() else {
            continue;
        };
        let journal = journal(path);
        events
            .usage_events
            .retain(|keyed| !truncated.covers(journal, keyed));
    }
}

impl Truncated {
    /// By the session a receipt names and by the one it's filed under: an
    /// exit holds the copied receipts read before it whatever they name.
    fn covers(&self, journal: Option<&str>, keyed: &KeyedUsageEvent) -> bool {
        let event = &keyed.event;
        let (Some(model), Some(time)) = (event.model.as_deref(), event.timestamp) else {
            return false;
        };
        let copied = keyed.key.as_ref().and_then(|key| self.cumulatives.get(key));
        let holds = |session: &str, sequence: Option<u64>| {
            let Some(exit) = self
                .first_exits
                .get(session)
                .and_then(|models| models.get(model))
                .filter(|_| journal != Some(session))
            else {
                return false;
            };
            let within = time <= exit.at
                || sequence
                    .zip(exit.mark)
                    .is_some_and(|(sequence, mark)| sequence <= mark);
            within && copied.is_none_or(|copied| !exit.counters.any_less_than(*copied))
        };
        let identity = keyed.key.as_deref().and_then(identity_of);
        identity.is_some_and(|(source, sequence)| holds(source, Some(sequence)))
            || event
                .session_id
                .as_deref()
                .is_some_and(|session| holds(session, None))
    }
}

fn journal(path: &Path) -> Option<&str> {
    path.parent()?.file_name()?.to_str()
}
