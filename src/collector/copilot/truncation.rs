use std::collections::HashMap;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::collector::FileEvents;

/// Key prefix of the event carrying a session whose journal lost its oldest
/// segment, and the time of its first exit read.
pub(super) const TRUNCATED_PREFIX: &str = "copilot-truncated:";

/// A journal missing its oldest segment counts everything before its first
/// exit read in that exit, so copies of the session's history held by other
/// journals (a fork's) and dated up to it give way: kept, a fork's copy of a
/// receipt or exit from the missing segment counted again. The session's
/// own records stay; its receipts already came off that exit.
pub(super) fn drop_copies_inside_truncated_exits(per_file: &mut [(PathBuf, Option<FileEvents>)]) {
    let mut first_exits: HashMap<String, OffsetDateTime> = HashMap::new();
    for events in per_file
        .iter_mut()
        .filter_map(|(_, events)| events.as_mut())
    {
        events.usage_events.retain(|keyed| {
            if !keyed
                .key
                .as_deref()
                .is_some_and(|key| key.starts_with(TRUNCATED_PREFIX))
            {
                return true;
            }
            if let (Some(session), Some(time)) =
                (keyed.event.session_id.clone(), keyed.event.timestamp)
            {
                first_exits
                    .entry(session)
                    .and_modify(|at| *at = (*at).max(time))
                    .or_insert(time);
            }
            false
        });
    }
    if first_exits.is_empty() {
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
        events.usage_events.retain(|keyed| {
            let (Some(session), Some(time)) =
                (keyed.event.session_id.as_deref(), keyed.event.timestamp)
            else {
                return true;
            };
            journal == Some(session) || first_exits.get(session).is_none_or(|first| time > *first)
        });
    }
}
