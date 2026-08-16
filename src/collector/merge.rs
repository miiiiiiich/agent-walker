//! Cross-file merge and keyed deduplication of collected events.
use std::collections::HashMap;
use std::path::PathBuf;

use time::OffsetDateTime;
use tracing::debug;

use super::events::FileEvents;
use crate::model::{Collection, UsageEvent};

/// Fill missing metadata on `target` from `source`. Duplicate keyed usage
/// lines can carry different metadata (a streaming fragment has the tokens
/// but not yet the attribution fields), so whichever variant wins on token
/// volume must still absorb the other's metadata instead of discarding it.
fn fill_usage_metadata(target: &mut UsageEvent, source: &UsageEvent) {
    if target.timestamp.is_none() {
        target.timestamp = source.timestamp;
    }
    if target.session_id.is_none() {
        target.session_id.clone_from(&source.session_id);
    }
    if target.model.is_none() {
        target.model.clone_from(&source.model);
    }
    if target.attribution_agent.is_none() {
        target
            .attribution_agent
            .clone_from(&source.attribution_agent);
    }
    if target.attribution_skill.is_none() {
        target
            .attribution_skill
            .clone_from(&source.attribution_skill);
    }
    if target.project.is_none() {
        target.project.clone_from(&source.project);
    }
    if target.reported_cost_usd.is_none() {
        target.reported_cost_usd = source.reported_cost_usd;
    }
}

/// Earlier of two optional timestamps; a lone `Some` beats `None`. Used on
/// keyed duplicates so the original observation's time wins over a replayed
/// copy stamped at the fork instant, whatever the file scan order.
fn older_timestamp(a: Option<OffsetDateTime>, b: Option<OffsetDateTime>) -> Option<OffsetDateTime> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Insert one keyed event into `sink`: unkeyed events pass through, the first
/// occurrence of a key is kept, and later duplicates fold into it via `merge`
/// (metadata fill, OR flags, earliest timestamp).
fn dedupe_into<E>(
    sink: &mut Vec<E>,
    seen: &mut HashMap<String, usize>,
    key: Option<String>,
    event: E,
    merge: impl FnOnce(&mut E, E),
) {
    match key {
        Some(key) => {
            if let Some(index) = seen.get(&key).copied() {
                merge(&mut sink[index], event);
            } else {
                seen.insert(key, sink.len());
                sink.push(event);
            }
        }
        None => sink.push(event),
    }
}

fn absorb_file_stats(collection: &mut Collection, events: &FileEvents) {
    collection.stats.lines_seen += events.lines_seen;
    collection.stats.parse_errors += events.parse_errors;
}

/// Merge ordered per-file events into the collection, applying cross-file
/// deduplication. Keyed usage duplicates keep the variant with the larger
/// token volume but fill missing metadata from the loser; keyed tool /
/// rate-limit / effort duplicates are dropped; keyed mode duplicates merge
/// their flags with OR. Every keyed duplicate keeps the EARLIEST observed
/// timestamp: a fork replay is stamped at the fork instant, and file scan
/// order doesn't put originals first (`archived_sessions` sorts before
/// `sessions` wholesale), so first-seen-wins would let a replay shift an
/// event's day attribution.
#[allow(
    clippy::too_many_lines,
    reason = "One homogeneous keyed-dedupe loop per event kind; splitting them adds indirection without reuse and the count grows with event kinds, not complexity."
)]
pub fn merge_into(collection: &mut Collection, per_file: Vec<(PathBuf, Option<FileEvents>)>) {
    let mut seen_usage: HashMap<String, usize> = HashMap::new();
    let mut seen_tools: HashMap<String, usize> = HashMap::new();
    let mut seen_limits: HashMap<String, usize> = HashMap::new();
    let mut seen_credits: HashMap<String, usize> = HashMap::new();
    let mut seen_durations: HashMap<String, usize> = HashMap::new();
    let mut seen_efforts: HashMap<String, usize> = HashMap::new();
    let mut seen_modes: HashMap<String, usize> = HashMap::new();
    let mut seen_permissions: HashMap<String, usize> = HashMap::new();

    for (path, events) in per_file {
        collection.stats.files_seen += 1;
        let Some(events) = events else {
            collection.stats.unreadable_files += 1;
            debug!(path = %path.display(), "skipping unreadable log file");
            continue;
        };
        absorb_file_stats(collection, &events);
        collection.session_touches.extend(events.session_touches);
        for keyed in events.duration_events {
            dedupe_into(
                &mut collection.duration_events,
                &mut seen_durations,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                },
            );
        }

        for keyed in events.usage_events {
            dedupe_into(
                &mut collection.usage_events,
                &mut seen_usage,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    let timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                    if incoming.usage.token_volume() > existing.usage.token_volume() {
                        let mut incoming = incoming;
                        fill_usage_metadata(&mut incoming, existing);
                        *existing = incoming;
                    } else {
                        fill_usage_metadata(existing, &incoming);
                    }
                    existing.timestamp = timestamp;
                },
            );
        }

        for keyed in events.tool_events {
            dedupe_into(
                &mut collection.tool_events,
                &mut seen_tools,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                },
            );
        }

        for keyed in events.rate_limit_samples {
            dedupe_into(
                &mut collection.rate_limit_samples,
                &mut seen_limits,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = existing.timestamp.min(incoming.timestamp);
                },
            );
        }

        for keyed in events.credit_samples {
            dedupe_into(
                &mut collection.credit_samples,
                &mut seen_credits,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = existing.timestamp.min(incoming.timestamp);
                },
            );
        }

        for keyed in events.effort_events {
            dedupe_into(
                &mut collection.effort_events,
                &mut seen_efforts,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                },
            );
        }

        for keyed in events.permission_events {
            dedupe_into(
                &mut collection.permission_events,
                &mut seen_permissions,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                },
            );
        }

        for keyed in events.mode_events {
            dedupe_into(
                &mut collection.mode_events,
                &mut seen_modes,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.has_thinking |= incoming.has_thinking;
                    existing.fast |= incoming.fast;
                    existing.timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                },
            );
        }
    }

    collection.stats.usage_events = collection.usage_events.len();
    collection.stats.tool_events = collection.tool_events.len();
    collection.stats.duration_events = collection.duration_events.len();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::KeyedUsageEvent;

    /// A keyed usage duplicate (Claude streaming fragments of one message,
    /// or a Codex fork replay) keeps the larger token volume, the EARLIEST
    /// timestamp, and metadata from both sides — whichever file order they
    /// arrive in.
    #[test]
    fn keyed_usage_merge_keeps_larger_volume_and_earliest_timestamp() {
        use crate::model::{Provider, TokenUsage};

        let early = OffsetDateTime::from_unix_timestamp(1_000).expect("valid timestamp");
        let late = OffsetDateTime::from_unix_timestamp(2_000).expect("valid timestamp");
        let event =
            |timestamp, input_tokens, model: Option<&str>, project: Option<&str>| KeyedUsageEvent {
                key: Some("message:m1".to_owned()),
                event: UsageEvent {
                    timestamp: Some(timestamp),
                    session_id: Some("s1".to_owned()),
                    model: model.map(ToOwned::to_owned),
                    source_kind: crate::model::SourceKind::Main,
                    attribution_agent: None,
                    attribution_skill: None,
                    project: project.map(ToOwned::to_owned),
                    usage: TokenUsage {
                        input_tokens,
                        ..TokenUsage::default()
                    },
                    reported_cost_usd: None,
                },
            };
        // Small fragment first with the early timestamp and a project; the
        // larger fragment arrives later with the model but no project.
        let small_early = event(early, 10, None, Some("proj"));
        let large_late = event(late, 20, Some("claude"), None);

        let mut collection = Collection::new(Provider::Claude, PathBuf::new());
        let per_file = vec![
            (
                PathBuf::from("a.jsonl"),
                Some(FileEvents {
                    usage_events: vec![small_early],
                    ..FileEvents::default()
                }),
            ),
            (
                PathBuf::from("b.jsonl"),
                Some(FileEvents {
                    usage_events: vec![large_late],
                    ..FileEvents::default()
                }),
            ),
        ];
        merge_into(&mut collection, per_file);

        assert_eq!(collection.usage_events.len(), 1);
        let merged = &collection.usage_events[0];
        assert_eq!(merged.usage.input_tokens, 20); // larger volume wins
        assert_eq!(merged.timestamp, Some(early)); // earliest timestamp wins
        assert_eq!(merged.model.as_deref(), Some("claude")); // metadata from both
        assert_eq!(merged.project.as_deref(), Some("proj"));
    }
}
