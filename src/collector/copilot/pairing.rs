use std::collections::HashMap;
use std::path::PathBuf;

use time::{Duration, OffsetDateTime};

use crate::collector::FileEvents;
use crate::model::{TokenUsage, UsageGranularity};

use super::receipts::source_of;

/// Copilot's 60 s slack between a call's records, plus a second for the
/// store's default stamp, which drops the fraction.
const PAIR_SLACK: Duration = Duration::seconds(61);

/// A receipt's key and the key of the store row of its call.
pub(super) struct Pair {
    pub(super) receipt: String,
    pub(super) row: String,
}

struct Receipt<'a> {
    key: &'a str,
    time: OffsetDateTime,
    model: &'a str,
    counts: [u64; 5],
    /// The session that made the call, then each session whose log holds
    /// the receipt: a fork's copy keeps the parent as its source, and a row
    /// caught up from the fork's log is filed under the fork.
    sessions: Vec<&'a str>,
}

/// Each receipt, once per key, paired with at most one store row of the
/// same call: a row of one of its sessions with the same model and token
/// counts, dated within `PAIR_SLACK` of it. The receipt and the live event
/// behind a row carry the same usage record, and the store copies it field
/// for field. Matching by time alone took rows of other calls; whichever of
/// two identical calls a receipt takes, the counts come out the same.
pub(super) fn pair_receipts(
    per_file: &[(PathBuf, Option<FileEvents>)],
    rows: &FileEvents,
) -> Vec<Pair> {
    let mut receipts: Vec<Receipt> = Vec::new();
    let mut by_key: HashMap<&str, usize> = HashMap::new();
    for keyed in per_file
        .iter()
        .filter_map(|(_, events)| events.as_ref())
        .flat_map(|events| &events.usage_events)
        .filter(|keyed| keyed.event.granularity == UsageGranularity::Call)
    {
        let event = &keyed.event;
        let (Some(key), Some(session), Some(time), Some(model)) = (
            keyed.key.as_deref(),
            event.session_id.as_deref(),
            event.timestamp,
            event.model.as_deref(),
        ) else {
            continue;
        };
        if let Some(&index) = by_key.get(key) {
            let sessions = &mut receipts[index].sessions;
            if !sessions.contains(&session) {
                sessions.push(session);
            }
            continue;
        }
        let mut sessions: Vec<&str> = source_of(key).into_iter().collect();
        if !sessions.contains(&session) {
            sessions.push(session);
        }
        by_key.insert(key, receipts.len());
        receipts.push(Receipt {
            key,
            time,
            model,
            counts: counts(&event.usage),
            sessions,
        });
    }
    if receipts.is_empty() {
        return Vec::new();
    }
    receipts.sort_by(|a, b| (a.time, a.key).cmp(&(b.time, b.key)));

    // Oldest first, each list read once: a row too old for one receipt is
    // too old for every later one, so a cursor past it never comes back.
    let mut calls: HashMap<(&str, &str, [u64; 5]), Calls> = HashMap::new();
    for keyed in &rows.usage_events {
        let event = &keyed.event;
        if let (Some(key), Some(session), Some(time), Some(model)) = (
            keyed.key.as_deref(),
            event.session_id.as_deref(),
            event.timestamp,
            event.model.as_deref(),
        ) {
            calls
                .entry((session, model, counts(&event.usage)))
                .or_default()
                .rows
                .push((time, key));
        }
    }
    for list in calls.values_mut() {
        list.rows.sort_unstable();
    }

    let mut pairs = Vec::new();
    for receipt in &receipts {
        let from = receipt.time.checked_sub(PAIR_SLACK).unwrap_or(receipt.time);
        let to = receipt.time.checked_add(PAIR_SLACK).unwrap_or(receipt.time);
        let mut best: Option<(OffsetDateTime, &str)> = None;
        for &session in &receipt.sessions {
            let Some(list) = calls.get_mut(&(session, receipt.model, receipt.counts)) else {
                continue;
            };
            if let Some(time) = list.next_from(from)
                && time <= to
                && best.is_none_or(|(earliest, _)| time < earliest)
            {
                best = Some((time, session));
            }
        }
        if let Some((_, session)) = best
            && let Some(list) = calls.get_mut(&(session, receipt.model, receipt.counts))
            && let Some(row) = list.take()
        {
            pairs.push(Pair {
                receipt: receipt.key.to_owned(),
                row: row.to_owned(),
            });
        }
    }
    pairs
}

/// One call's store rows, oldest first, and the first not yet taken or
/// passed.
#[derive(Default)]
struct Calls<'a> {
    rows: Vec<(OffsetDateTime, &'a str)>,
    next: usize,
}

impl<'a> Calls<'a> {
    fn next_from(&mut self, from: OffsetDateTime) -> Option<OffsetDateTime> {
        while self
            .rows
            .get(self.next)
            .is_some_and(|(time, _)| *time < from)
        {
            self.next += 1;
        }
        self.rows.get(self.next).map(|(time, _)| *time)
    }

    fn take(&mut self) -> Option<&'a str> {
        let (_, key) = self.rows.get(self.next)?;
        self.next += 1;
        Some(key)
    }
}

fn counts(usage: &TokenUsage) -> [u64; 5] {
    [
        usage.input_tokens,
        usage.output_tokens,
        usage.reasoning_output_tokens,
        usage.cache_read_input_tokens,
        usage.cache_creation_input_tokens,
    ]
}
