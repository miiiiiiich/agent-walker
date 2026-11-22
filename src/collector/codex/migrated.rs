//! Codex's rollout migration rewrote some spawned children with every row
//! stamped at the fork instant, so the fork window dropped the child's own
//! calls with the copies of its parent. Those children predate
//! `token_usage_record`, so no row names its owner either. Their window rows
//! are held while the file parses and checked against the parent's rollout
//! once every file is parsed: a `token_count` the parent also holds is a
//! copy, the rest are the child's own. Without the parent in the scan the
//! window stands, as for any fork.
//!
//! Re-keying the child's rows under the parent's id to let the merge collapse
//! the copies was rejected: a parent with records counts none of its
//! `token_count`s, so the copies would have nothing to collapse into.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde_json::Value;
use time::OffsetDateTime;

use crate::collector::FileEvents;
use crate::collector::KeyedUsageEvent;
use crate::collector::json::parse_line;
use crate::collector::read::Lines;

use super::fork::in_fork_burst;
use super::rollout::{open_rollout, thread_id_of};
use super::usage::token_count_fingerprint;

/// Prefixes a held row's key until `settle` decides it; a held row never
/// reaches the merge.
const HELD: &str = "codex-held:";

/// A copied fork child's window rows, held until its shape is known.
#[derive(Default)]
pub(super) struct HeldRows {
    parent: Option<String>,
    rows: Vec<(String, KeyedUsageEvent)>,
    any_record: bool,
    left_window: bool,
}

impl HeldRows {
    pub(super) fn for_fork_of(parent: Option<String>) -> Self {
        Self {
            parent,
            ..Self::default()
        }
    }

    pub(super) fn wanted(&self) -> bool {
        self.parent.is_some() && !self.any_record && !self.left_window
    }

    /// Every row after the child's own `session_meta`.
    pub(super) fn note(
        &mut self,
        value: &Value,
        fork_instant: Option<OffsetDateTime>,
        timestamp: Option<OffsetDateTime>,
    ) {
        if value.get("type").and_then(Value::as_str) == Some("token_usage_record") {
            self.any_record = true;
        }
        if let (Some(fork), Some(at)) = (fork_instant, timestamp)
            && !in_fork_burst(fork, at)
        {
            self.left_window = true;
        }
        if !self.wanted() {
            self.rows.clear();
        }
    }

    /// A window `token_count` and the usage it would have counted.
    pub(super) fn hold(&mut self, value: &Value, counted: FileEvents) {
        let Some(pair) = token_count_fingerprint(value) else {
            return;
        };
        for event in counted.usage_events {
            if event.key.is_some() {
                self.rows.push((pair.clone(), event));
            }
        }
    }

    /// A child with a record, or with a row past the fork window, kept its
    /// own timestamps, and the window already told its rows apart.
    pub(super) fn finish(self, events: &mut FileEvents) {
        if !self.wanted() {
            return;
        }
        let Some(parent) = self.parent else {
            return;
        };
        for (pair, mut event) in self.rows {
            let Some(key) = event.key.take() else {
                continue;
            };
            event.key = Some(format!("{HELD}{pair}|{}:{parent}{key}", parent.len()));
            events.usage_events.push(event);
        }
    }
}

struct Held<'a> {
    pair: &'a str,
    parent: &'a str,
    key: &'a str,
}

fn parse_held(key: &str) -> Option<Held<'_>> {
    let (pair, rest) = key.strip_prefix(HELD)?.split_once('|')?;
    let (len, rest) = rest.split_once(':')?;
    let len: usize = len.parse().ok()?;
    Some(Held {
        pair,
        parent: rest.get(..len)?,
        key: rest.get(len..)?,
    })
}

/// Decides every held row before the merge: a row the parent's rollout also
/// holds is a copy and goes; the child's own takes its key back. Rows of a
/// parent outside the scan go, as the window would have had them.
pub(super) fn settle(files: &[PathBuf], per_file: &mut [(PathBuf, Option<FileEvents>)]) {
    let mut parents: HashMap<String, Option<HashSet<String>>> = HashMap::new();
    for events in per_file.iter().filter_map(|(_, events)| events.as_ref()) {
        for event in &events.usage_events {
            if let Some(held) = event.key.as_deref().and_then(parse_held) {
                parents.entry(held.parent.to_owned()).or_default();
            }
        }
    }
    if parents.is_empty() {
        return;
    }
    let parent_files: Vec<(String, &PathBuf)> = files
        .iter()
        .filter_map(|path| Some((thread_id_of(path)?, path)))
        .filter(|(thread, _)| parents.contains_key(thread))
        .collect();
    let read: Vec<(String, HashSet<String>)> = parent_files
        .into_par_iter()
        .map(|(thread, path)| (thread, token_count_pairs(path)))
        .collect();
    for (thread, pairs) in read {
        if let Some(known) = parents.get_mut(&thread) {
            known.get_or_insert_with(HashSet::new).extend(pairs);
        }
    }
    for events in per_file
        .iter_mut()
        .filter_map(|(_, events)| events.as_mut())
    {
        events.usage_events.retain_mut(|event| {
            let Some(held) = event.key.as_deref().and_then(parse_held) else {
                return true;
            };
            let own = parents
                .get(held.parent)
                .and_then(Option::as_ref)
                .is_some_and(|pairs| !pairs.contains(held.pair));
            let key = own.then(|| held.key.to_owned());
            if own {
                event.key = key;
            }
            own
        });
    }
}

/// Every `token_count` the rollout holds, counted or not: the parent's own
/// copies of its parent and its echoes of records are what a child copies too.
fn token_count_pairs(path: &Path) -> HashSet<String> {
    let mut pairs = HashSet::new();
    let Some(reader) = open_rollout(path) else {
        return pairs;
    };
    let mut lines = Lines::new(reader);
    let mut scratch = FileEvents::default();
    while let Some((_, line)) = lines.next(&mut scratch) {
        // Most rows are not `token_count`s; skip parsing them.
        if !std::str::from_utf8(line).is_ok_and(|text| text.contains("\"token_count\"")) {
            continue;
        }
        if let Some(pair) = parse_line(line, &mut scratch)
            .as_ref()
            .and_then(token_count_fingerprint)
        {
            pairs.insert(pair);
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use time::UtcOffset;

    use super::*;
    use crate::collector::codex::collect;
    use crate::collector::codex::tests::{
        meta_line, record_line, token_count_line, total_volume, usage_json, write_lines,
    };

    const SPAWN: &str = r#"{"subagent":{"thread_spawn":{"parent_thread_id":"p1","depth":1}}}"#;

    /// The parent: one call counted from its record (its `token_count` is
    /// the echo), then two counted from `token_count`s.
    fn write_parent(dir: &Path) {
        let first = usage_json(100, 40, 10);
        write_lines(
            &dir.join("rollout-2026-06-01T00-00-00-p1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "p1", r#""cli""#, ""),
                record_line("2026-06-01T00:00:03Z", "p1", "resp_1", &first),
                token_count_line("2026-06-01T00:00:03.100Z", &first, 110),
                token_count_line("2026-06-01T00:00:09Z", &usage_json(120, 0, 20), 250),
                token_count_line("2026-06-01T00:00:20Z", &usage_json(200, 0, 20), 470),
            ],
        );
    }

    /// A migrated child: every row at the fork instant, the parent's first
    /// two calls copied, then its own two calls, the last one sent twice.
    fn migrated_child(ts: &str) -> Vec<String> {
        let own = usage_json(90, 10, 40);
        vec![
            meta_line(ts, "c1", SPAWN, r#","forked_from_id":"p1""#),
            token_count_line(ts, &usage_json(100, 40, 10), 110),
            token_count_line(ts, &usage_json(120, 0, 20), 250),
            token_count_line(ts, &usage_json(60, 0, 6), 316),
            token_count_line(ts, &own, 446),
            token_count_line(ts, &own, 446),
        ]
    }

    #[test]
    fn migrated_child_keeps_its_own_calls_and_drops_the_parents() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_parent(temp.path());
        write_lines(
            &temp.path().join("rollout-2026-06-01T01-00-00-c1.jsonl"),
            &migrated_child("2026-06-01T01:00:00Z"),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 3 + 2);
        assert_eq!(total_volume(&collection), 110 + 140 + 220 + 66 + 130);
        let own = collection
            .usage_events
            .iter()
            .find(|event| event.usage.token_volume() == 66)
            .expect("the child's own call counts");
        assert_eq!(own.session_id.as_deref(), Some("p1"));
    }

    /// Without the parent nothing tells the copies apart, so the window
    /// stands. A child that kept its timestamps, or has records, was told
    /// apart by the window already.
    #[test]
    fn the_window_stands_without_the_parent_or_for_other_forks() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_lines(
            &temp.path().join("rollout-2026-06-01T01-00-00-c1.jsonl"),
            &migrated_child("2026-06-01T01:00:00Z"),
        );
        let alone = collect(temp.path(), None, false, UtcOffset::UTC);
        assert!(alone.usage_events.is_empty());

        write_parent(temp.path());
        let mut kept_time = migrated_child("2026-06-01T02:00:00Z");
        kept_time[0] = meta_line(
            "2026-06-01T02:00:00Z",
            "c2",
            SPAWN,
            r#","forked_from_id":"p1""#,
        );
        kept_time.push(token_count_line(
            "2026-06-01T02:00:30Z",
            &usage_json(70, 0, 7),
            523,
        ));
        write_lines(
            &temp.path().join("rollout-2026-06-01T02-00-00-c2.jsonl"),
            &kept_time,
        );
        let mut with_record = migrated_child("2026-06-01T03:00:00Z");
        with_record[0] = meta_line(
            "2026-06-01T03:00:00Z",
            "c3",
            SPAWN,
            r#","forked_from_id":"p1""#,
        );
        with_record.push(record_line(
            "2026-06-01T03:00:00Z",
            "c3",
            "resp_c3",
            &usage_json(80, 0, 8),
        ));
        write_lines(
            &temp.path().join("rollout-2026-06-01T03-00-00-c3.jsonl"),
            &with_record,
        );
        std::fs::remove_file(temp.path().join("rollout-2026-06-01T01-00-00-c1.jsonl"))
            .expect("fixture should be removed");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110 + 140 + 220 + 77 + 88);
    }

    #[test]
    fn held_keys_round_trip_any_parent_id() {
        let mut held = HeldRows::for_fork_of(Some("a:b|c".to_owned()));
        held.rows.push((
            "1:2:3:4:5:6:7:8:9:10:11:12".to_owned(),
            KeyedUsageEvent {
                key: Some("codex-usage:v2:x:y".to_owned()),
                event: crate::model::UsageEvent {
                    timestamp: None,
                    session_id: None,
                    model: None,
                    source_kind: crate::model::SourceKind::Subagent,
                    attribution_agent: None,
                    attribution_skill: None,
                    project: None,
                    usage: crate::model::TokenUsage::default(),
                    granularity: crate::model::UsageGranularity::Call,
                    billing: crate::model::Billing::default(),
                },
            },
        ));
        let mut events = FileEvents::default();
        held.finish(&mut events);

        let key = events.usage_events[0].key.as_deref().expect("held key");
        let parsed = parse_held(key).expect("held key parses");
        assert_eq!(parsed.parent, "a:b|c");
        assert_eq!(parsed.pair, "1:2:3:4:5:6:7:8:9:10:11:12");
        assert_eq!(parsed.key, "codex-usage:v2:x:y");
        assert!(parse_held("codex-usage:v2:x:y").is_none());
    }
}
