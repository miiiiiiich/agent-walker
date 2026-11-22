use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use tempfile::TempDir;
use time::macros::datetime;
use time::{Date, OffsetDateTime, UtcOffset};

use super::spread::apportion;
use crate::collector::claude::collect;
use crate::model::{Collection, SourceKind, TokenUsage, UsageEvent, UsageGranularity};

const NOW: OffsetDateTime = datetime!(2026-06-08 12:00 UTC);

fn assistant(at: &str, session: &str, id: &str, model: &str, input: u64, reads: u64) -> String {
    format!(
        r#"{{"timestamp":"{at}","sessionId":"{session}","type":"assistant","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":1,"cache_read_input_tokens":{reads}}}}}}}"#
    )
}

fn user(at: &str, session: &str, uuid: &str) -> String {
    format!(
        r#"{{"timestamp":"{at}","sessionId":"{session}","type":"user","uuid":"{uuid}","message":{{"role":"user","content":"go"}}}}"#
    )
}

/// `models`: (key, input, output, cache write, cache read, 5m, 1h).
fn cost_state(
    session: &str,
    start: i64,
    models: &[(&str, u64, u64, u64, u64, u64, u64)],
) -> String {
    let entries = models
        .iter()
        .map(|(model, input, output, writes, reads, short, long)| {
            format!(
                r#""{model}":{{"inputTokens":{input},"outputTokens":{output},"cacheCreationInputTokens":{writes},"cacheReadInputTokens":{reads},"cacheCreationEphemeral5mInputTokens":{short},"cacheCreationEphemeral1hInputTokens":{long},"costUSD":0.1}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"type":"cost-state","sessionId":"{session}","startTime":{start},"totalCostUSD":0.1,"modelUsage":{{{entries}}}}}"#
    )
}

fn write(dir: &Path, name: &str, lines: &[String]) {
    fs::create_dir_all(dir).expect("test dirs should be created");
    fs::write(dir.join(name), lines.join("\n")).expect("fixture should be written");
}

fn added(collection: &Collection) -> Vec<&UsageEvent> {
    collection
        .usage_events
        .iter()
        .filter(|event| event.granularity == UsageGranularity::Remainder)
        .collect()
}

fn added_by_day(collection: &Collection) -> BTreeMap<(Date, String), TokenUsage> {
    let mut days: BTreeMap<(Date, String), TokenUsage> = BTreeMap::new();
    for event in added(collection) {
        let date = event.timestamp.expect("dated").date();
        days.entry((date, event.model.clone().expect("model")))
            .or_default()
            .add_assign(&event.usage);
    }
    days
}

fn total(collection: &Collection, model: &str) -> TokenUsage {
    let mut usage = TokenUsage::default();
    for event in added(collection) {
        if event.model.as_deref() == Some(model) {
            usage.add_assign(&event.usage);
        }
    }
    usage
}

/// Only the part of each field over the transcript is added, and a field the
/// transcript is ahead on adds nothing. A model with no row of its own is
/// added whole. The 1M-context key is its plain model.
#[test]
fn adds_the_part_of_each_field_over_the_transcript() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "s1.jsonl",
        &[
            user("2026-06-07T10:00:00Z", "s1", "u1"),
            assistant("2026-06-07T10:01:00Z", "s1", "m1", "claude-opus-5", 10, 100),
            assistant("2026-06-07T10:02:00Z", "s1", "m2", "claude-opus-5", 10, 100),
            cost_state(
                "s1",
                1,
                &[
                    ("claude-opus-5", 15, 1, 0, 150, 0, 0),
                    ("claude-opus-5[1m]", 0, 0, 0, 100, 0, 0),
                    ("claude-haiku-5-5", 30, 3, 0, 0, 0, 0),
                ],
            ),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    let opus = total(&collection, "claude-opus-5");
    assert_eq!(
        (
            opus.input_tokens,
            opus.output_tokens,
            opus.cache_read_input_tokens
        ),
        (0, 0, 50)
    );
    let haiku = total(&collection, "claude-haiku-5-5");
    assert_eq!((haiku.input_tokens, haiku.output_tokens), (30, 3));
    assert_eq!(total(&collection, "claude-opus-5[1m]").token_volume(), 0);
    assert!(added(&collection).iter().all(|event| {
        event.source_kind == SourceKind::Main && event.session_id.as_deref() == Some("s1")
    }));
    assert_eq!(collection.stats.usage_events, collection.usage_events.len());
}

/// A session's excess lands on the days it ran, in proportion to its volume
/// there, stamped at a row it already has: no new day, session or span.
#[test]
fn excess_is_spread_over_the_sessions_days_by_volume() {
    let temp = TempDir::new().expect("tempdir");
    let lines = [
        user("2026-06-05T09:00:00Z", "s1", "u1"),
        assistant("2026-06-05T09:01:00Z", "s1", "m1", "claude-opus-5", 0, 299),
        user("2026-06-06T20:00:00Z", "s1", "u2"),
        assistant("2026-06-06T20:01:00Z", "s1", "m2", "claude-opus-5", 0, 99),
    ];
    write(&temp.path().join("plain"), "s1.jsonl", &lines);
    write(
        &temp.path().join("plain"),
        "s2.jsonl",
        &[
            user("2026-06-07T08:00:00Z", "s2", "v1"),
            assistant("2026-06-07T08:01:00Z", "s2", "n1", "claude-opus-5", 0, 10),
        ],
    );
    let mut with_total = lines.to_vec();
    with_total.push(cost_state(
        "s1",
        1,
        &[("claude-opus-5", 0, 2, 0, 398 + 400, 0, 0)],
    ));
    write(&temp.path().join("totalled"), "s1.jsonl", &with_total);
    write(
        &temp.path().join("totalled"),
        "s2.jsonl",
        &[
            user("2026-06-07T08:00:00Z", "s2", "v1"),
            assistant("2026-06-07T08:01:00Z", "s2", "n1", "claude-opus-5", 0, 10),
        ],
    );

    let plain = collect(&temp.path().join("plain"), None, false, UtcOffset::UTC);
    let totalled = collect(&temp.path().join("totalled"), None, false, UtcOffset::UTC);

    let days = added_by_day(&totalled);
    let reads = |date: Date| days[&(date, "claude-opus-5".to_owned())].cache_read_input_tokens;
    assert_eq!(days.len(), 2);
    assert_eq!(reads(time::macros::date!(2026 - 06 - 05)), 300);
    assert_eq!(reads(time::macros::date!(2026 - 06 - 06)), 100);
    let row_stamps: Vec<OffsetDateTime> = plain
        .usage_events
        .iter()
        .filter_map(|event| event.timestamp)
        .collect();
    assert!(
        added(&totalled)
            .iter()
            .all(|event| row_stamps.contains(&event.timestamp.expect("dated")))
    );

    let before = crate::analyzer::summarize(&plain, NOW, 7, UtcOffset::UTC);
    let after = crate::analyzer::summarize(&totalled, NOW, 7, UtcOffset::UTC);
    assert_eq!(
        after.total_usage.token_volume(),
        before.total_usage.token_volume() + 400
    );
    let calls = |summary: &crate::model::Summary| {
        summary
            .models
            .iter()
            .map(|model| (model.name.clone(), model.events))
            .collect::<Vec<_>>()
    };
    assert_eq!(calls(&after), calls(&before));
    assert_eq!(after.sessions, before.sessions);
    assert_eq!(after.active_days, before.active_days);
    assert_eq!(
        after.recent_window_active_days,
        before.recent_window_active_days
    );
    let span = |summary: &crate::model::Summary| {
        summary
            .longest_session
            .as_ref()
            .map(|span| (span.started_at, span.ended_at))
    };
    assert_eq!(span(&after), span(&before));
    assert_eq!(
        after.active_time.map(|time| time.turns),
        before.active_time.map(|time| time.turns)
    );
}

/// Within a day the excess follows the session's hours, not the day's last
/// call.
#[test]
fn excess_follows_the_sessions_hours() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant("2026-06-07T09:00:00Z", "s1", "m1", "claude-opus-5", 0, 299),
            assistant("2026-06-07T09:30:00Z", "s1", "m2", "claude-opus-5", 0, 99),
            assistant("2026-06-07T23:10:00Z", "s1", "m3", "claude-opus-5", 0, 99),
            cost_state("s1", 1, &[("claude-opus-5", 0, 3, 0, 497 + 500, 0, 0)]),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    let hourly = |hour: u8| {
        added(&collection)
            .iter()
            .filter(|event| event.timestamp.expect("dated").hour() == hour)
            .map(|event| event.usage.cache_read_input_tokens)
            .sum::<u64>()
    };
    assert_eq!((hourly(9), hourly(23)), (400, 100));
    let nine = added(&collection)
        .into_iter()
        .find(|event| event.timestamp.expect("dated").hour() == 9)
        .expect("a row at 9");
    assert_eq!(nine.timestamp, Some(datetime!(2026-06-07 09:30 UTC)));
}

/// A later process that carries the previous total forward is one run: its
/// last total against every call. Splitting at the process would count the
/// carried part once per process.
#[test]
fn a_carried_total_is_compared_once() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s1", "m1", "claude-opus-5", 0, 100),
            cost_state("s1", 1, &[("claude-opus-5", 0, 1, 0, 130, 0, 0)]),
            assistant("2026-06-06T09:00:00Z", "s1", "m2", "claude-opus-5", 0, 100),
            cost_state("s1", 2, &[("claude-opus-5", 0, 2, 0, 250, 0, 0)]),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    assert_eq!(
        total(&collection, "claude-opus-5").cache_read_input_tokens,
        50
    );
}

/// A process that starts again from zero closes a run: each run's total is
/// compared with the calls made before it was written, and its excess lands
/// on that run's days.
#[test]
fn a_restarted_total_compares_each_run_with_its_own_calls() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s1", "m1", "claude-opus-5", 0, 100),
            cost_state("s1", 1, &[("claude-opus-5", 0, 1, 0, 130, 0, 0)]),
            assistant("2026-06-06T09:00:00Z", "s1", "m2", "claude-opus-5", 0, 50),
            cost_state("s1", 2, &[("claude-opus-5", 0, 1, 0, 60, 0, 0)]),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    let days = added_by_day(&collection);
    let reads = |date: Date| {
        days.get(&(date, "claude-opus-5".to_owned()))
            .map_or(0, |usage| usage.cache_read_input_tokens)
    };
    assert_eq!(reads(time::macros::date!(2026 - 06 - 05)), 30);
    assert_eq!(reads(time::macros::date!(2026 - 06 - 06)), 10);
}

/// A new process whose total is larger but has lost a model's counter has
/// started again: the earlier run keeps its own excess.
#[test]
fn a_restart_that_drops_a_model_is_a_new_run_though_larger() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant(
                "2026-06-05T09:00:00Z",
                "s1",
                "m1",
                "claude-haiku-5-5",
                0,
                80,
            ),
            cost_state("s1", 1, &[("claude-haiku-5-5", 0, 1, 0, 100, 0, 0)]),
            assistant("2026-06-06T09:00:00Z", "s1", "m2", "claude-opus-5", 0, 150),
            cost_state("s1", 2, &[("claude-opus-5", 0, 1, 0, 200, 0, 0)]),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    let reads = |model: &str| total(&collection, model).cache_read_input_tokens;
    assert_eq!(
        (reads("claude-haiku-5-5"), reads("claude-opus-5")),
        (20, 50)
    );
}

/// Calls after the last total were made by a process that recorded none:
/// the total is compared with the calls before it only, and they add nothing.
#[test]
fn a_stale_total_is_compared_with_the_calls_before_it() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s1", "m1", "claude-opus-5", 0, 100),
            cost_state("s1", 1, &[("claude-opus-5", 0, 1, 0, 130, 0, 0)]),
            user("2026-06-06T09:00:00Z", "s1", "u2"),
            assistant("2026-06-06T09:01:00Z", "s1", "m2", "claude-opus-5", 0, 500),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    let days = added_by_day(&collection);
    assert_eq!(days.len(), 1);
    assert_eq!(
        days[&(
            time::macros::date!(2026 - 06 - 05),
            "claude-opus-5".to_owned()
        )]
            .cache_read_input_tokens,
        30
    );
}

/// A subagent's calls and a reply's advisor call are in the totals; a call
/// copied into another session counts in each session's own comparison.
#[test]
fn subagent_advisor_and_shared_calls_count_on_the_transcript_side() {
    let temp = TempDir::new().expect("tempdir");
    let advisor = r#"{"timestamp":"2026-06-05T09:02:00Z","sessionId":"s1","type":"assistant","message":{"id":"m3","model":"claude-opus-5","usage":{"input_tokens":2,"output_tokens":1,"iterations":[{"type":"message","input_tokens":2,"output_tokens":1},{"type":"advisor_message","model":"claude-fable-5-2","input_tokens":7,"output_tokens":4}]}}}"#;
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s1", "m1", "claude-opus-5", 0, 100),
            advisor.to_owned(),
            cost_state(
                "s1",
                1,
                &[
                    ("claude-opus-5", 2, 2, 0, 100, 0, 0),
                    ("claude-haiku-5-5", 5, 1, 0, 50, 0, 0),
                    ("claude-fable-5-2", 7, 4, 0, 0, 0, 0),
                ],
            ),
        ],
    );
    write(
        &temp.path().join("s1").join("subagents"),
        "agent-a1.jsonl",
        &[assistant(
            "2026-06-05T09:01:00Z",
            "s1",
            "m2",
            "claude-haiku-5-5",
            5,
            50,
        )],
    );
    write(
        temp.path(),
        "s2.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s2", "m1", "claude-opus-5", 0, 100),
            cost_state("s2", 1, &[("claude-opus-5", 0, 1, 0, 100, 0, 0)]),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    assert!(added(&collection).is_empty());
}

/// A fork restores its parent's total and counts on from it, copying only
/// some of the parent's calls: the parent's part is not counted again.
#[test]
fn a_forks_carried_total_is_not_counted_again() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "parent.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "pa", "m0", "claude-opus-5", 0, 400),
            assistant("2026-06-05T09:10:00Z", "pa", "m1", "claude-opus-5", 0, 100),
            cost_state("pa", 7, &[("claude-opus-5", 0, 2, 0, 530, 0, 0)]),
        ],
    );
    write(
        temp.path(),
        "fork.jsonl",
        &[
            assistant("2026-06-06T09:00:00Z", "fk", "m1", "claude-opus-5", 0, 100),
            assistant("2026-06-06T09:05:00Z", "fk", "m9", "claude-opus-5", 0, 50),
            cost_state("fk", 7, &[("claude-opus-5", 0, 3, 0, 530 + 50 + 20, 0, 0)]),
        ],
    );
    write(
        temp.path(),
        "alongside.jsonl",
        &[
            assistant("2026-06-06T10:00:00Z", "al", "n1", "claude-opus-5", 0, 100),
            cost_state("al", 7, &[("claude-opus-5", 0, 1, 0, 110, 0, 0)]),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    let reads = |session: &str| {
        added(&collection)
            .iter()
            .filter(|event| event.session_id.as_deref() == Some(session))
            .map(|event| event.usage.cache_read_input_tokens)
            .sum::<u64>()
    };
    assert_eq!((reads("pa"), reads("fk"), reads("al")), (30, 20, 10));
}

/// A copy of a call the parent made after its last total is not in the
/// carried part: it stays on the fork's side, or it would count twice.
#[test]
fn a_forks_copy_of_a_call_after_the_parents_total_stays_counted_once() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "parent.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "pa", "m0", "claude-opus-5", 0, 400),
            cost_state("pa", 7, &[("claude-opus-5", 0, 1, 0, 430, 0, 0)]),
            assistant("2026-06-05T09:10:00Z", "pa", "m1", "claude-opus-5", 0, 100),
        ],
    );
    write(
        temp.path(),
        "fork.jsonl",
        &[
            assistant("2026-06-06T09:00:00Z", "fk", "m1", "claude-opus-5", 0, 100),
            assistant("2026-06-06T09:05:00Z", "fk", "m9", "claude-opus-5", 0, 50),
            cost_state(
                "fk",
                7,
                &[("claude-opus-5", 0, 3, 0, 430 + 100 + 50 + 20, 0, 0)],
            ),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    let reads = |session: &str| {
        added(&collection)
            .iter()
            .filter(|event| event.session_id.as_deref() == Some(session))
            .map(|event| event.usage.cache_read_input_tokens)
            .sum::<u64>()
    };
    assert_eq!((reads("pa"), reads("fk")), (30, 20));
}

fn reads_of(collection: &Collection, session: &str) -> u64 {
    added(collection)
        .iter()
        .filter(|event| event.session_id.as_deref() == Some(session))
        .map(|event| event.usage.cache_read_input_tokens)
        .sum()
}

/// `pa` makes m0 and m1; `fk` forks it a day later, copying only m1, and
/// makes m9 of its own.
fn parent_and_fork(dir: &Path) {
    write(
        dir,
        "parent.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "pa", "m0", "claude-opus-5", 0, 400),
            assistant("2026-06-05T09:10:00Z", "pa", "m1", "claude-opus-5", 0, 100),
            cost_state("pa", 7, &[("claude-opus-5", 0, 2, 0, 530, 0, 0)]),
        ],
    );
    write(
        dir,
        "fork.jsonl",
        &[
            assistant("2026-06-06T09:00:00Z", "fk", "m1", "claude-opus-5", 0, 100),
            assistant("2026-06-06T09:05:00Z", "fk", "m9", "claude-opus-5", 0, 50),
            cost_state("fk", 7, &[("claude-opus-5", 0, 3, 0, 530 + 50 + 20, 0, 0)]),
        ],
    );
}

/// A sibling forked earlier holds the parent's total plus its own calls: it
/// is not the later fork's parent, though its total is larger, whether or
/// not the forks copied all of the parent's calls.
#[test]
fn a_sibling_fork_is_not_taken_for_the_parent() {
    let temp = TempDir::new().expect("tempdir");
    let partial = temp.path().join("partial");
    parent_and_fork(&partial);
    write(
        &partial,
        "sibling.jsonl",
        &[
            assistant("2026-06-07T09:00:00Z", "sb", "m1", "claude-opus-5", 0, 100),
            assistant("2026-06-07T09:05:00Z", "sb", "b9", "claude-opus-5", 0, 80),
            cost_state("sb", 7, &[("claude-opus-5", 0, 3, 0, 530 + 80 + 15, 0, 0)]),
        ],
    );
    let whole = temp.path().join("whole");
    write(
        &whole,
        "parent.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "pa", "m1", "claude-opus-5", 0, 100),
            cost_state("pa", 3, &[("claude-opus-5", 0, 1, 0, 150, 0, 0)]),
        ],
    );
    for (session, day, own, total) in [("f1", 6, 20, 180), ("f2", 7, 40, 195)] {
        write(
            &whole,
            &format!("{session}.jsonl"),
            &[
                assistant(
                    &format!("2026-06-0{day}T09:00:00Z"),
                    session,
                    "m1",
                    "claude-opus-5",
                    0,
                    100,
                ),
                assistant(
                    &format!("2026-06-0{day}T09:05:00Z"),
                    session,
                    "o1",
                    "claude-opus-5",
                    0,
                    own,
                ),
                cost_state(session, 3, &[("claude-opus-5", 0, 2, 0, total, 0, 0)]),
            ],
        );
    }

    let partial = collect(&partial, None, false, UtcOffset::UTC);
    let whole = collect(&whole, None, false, UtcOffset::UTC);

    let reads = |collection: &Collection, sessions: [&str; 3]| {
        sessions.map(|session| reads_of(collection, session))
    };
    assert_eq!(reads(&partial, ["pa", "fk", "sb"]), [30, 20, 15]);
    assert_eq!(reads(&whole, ["pa", "f1", "f2"]), [50, 10, 5]);
}

/// A fork of a fork is compared with the fork it was made from, the nearest
/// of the two sessions it shares calls with.
#[test]
fn a_fork_of_a_fork_is_compared_with_the_nearer_fork() {
    let temp = TempDir::new().expect("tempdir");
    parent_and_fork(temp.path());
    write(
        temp.path(),
        "grandchild.jsonl",
        &[
            assistant("2026-06-07T09:00:00Z", "gc", "m1", "claude-opus-5", 0, 100),
            assistant("2026-06-07T09:01:00Z", "gc", "m9", "claude-opus-5", 0, 50),
            assistant("2026-06-07T09:05:00Z", "gc", "g9", "claude-opus-5", 0, 30),
            cost_state("gc", 7, &[("claude-opus-5", 0, 4, 0, 600 + 30 + 5, 0, 0)]),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    assert_eq!(reads_of(&collection, "gc"), 5);
}

/// Two sessions the fork could have come from, equal in every respect but
/// how their totals split by model: the choice must not follow map order.
#[test]
fn tied_parents_are_chosen_the_same_way_every_time() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "a.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "pa", "m1", "claude-opus-5", 0, 100),
            cost_state("pa", 7, &[("claude-opus-5", 0, 1, 0, 149, 0, 0)]),
        ],
    );
    write(
        temp.path(),
        "b.jsonl",
        &[
            assistant("2026-06-05T10:00:00Z", "pb", "m1", "claude-opus-5", 0, 100),
            cost_state(
                "pb",
                7,
                &[
                    ("claude-opus-5", 0, 1, 0, 99, 0, 0),
                    ("claude-haiku-5-5", 50, 0, 0, 0, 0, 0),
                ],
            ),
        ],
    );
    write(
        temp.path(),
        "fork.jsonl",
        &[
            assistant("2026-06-06T09:00:00Z", "fk", "m1", "claude-opus-5", 0, 100),
            assistant("2026-06-06T09:05:00Z", "fk", "m9", "claude-opus-5", 0, 50),
            cost_state("fk", 7, &[("claude-opus-5", 0, 2, 0, 149 + 50 + 20, 0, 0)]),
        ],
    );

    let runs: Vec<u64> = (0..32)
        .map(|_| reads_of(&collect(temp.path(), None, false, UtcOffset::UTC), "fk"))
        .collect();

    assert_eq!(runs, vec![20; 32]);
}

/// A streamed reply whose larger copy does not name its model is still that
/// model's call, as the collection merge reads it, in either copy order.
#[test]
fn a_streamed_copy_without_a_model_counts_under_the_other_copys_model() {
    let temp = TempDir::new().expect("tempdir");
    let unnamed = |at: &str, session: &str| {
        format!(
            r#"{{"timestamp":"{at}","sessionId":"{session}","type":"assistant","message":{{"id":"m1","usage":{{"input_tokens":99,"output_tokens":1}}}}}}"#
        )
    };
    let total = |session: &str| cost_state(session, 1, &[("claude-opus-5", 99, 1, 0, 0, 0, 0)]);
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s1", "m1", "claude-opus-5", 9, 0),
            unnamed("2026-06-05T09:00:01Z", "s1"),
            total("s1"),
        ],
    );
    write(
        temp.path(),
        "s2.jsonl",
        &[
            unnamed("2026-06-05T09:00:00Z", "s2"),
            assistant("2026-06-05T09:00:01Z", "s2", "m1", "claude-opus-5", 9, 0),
            total("s2"),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    assert!(added(&collection).is_empty());
}

/// Totals are cumulative: of two files holding one session's totals, only
/// the one written last is used.
#[test]
fn a_session_totalled_in_two_files_is_compared_once() {
    let temp = TempDir::new().expect("tempdir");
    let rows = [
        assistant("2026-06-05T09:00:00Z", "s1", "m1", "claude-opus-5", 0, 100),
        cost_state("s1", 1, &[("claude-opus-5", 0, 1, 0, 120, 0, 0)]),
    ];
    write(&temp.path().join("a"), "s1.jsonl", &rows);
    let mut later = rows.to_vec();
    later.push(assistant(
        "2026-06-05T10:00:00Z",
        "s1",
        "m2",
        "claude-opus-5",
        0,
        100,
    ));
    later.push(cost_state(
        "s1",
        1,
        &[("claude-opus-5", 0, 2, 0, 230, 0, 0)],
    ));
    write(&temp.path().join("b"), "s1.jsonl", &later);

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    assert_eq!(
        total(&collection, "claude-opus-5").cache_read_input_tokens,
        30
    );
}

/// A long session's subagent file older than the mtime floor still counts
/// against its total, or its calls read as uncounted on a short window.
#[test]
fn a_long_sessions_old_subagent_file_is_compared_too() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s1", "m1", "claude-opus-5", 0, 100),
            cost_state(
                "s1",
                1,
                &[
                    ("claude-opus-5", 0, 1, 0, 100, 0, 0),
                    ("claude-haiku-5-5", 5, 1, 0, 50, 0, 0),
                ],
            ),
        ],
    );
    let subagents = temp.path().join("s1").join("subagents");
    write(
        &subagents,
        "agent-a1.jsonl",
        &[assistant(
            "2026-06-05T09:01:00Z",
            "s1",
            "m2",
            "claude-haiku-5-5",
            5,
            50,
        )],
    );
    let old = std::time::SystemTime::now() - std::time::Duration::from_hours(48);
    fs::File::options()
        .write(true)
        .open(subagents.join("agent-a1.jsonl"))
        .and_then(|file| file.set_modified(old))
        .expect("mtime should be set");
    let floor = std::time::SystemTime::now() - std::time::Duration::from_hours(24);

    let collection = collect(temp.path(), Some(floor), false, UtcOffset::UTC);

    assert!(added(&collection).is_empty());
}

/// Every added row keeps its 5m and 1h writes within its writes, or the whole
/// write prices at the 5m rate.
#[test]
fn the_cache_write_split_stays_within_each_rows_writes() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s1", "m1", "claude-opus-5", 0, 100),
            assistant("2026-06-06T09:00:00Z", "s1", "m2", "claude-opus-5", 0, 100),
            assistant("2026-06-07T09:00:00Z", "s1", "m3", "claude-opus-5", 0, 100),
            cost_state("s1", 1, &[("claude-opus-5", 0, 3, 100, 300, 70, 80)]),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    let opus = total(&collection, "claude-opus-5");
    assert_eq!(opus.cache_creation_input_tokens, 100);
    assert_eq!(opus.cache_creation_ephemeral_1h_input_tokens, 80);
    assert_eq!(opus.cache_creation_ephemeral_5m_input_tokens, 20);
    assert!(added(&collection).iter().all(|event| {
        event.usage.cache_creation_ephemeral_5m_input_tokens
            + event.usage.cache_creation_ephemeral_1h_input_tokens
            <= event.usage.cache_creation_input_tokens
    }));
}

/// A total is spread per model over every hour its session ran, so a row
/// naming more models than any process uses is a parse error, not a total.
#[test]
fn a_total_naming_too_many_models_is_a_parse_error() {
    let temp = TempDir::new().expect("tempdir");
    let names = |count: usize| (0..count).map(|i| format!("model-{i}")).collect::<Vec<_>>();
    let row = |session: &str, names: &[String]| {
        let models: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), 1, 0, 0, 0, 0, 0))
            .collect();
        cost_state(session, 1, &models)
    };
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s1", "m1", "model-0", 1, 1),
            row("s1", &names(64)),
        ],
    );
    write(
        temp.path(),
        "s2.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s2", "m2", "model-0", 1, 1),
            row("s2", &names(65)),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    assert_eq!(collection.stats.parse_errors, 1);
    let sessions = |session: &str| {
        added(&collection)
            .iter()
            .filter(|event| event.session_id.as_deref() == Some(session))
            .count()
    };
    assert_eq!((sessions("s1"), sessions("s2")), (63, 0));
}

/// The observer's own totals count like its calls: usage, never activity.
#[test]
fn a_background_sessions_excess_stays_background() {
    let temp = TempDir::new().expect("tempdir");
    write(
        &temp.path().join("-home-claude-mem-observer-sessions"),
        "obs.jsonl",
        &[
            assistant(
                "2026-06-05T09:00:00Z",
                "obs",
                "m1",
                "claude-haiku-5-5",
                0,
                100,
            ),
            cost_state("obs", 1, &[("claude-haiku-5-5", 0, 1, 0, 150, 0, 0)]),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    let rows = added(&collection);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source_kind, SourceKind::Background);
    assert_eq!(rows[0].usage.cache_read_input_tokens, 50);
}

/// A session with no dated call has no day to land on and adds nothing.
#[test]
fn a_total_without_dated_calls_adds_nothing() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "s1.jsonl",
        &[
            user("2026-06-05T09:00:00Z", "s1", "u1"),
            cost_state("s1", 1, &[("claude-haiku-5-5", 10, 1, 0, 0, 0, 0)]),
        ],
    );

    let collection = collect(temp.path(), None, false, UtcOffset::UTC);

    assert!(added(&collection).is_empty());
}

/// The parse cache stores each file's totals; read back, they add the same.
#[test]
fn totals_survive_the_parse_cache() {
    let temp = TempDir::new().expect("tempdir");
    write(
        temp.path(),
        "s1.jsonl",
        &[
            assistant("2026-06-05T09:00:00Z", "s1", "m1", "claude-opus-5", 0, 100),
            cost_state("s1", 1, &[("claude-opus-5", 0, 1, 40, 130, 10, 30)]),
            assistant("2026-06-05T09:05:00Z", "s1", "m2", "claude-opus-5", 0, 100),
        ],
    );
    let path = temp.path().join("s1.jsonl");
    let parsed = super::super::parse_file(&path, temp.path(), UtcOffset::UTC).expect("parsed");
    let bytes = postcard::to_stdvec(&parsed).expect("encoded");
    let decoded: crate::collector::FileEvents = postcard::from_bytes(&bytes).expect("decoded");

    let added = |events| {
        super::uncounted_calls(&[(path.clone(), Some(events))], UtcOffset::UTC)
            .into_iter()
            .map(|event| event.usage)
            .collect::<Vec<_>>()
    };
    let direct = added(parsed);
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].cache_read_input_tokens, 30);
    assert_eq!(direct, added(decoded));
}

#[test]
fn apportion_adds_up_and_gives_remainders_to_the_largest_fractions() {
    assert_eq!(apportion(10, &[1, 1, 1]), vec![4, 3, 3]);
    assert_eq!(apportion(7, &[0, 5, 2]), vec![0, 5, 2]);
    assert_eq!(apportion(1, &[1, 3]), vec![0, 1]);
    let shares = apportion(u64::MAX, &[u64::MAX, u64::MAX, 1]);
    assert_eq!(
        shares
            .iter()
            .fold(0_u64, |sum, share| sum.saturating_add(*share)),
        u64::MAX
    );
}
