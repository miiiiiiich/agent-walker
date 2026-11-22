use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::collector::FileEvents;
use crate::model::{TokenUsage, UsageGranularity};

use super::receipts::source_of;

/// Key prefix of what an exit's own receipts carried, for the exit keyed by
/// the rest of the key. Stripped here, before anything else reads the events.
pub(super) const OWN_PREFIX: &str = "copilot-own:";

/// One source per session that made calls. With any readable store row,
/// its rows and its exits count and its receipts don't; without, its
/// receipts count and its exits only for what they didn't carry. Each round
/// of rules mixing the two inside one session lost or doubled calls in
/// another case, and no log on disk exercises them. A receipt goes by the
/// session its accounting identity names, so a fork's copies of a parent
/// with store rows don't count again.
pub(super) fn choose_sources(
    per_file: &mut [(PathBuf, Option<FileEvents>)],
    store: &HashSet<String>,
) {
    for events in per_file
        .iter_mut()
        .filter_map(|(_, events)| events.as_mut())
    {
        let mut own: HashMap<String, TokenUsage> = HashMap::new();
        events.usage_events.retain(|keyed| {
            let Some(exit) = keyed
                .key
                .as_deref()
                .and_then(|key| key.strip_prefix(OWN_PREFIX))
            else {
                return true;
            };
            if keyed
                .event
                .session_id
                .as_ref()
                .is_some_and(|sid| !store.contains(sid))
            {
                own.insert(exit.to_owned(), keyed.event.usage.clone());
            }
            false
        });
        for keyed in &mut events.usage_events {
            if let Some(carried) = keyed.key.as_ref().and_then(|key| own.get(key)) {
                take_off(&mut keyed.event.usage, carried);
            }
        }
        events
            .usage_events
            .retain(|keyed| match keyed.event.granularity {
                UsageGranularity::Call => {
                    let source = keyed
                        .key
                        .as_deref()
                        .and_then(source_of)
                        .or(keyed.event.session_id.as_deref());
                    source.is_none_or(|source| !store.contains(source))
                }
                UsageGranularity::Aggregate | UsageGranularity::Remainder => {
                    keyed.event.usage.token_volume() > 0
                }
            });
    }
}

fn take_off(usage: &mut TokenUsage, carried: &TokenUsage) {
    usage.input_tokens = usage.input_tokens.saturating_sub(carried.input_tokens);
    usage.output_tokens = usage.output_tokens.saturating_sub(carried.output_tokens);
    usage.reasoning_output_tokens = usage
        .reasoning_output_tokens
        .saturating_sub(carried.reasoning_output_tokens);
    usage.cache_read_input_tokens = usage
        .cache_read_input_tokens
        .saturating_sub(carried.cache_read_input_tokens);
    usage.cache_creation_input_tokens = usage
        .cache_creation_input_tokens
        .saturating_sub(carried.cache_creation_input_tokens);
}
