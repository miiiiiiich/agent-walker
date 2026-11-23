use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::collector::FileEvents;
use crate::model::{TokenUsage, UsageGranularity};

use super::receipts::source_of;

/// One source per session that made calls. On the store, its rows and its
/// exits count and its receipts don't; otherwise its receipts count and its
/// exits only for what they didn't carry. A session is on the store with a
/// readable `live` row, or with `caught_up` rows only and no receipts in its
/// journal (the journal is gone); a caught-up row copies a receipt, so with
/// the receipts at hand the rows go. Each round of rules mixing the two
/// inside one session lost or doubled calls in another case, and no log on
/// disk exercises them. A receipt goes by the session its accounting
/// identity names and by every journal holding it, so a fork's copies of a
/// parent on the store don't count again. Returns the sessions whose
/// caught-up rows go.
pub(super) fn choose_sources(
    per_file: &mut [(PathBuf, Option<FileEvents>)],
    live: &HashSet<String>,
    caught_up: &HashSet<String>,
) -> HashSet<String> {
    // By the journal's directory: a fork's copies go to the parent, and by
    // their sessions a parent known only by caught-up rows, its journal gone,
    // would look like one holding receipts and lose every call not copied.
    let journals: HashSet<String> = per_file
        .iter()
        .filter(|(_, events)| {
            events.as_ref().is_some_and(|events| {
                events
                    .usage_events
                    .iter()
                    .any(|keyed| keyed.event.granularity == UsageGranularity::Call)
            })
        })
        .filter_map(|(path, _)| path.parent()?.file_name()?.to_str().map(ToOwned::to_owned))
        .collect();
    let on_store = |session: &str| {
        live.contains(session) || (caught_up.contains(session) && !journals.contains(session))
    };
    // A receipt goes if its source or any journal holding it is on the
    // store. Its source alone kept a copy without an identity, which names
    // none, and a receipt naming another session, beside the live row of a
    // journal on the store.
    let mut gone: HashSet<String> = HashSet::new();
    for keyed in per_file
        .iter()
        .filter_map(|(_, events)| events.as_ref())
        .flat_map(|events| &events.usage_events)
        .filter(|keyed| keyed.event.granularity == UsageGranularity::Call)
    {
        let Some(key) = keyed.key.as_deref() else {
            continue;
        };
        if source_of(key)
            .into_iter()
            .chain(keyed.event.session_id.as_deref())
            .any(&on_store)
        {
            gone.insert(key.to_owned());
        }
    }
    for events in per_file
        .iter_mut()
        .filter_map(|(_, events)| events.as_mut())
    {
        if let Some(summary) = &events.copilot {
            // In the order read, so the last rest standing for a key wins.
            let rests: HashMap<&str, &TokenUsage> = summary
                .rests
                .iter()
                .filter(|rest| rest.session.as_deref().is_some_and(|sid| !on_store(sid)))
                .map(|rest| (rest.key.as_str(), &rest.usage))
                .collect();
            for keyed in &mut events.usage_events {
                if let Some(rest) = keyed.key.as_deref().and_then(|key| rests.get(key)) {
                    keyed.event.usage = (*rest).clone();
                }
            }
        }
        events
            .usage_events
            .retain(|keyed| match keyed.event.granularity {
                UsageGranularity::Call => keyed.key.as_ref().is_none_or(|key| !gone.contains(key)),
                UsageGranularity::Aggregate | UsageGranularity::Remainder => {
                    keyed.event.usage.token_volume() > 0
                }
            });
    }
    caught_up
        .iter()
        .filter(|session| !on_store(session))
        .cloned()
        .collect()
}
