use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use time::OffsetDateTime;

use crate::collector::FileEvents;
use crate::model::{SourceKind, TokenUsage};

use super::transcript_model;

/// A transcript call of a session that has a cost-state.
pub(super) struct Call<'a> {
    /// The `message.id` key, shared by every copy of the call.
    pub(super) key: Option<&'a str>,
    pub(super) timestamp: Option<OffsetDateTime>,
    pub(super) model: String,
    pub(super) usage: TokenUsage,
    pub(super) project: Option<String>,
    pub(super) source_kind: SourceKind,
}

/// The calls of the sessions that have a cost-state, each `message.id` once
/// within its session, as the totals count them. The collection-wide dedupe
/// keeps a copy shared by two sessions in only one of them, which would read
/// as uncounted in the other.
pub(super) fn transcripts<'a>(
    per_file: &'a [(PathBuf, Option<FileEvents>)],
    sessions: &HashSet<&'a str>,
) -> HashMap<&'a str, Vec<Call<'a>>> {
    let mut calls: HashMap<&str, Vec<Call>> = HashMap::new();
    let mut seen: HashMap<(&str, &str), usize> = HashMap::new();
    for (_, events) in per_file {
        let Some(events) = events else {
            continue;
        };
        for keyed in &events.usage_events {
            let event = &keyed.event;
            let Some(&session) = event.session_id.as_deref().and_then(|id| sessions.get(id)) else {
                continue;
            };
            let call = Call {
                key: keyed.key.as_deref(),
                timestamp: event.timestamp,
                model: transcript_model(event.model.as_deref().unwrap_or("unknown")).to_owned(),
                usage: event.usage.clone(),
                project: event.project.clone(),
                source_kind: event.source_kind,
            };
            let session_calls = calls.entry(session).or_default();
            let Some(key) = call.key else {
                session_calls.push(call);
                continue;
            };
            if let Some(&index) = seen.get(&(session, key)) {
                let kept = &mut session_calls[index];
                let earliest = match (kept.timestamp, call.timestamp) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                if call.usage.token_volume() > kept.usage.token_volume() {
                    let project = kept.project.take();
                    *kept = call;
                    kept.project = kept.project.take().or(project);
                } else if kept.project.is_none() {
                    kept.project = call.project;
                }
                kept.timestamp = earliest;
            } else {
                seen.insert((session, key), session_calls.len());
                session_calls.push(call);
            }
        }
    }
    calls
}
