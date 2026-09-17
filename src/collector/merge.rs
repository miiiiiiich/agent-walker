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

/// Every keyed duplicate keeps the EARLIEST observed
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
    let mut seen_interrupts: HashMap<String, usize> = HashMap::new();
    let mut seen_paces: HashMap<String, usize> = HashMap::new();

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
                    // A fork child's copy of a turn can be a prefix of the
                    // parent's (the fork happened mid-turn): the longer
                    // observation is the complete one. Keep it whole so its
                    // end stamp, length and human wait stay consistent. Equal
                    // observations (Grok fork copies rewrite the stamp) keep
                    // the earlier stamp, whichever file was scanned first.
                    if incoming.duration_ms > existing.duration_ms {
                        *existing = incoming;
                    } else if incoming.duration_ms == existing.duration_ms {
                        existing.timestamp =
                            older_timestamp(existing.timestamp, incoming.timestamp);
                    }
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

        for keyed in events.interrupt_events {
            dedupe_into(
                &mut collection.interrupt_events,
                &mut seen_interrupts,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                },
            );
        }

        for keyed in events.pace_events {
            dedupe_into(
                &mut collection.pace_events,
                &mut seen_paces,
                keyed.key,
                keyed.event,
                |_existing, _incoming| {},
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
    use crate::collector::{KeyedDurationEvent, KeyedUsageEvent};
    use crate::model::{DurationEvent, Provider};

    fn keyed_turn(timestamp: &str, duration_ms: u64) -> KeyedDurationEvent {
        KeyedDurationEvent {
            key: Some("grok-duration:p1".to_owned()),
            event: DurationEvent {
                timestamp: Some(
                    OffsetDateTime::parse(
                        timestamp,
                        &time::format_description::well_known::Rfc3339,
                    )
                    .expect("rfc3339"),
                ),
                session_id: None,
                duration_ms,
                human_wait_ms: 0,
                model_ms: None,
                status: Some("turn".to_owned()),
            },
        }
    }

    #[test]
    fn keyed_durations_keep_longer_then_earlier() {
        let later_first = || {
            let mut collection = Collection::new(Provider::Grok, PathBuf::from("/tmp"));
            let mut a = FileEvents::default();
            a.duration_events
                .push(keyed_turn("2026-07-21T00:00:00Z", 5_000));
            let mut b = FileEvents::default();
            b.duration_events
                .push(keyed_turn("2026-07-20T00:00:00Z", 5_000));
            merge_into(
                &mut collection,
                vec![(PathBuf::from("b"), Some(a)), (PathBuf::from("a"), Some(b))],
            );
            collection
        };
        let collection = later_first();
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(
            collection.duration_events[0]
                .timestamp
                .map(OffsetDateTime::day),
            Some(20)
        );

        let mut collection = Collection::new(Provider::Grok, PathBuf::from("/tmp"));
        let mut prefix = FileEvents::default();
        prefix
            .duration_events
            .push(keyed_turn("2026-07-20T00:01:00Z", 1_000));
        let mut full = FileEvents::default();
        full.duration_events
            .push(keyed_turn("2026-07-20T00:12:00Z", 12_000));
        merge_into(
            &mut collection,
            vec![
                (PathBuf::from("p"), Some(prefix)),
                (PathBuf::from("f"), Some(full)),
            ],
        );
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 12_000);
        assert_eq!(
            collection.duration_events[0]
                .timestamp
                .map(OffsetDateTime::minute),
            Some(12)
        );
    }

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
