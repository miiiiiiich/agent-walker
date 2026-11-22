use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use time::{Duration, OffsetDateTime};

use crate::collector::FileEvents;
use crate::model::{TokenUsage, UsageGranularity};

use super::receipts::source_of;

/// Copilot's 60 s slack between a call's records, plus a second for the
/// store's default stamp, which drops the fraction.
const PAIR_SLACK: Duration = Duration::seconds(61);

/// What a call with several records drops: its receipt when a live row
/// stands for it, and every row caught up from it.
pub(super) struct Copies {
    pub(super) receipt: Option<String>,
    pub(super) caught_up: Vec<String>,
}

struct Receipt<'a> {
    key: &'a str,
    time: OffsetDateTime,
    model: &'a str,
    counts: [u64; 5],
    /// The session that made the call, whose live row it is.
    source: Option<&'a str>,
    /// Each session whose log holds the receipt: a fork's copy keeps the
    /// parent as its source, and a row caught up from the fork's log is
    /// filed under the fork.
    logs: Vec<&'a str>,
}

/// Each receipt, once per key, with the store rows of the same call: at most
/// one live row of the session that made it, and at most one caught-up row
/// per session that made it or holds it in its log (the store keeps one row
/// per session and event). A row is the call's when it has the same model
/// and token counts and is dated within `PAIR_SLACK` of the receipt. The
/// receipt and the live event behind a row are the same usage record in the
/// schema, and local rows add up to the exits field for field; that a
/// receipt's counts equal its row's is not verified on disk. Matching by
/// time alone took rows of other calls; whichever of two identical calls a
/// receipt takes, the counts come out the same.
pub(super) fn pair_receipts(
    per_file: &[(PathBuf, Option<FileEvents>)],
    rows: &FileEvents,
    caught_up: &HashSet<&str>,
) -> Vec<Copies> {
    let mut receipts = receipts(per_file);
    if receipts.is_empty() {
        return Vec::new();
    }
    receipts.sort_by(|a, b| (a.time, a.key).cmp(&(b.time, b.key)));

    // Oldest first, each list read once: a row too old for one receipt is
    // too old for every later one, so a cursor past it never comes back.
    let mut calls: HashMap<CallKey, Calls> = HashMap::new();
    for keyed in &rows.usage_events {
        let event = &keyed.event;
        if let (Some(key), Some(session), Some(time), Some(model)) = (
            keyed.key.as_deref(),
            event.session_id.as_deref(),
            event.timestamp,
            event.model.as_deref(),
        ) {
            let call = (
                caught_up.contains(key),
                session,
                model,
                counts(&event.usage),
            );
            calls.entry(call).or_default().rows.push((time, key));
        }
    }
    for list in calls.values_mut() {
        list.rows.sort_unstable();
    }

    let mut found = Vec::new();
    for receipt in &receipts {
        let from = receipt.time.checked_sub(PAIR_SLACK).unwrap_or(receipt.time);
        let to = receipt.time.checked_add(PAIR_SLACK).unwrap_or(receipt.time);
        let live_sessions = receipt
            .source
            .map_or_else(|| receipt.logs.clone(), |source| vec![source]);
        let mut live: Option<(OffsetDateTime, &str)> = None;
        for &session in &live_sessions {
            if let Some(list) = calls.get_mut(&receipt.call(false, session))
                && let Some(time) = list.next_from(from)
                && time <= to
                && live.is_none_or(|(earliest, _)| time < earliest)
            {
                live = Some((time, session));
            }
        }
        let live = live
            .and_then(|(_, session)| calls.get_mut(&receipt.call(false, session))?.take())
            .is_some();
        let mut sessions: Vec<&str> = receipt.source.into_iter().collect();
        for &session in &receipt.logs {
            if !sessions.contains(&session) {
                sessions.push(session);
            }
        }
        let mut copies = Vec::new();
        for session in sessions {
            if let Some(list) = calls.get_mut(&receipt.call(true, session))
                && list.next_from(from).is_some_and(|time| time <= to)
                && let Some(row) = list.take()
            {
                copies.push(row.to_owned());
            }
        }
        if live || !copies.is_empty() {
            found.push(Copies {
                receipt: live.then(|| receipt.key.to_owned()),
                caught_up: copies,
            });
        }
    }
    found
}

fn receipts(per_file: &[(PathBuf, Option<FileEvents>)]) -> Vec<Receipt<'_>> {
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
            let logs = &mut receipts[index].logs;
            if !logs.contains(&session) {
                logs.push(session);
            }
            continue;
        }
        by_key.insert(key, receipts.len());
        receipts.push(Receipt {
            key,
            time,
            model,
            counts: counts(&event.usage),
            source: source_of(key),
            logs: vec![session],
        });
    }
    receipts
}

/// Caught up or live, session, model, token counts.
type CallKey<'a> = (bool, &'a str, &'a str, [u64; 5]);

impl<'a> Receipt<'a> {
    fn call(&self, caught_up: bool, session: &'a str) -> CallKey<'a> {
        (caught_up, session, self.model, self.counts)
    }
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
