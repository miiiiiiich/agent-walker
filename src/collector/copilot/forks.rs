use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;
use time::OffsetDateTime;

use crate::collector::FileEvents;
use crate::collector::ident::ident;
use crate::model::UsageGranularity;

use super::receipts::source_of;
use super::summaries;

/// Each fork's session and the time of its fork: from the fork's own
/// marker, or failing that the parent's.
pub(super) struct Forks(HashMap<String, OffsetDateTime>);

impl Forks {
    pub(super) fn read(per_file: &[(PathBuf, Option<FileEvents>)]) -> Self {
        let mut own: HashMap<String, OffsetDateTime> = HashMap::new();
        let mut named: HashMap<String, OffsetDateTime> = HashMap::new();
        for summary in summaries(per_file) {
            for (forks, found) in [
                (&mut own, &summary.forks),
                (&mut named, &summary.forked_into),
            ] {
                for (session, time) in found {
                    forks
                        .entry(session.clone())
                        .and_modify(|at| *at = (*at).max(*time))
                        .or_insert(*time);
                }
            }
        }
        for (fork, at) in named {
            own.entry(fork).or_insert(at);
        }
        Self(own)
    }

    /// A session can't have used anything before it existed: a fork's
    /// record dated up to its fork is a copy of its parent's history,
    /// whether or not a marker named the parent. The store's rows and the
    /// exit marks get the same cut.
    pub(super) fn copied(&self, session: &str, time: OffsetDateTime) -> bool {
        self.0.get(session).is_some_and(|fork| time <= *fork)
    }

    /// Only a receipt naming another session as its source stays, for that
    /// source to decide.
    pub(super) fn drop_copies(&self, per_file: &mut [(PathBuf, Option<FileEvents>)]) {
        if self.0.is_empty() {
            return;
        }
        for events in per_file
            .iter_mut()
            .filter_map(|(_, events)| events.as_mut())
        {
            events.usage_events.retain(|keyed| {
                let (Some(session), Some(time)) =
                    (keyed.event.session_id.as_deref(), keyed.event.timestamp)
                else {
                    return true;
                };
                let named_elsewhere = keyed.event.granularity == UsageGranularity::Call
                    && keyed
                        .key
                        .as_deref()
                        .and_then(source_of)
                        .is_some_and(|source| source != session);
                named_elsewhere || !self.copied(session, time)
            });
        }
    }
}

/// A fork's journal opens with a copy of its parent's history, event ids
/// and all, then a `session.info` of `infoType` "fork" reading "Forked from
/// <parent> [before event <id>] as <fork>.", each session named by its id
/// or as `"<name>" (<id>)`. The parent's own marker reads "Forked this
/// session into …" and isn't one.
pub(super) struct ForkMarker {
    /// `None` when the marker doesn't name it in a form read here.
    pub(super) parent: Option<String>,
    /// The fork itself, likewise.
    pub(super) child: Option<String>,
}

fn fork_message(value: &Value) -> Option<&str> {
    if value.get("type").and_then(Value::as_str) != Some("session.info") {
        return None;
    }
    let data = value.get("data")?;
    if data.get("infoType").and_then(Value::as_str) != Some("fork") {
        return None;
    }
    data.get("message")?.as_str()
}

/// The fork a parent's own marker, "Forked this session into <fork>
/// [before event <id>].", names. Its history stays the parent's.
pub(super) fn forked_into(value: &Value) -> Option<String> {
    let named = fork_message(value)?.strip_prefix("Forked this session into ")?;
    session_ref(named).map(|(id, _)| id.to_owned())
}

pub(super) fn fork_marker(value: &Value) -> Option<ForkMarker> {
    let named = fork_message(value)?.strip_prefix("Forked from ")?;
    let parent = session_ref(named);
    let child = parent.and_then(|(_, rest)| {
        let rest = match rest.strip_prefix(" before event ") {
            Some(event) => &event[event.find(' ')?..],
            None => rest,
        };
        session_ref(rest.strip_prefix(" as ")?)
    });
    Some(ForkMarker {
        parent: parent.map(|(id, _)| id.to_owned()),
        child: child.map(|(id, _)| id.to_owned()),
    })
}

/// The session id a reference starting `text` names, and the text after it.
/// A name is a JSON string, so it is parsed rather than split: it may hold
/// " as " or quotes.
fn session_ref(text: &str) -> Option<(&str, &str)> {
    let (id, rest) = if text.starts_with('"') {
        let mut values = serde_json::Deserializer::from_str(text).into_iter::<String>();
        values.next()?.ok()?;
        let after = text.get(values.byte_offset()..)?.strip_prefix(" (")?;
        let end = after.find(')')?;
        (&after[..end], &after[end + 1..])
    } else {
        let end = text.find([' ', '.']).unwrap_or(text.len());
        (&text[..end], &text[end..])
    };
    let valid = !id.is_empty() && !id.contains(char::is_whitespace);
    Some((ident(id).filter(|_| valid)?, rest))
}

#[cfg(test)]
pub(in crate::collector::copilot) mod tests {
    use serde_json::json;
    use tempfile::TempDir;
    use time::UtcOffset;

    use super::*;
    use crate::collector::copilot::collect;
    use crate::collector::copilot::ledger::tests::{call, write_store};
    use crate::collector::copilot::tests::{shutdown, total_volume, write_session};

    /// A fork's `session.start`, rewritten by Copilot to the fork's id and
    /// the time of the fork.
    pub(in crate::collector::copilot) fn fork_start(session: &str, at: &str) -> String {
        json!({
            "type": "session.start",
            "id": format!("st-{session}"),
            "timestamp": at,
            "data": {"sessionId": session, "startTime": at, "context": {"cwd": "/Users/me/code/app"}},
        })
        .to_string()
    }

    pub(in crate::collector::copilot) fn fork_info(id: &str, at: &str, message: &str) -> String {
        json!({
            "type": "session.info",
            "id": id,
            "timestamp": at,
            "data": {"infoType": "fork", "message": message},
        })
        .to_string()
    }

    fn lines(lines: &[String]) -> String {
        lines.join("\n") + "\n"
    }

    fn marker(message: &str) -> Option<(Option<String>, Option<String>)> {
        let marker = fork_marker(
            &serde_json::from_str(&fork_info("m", "2026-07-27T12:00:00.000Z", message)).ok()?,
        )?;
        Some((marker.parent, marker.child))
    }

    fn named(parent: &str, child: &str) -> (Option<String>, Option<String>) {
        (Some(parent.to_owned()), Some(child.to_owned()))
    }

    #[test]
    fn a_marker_names_its_parent_and_fork_by_id_or_by_name() {
        assert_eq!(marker("Forked from p-1 as f-1."), Some(named("p-1", "f-1")));
        assert_eq!(
            marker("Forked from p-1 before event e-9 as f-1."),
            Some(named("p-1", "f-1"))
        );
        assert_eq!(
            marker(r#"Forked from "my \"plan\" as draft" (p-1) as "copy" (f-1)."#),
            Some(named("p-1", "f-1"))
        );
        assert_eq!(marker("Forked this session into f-1."), None);
        let into = |message: &str| {
            forked_into(
                &serde_json::from_str(&fork_info("m", "2026-07-27T12:00:00.000Z", message))
                    .expect("marker json"),
            )
        };
        assert_eq!(
            into("Forked this session into f-1.").as_deref(),
            Some("f-1")
        );
        assert_eq!(
            into(r#"Forked this session into "a (b)" (f-1) before event e-9."#).as_deref(),
            Some("f-1")
        );
        assert_eq!(into("Forked from p-1 as f-1."), None);
        assert_eq!(
            marker(r#"Forked from "unclosed (p-1) as f-1."#),
            Some((None, None))
        );
    }

    /// The parent's journal is gone but its live row isn't: the fork's copy
    /// of the parent's exit is the parent's, and gives way to that row.
    #[test]
    fn a_forks_copied_exit_gives_way_to_the_parents_rows() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T12:00:00.000Z"),
                shutdown("e-1", "2026-07-27T11:00:00.000Z", 100, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T12:00:00.000Z",
                    "Forked from parent as fork.",
                ),
                shutdown("e-2", "2026-07-27T13:00:00.000Z", 400, 40),
            ]),
        );
        write_store(
            temp.path(),
            &[call("parent", "2026-07-27T10:00:00.500Z", 100, 10)],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110 + 330);
    }

    /// A name holding " as " and quotes still names the parent.
    #[test]
    fn a_quoted_fork_name_still_names_the_parent() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T12:00:00.000Z"),
                shutdown("e-1", "2026-07-27T11:00:00.000Z", 100, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T12:00:00.000Z",
                    r#"Forked from "my \"plan\" as draft" (parent) as "copy" (fork)."#,
                ),
                shutdown("e-2", "2026-07-27T13:00:00.000Z", 400, 40),
            ]),
        );
        write_store(
            temp.path(),
            &[call("parent", "2026-07-27T10:00:00.500Z", 100, 10)],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110 + 330);
    }

    /// A fork of a fork holds two markers: each names the owner of the
    /// block before it.
    #[test]
    fn a_fork_of_a_fork_gives_each_block_to_its_owner() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "f2",
            &lines(&[
                fork_start("f2", "2026-07-27T14:00:00.000Z"),
                shutdown("e-1", "2026-07-27T11:00:00.000Z", 100, 10),
                fork_info("m-1", "2026-07-27T12:00:00.000Z", "Forked from p as f1."),
                shutdown("e-2", "2026-07-27T13:00:00.000Z", 400, 40),
                fork_info("m-2", "2026-07-27T14:00:00.000Z", "Forked from f1 as f2."),
                shutdown("e-3", "2026-07-27T15:00:00.000Z", 600, 60),
            ]),
        );
        write_store(
            temp.path(),
            &[
                call("p", "2026-07-27T10:00:00.500Z", 100, 10),
                call("f1", "2026-07-27T12:30:00.000Z", 300, 30),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110 + 330 + 220);
    }

    /// A marker that doesn't name its parent in a form read here leaves the
    /// copied exits to no one: they don't count, rather than count beside
    /// the parent's own records.
    #[test]
    fn copied_exits_under_an_unreadable_marker_dont_count() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T12:00:00.000Z"),
                shutdown("e-1", "2026-07-27T11:00:00.000Z", 100, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T12:00:00.000Z",
                    r#"Forked from "broken as fork."#,
                ),
                shutdown("e-2", "2026-07-27T13:00:00.000Z", 400, 40),
            ]),
        );
        write_store(
            temp.path(),
            &[call("parent", "2026-07-27T10:00:00.500Z", 100, 10)],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110 + 330);
    }

    /// Without a marker, exits dated before the journal's own start are
    /// copies: Copilot dates a fork's start at the fork.
    #[test]
    fn exits_dated_before_the_start_dont_count_without_a_marker() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T12:00:00.000Z"),
                shutdown("e-1", "2026-07-27T11:00:00.000Z", 100, 10),
                shutdown("e-2", "2026-07-27T13:00:00.000Z", 400, 40),
            ]),
        );
        write_store(
            temp.path(),
            &[call("parent", "2026-07-27T10:00:00.500Z", 100, 10)],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110 + 330);
    }

    /// The fork can't have recorded a call before it existed, so its store
    /// rows dated up to its marker copy the parent's history (caught up from
    /// the copied receipts, or copied in the store): they give way to the
    /// parent's own receipt.
    #[test]
    fn a_forks_rows_from_before_the_fork_dont_count() {
        use crate::collector::copilot::ledger::tests::set_event_ids;
        use crate::collector::copilot::receipts::tests::receipt;

        let temp = TempDir::new().expect("test tempdir should be created");
        let copied = receipt("2026-07-27T10:00:00.000Z", "parent", 1, 100, 10);
        write_session(temp.path(), "parent", &lines(std::slice::from_ref(&copied)));
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T11:00:00.000Z"),
                copied,
                fork_info(
                    "m-1",
                    "2026-07-27T11:00:00.000Z",
                    "Forked from parent as fork.",
                ),
                receipt("2026-07-27T12:00:00.000Z", "fork", 1, 400, 40),
            ]),
        );
        write_store(
            temp.path(),
            &[
                call("fork", "2026-07-27T10:00:00.000Z", 100, 10),
                call("fork", "2026-07-27T12:00:00.500Z", 400, 40),
            ],
        );
        set_event_ids(temp.path(), &[(1, "usage-record:a")]);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110 + 440);
    }

    /// A fork whose marker doesn't name it, or that has none, still can't
    /// have made a call before it existed: its own journal's rows from
    /// before the fork go as well, dated up to the marker or, without one,
    /// up to its start.
    #[test]
    fn an_unnamed_forks_rows_from_before_the_fork_dont_count() {
        use crate::collector::copilot::ledger::tests::set_event_ids;
        use crate::collector::copilot::receipts::tests::receipt;

        for marker in [Some(r#"Forked from "broken as fork."#), None] {
            let temp = TempDir::new().expect("test tempdir should be created");
            let copied = receipt("2026-07-27T10:00:00.000Z", "parent", 1, 100, 10);
            write_session(temp.path(), "parent", &lines(std::slice::from_ref(&copied)));
            let mut fork = vec![fork_start("fork", "2026-07-27T11:00:00.000Z"), copied];
            fork.extend(
                marker.map(|message| fork_info("m-1", "2026-07-27T11:00:00.000Z", message)),
            );
            fork.push(receipt("2026-07-27T12:00:00.000Z", "fork", 1, 400, 40));
            write_session(temp.path(), "fork", &lines(&fork));
            write_store(
                temp.path(),
                &[
                    call("parent", "2026-07-27T10:00:00.500Z", 100, 10),
                    call("fork", "2026-07-27T10:00:00.000Z", 100, 10),
                    call("fork", "2026-07-27T12:00:00.500Z", 400, 40),
                ],
            );
            set_event_ids(temp.path(), &[(2, "usage-record:a")]);

            let collection = collect(temp.path(), None, false, UtcOffset::UTC);

            assert_eq!(total_volume(&collection), 110 + 440, "{marker:?}");
        }
    }

    /// Each unreadable marker looks only at the block it closes: looking at
    /// every event kept before it, N markers after N receipts took N² steps.
    #[test]
    fn unreadable_markers_after_many_receipts_take_linear_time() {
        use crate::collector::copilot::receipts::tests::receipt;
        use crate::collector::read::tests::finishes_quickly;

        const N: u64 = 50_000;
        let temp = TempDir::new().expect("test tempdir should be created");
        let mut journal = String::new();
        for sequence in 1..=N {
            journal.push_str(&receipt(
                "2026-07-27T10:00:00.000Z",
                "parent",
                sequence,
                1,
                1,
            ));
            journal.push('\n');
        }
        let unreadable = fork_info("m", "2026-07-27T11:00:00.000Z", r#"Forked from "broken"#);
        for _ in 0..N {
            journal.push_str(&unreadable);
            journal.push('\n');
        }
        write_session(temp.path(), "s1", &journal);

        let root = temp.path().to_path_buf();
        let collection = finishes_quickly(move || collect(&root, None, false, UtcOffset::UTC));

        assert_eq!(total_volume(&collection), 2 * N);
    }

    /// The review's case: a marker-less fork whose only copy dated before
    /// its start is a user message is still a fork, so its caught-up row of
    /// the parent's call doesn't count beside the parent's live row.
    #[test]
    fn any_record_dated_before_the_start_marks_a_fork() {
        use crate::collector::copilot::ledger::tests::set_event_ids;

        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T11:00:00.000Z"),
                json!({"type": "user.message", "id": "u-1", "timestamp": "2026-07-27T10:00:00.000Z", "data": {"content": "hi"}}).to_string(),
            ]),
        );
        write_store(
            temp.path(),
            &[
                call("parent", "2026-07-27T10:00:00.500Z", 100, 10),
                call("fork", "2026-07-27T10:00:00.000Z", 100, 10),
                call("fork", "2026-07-27T12:00:00.500Z", 400, 40),
            ],
        );
        set_event_ids(temp.path(), &[(2, "usage-record:a")]);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110 + 440);
    }

    /// The review's case: the fork's journal is gone, but the parent's own
    /// marker names it, so the fork's caught-up row of the parent's call
    /// doesn't count beside the parent's live row.
    #[test]
    fn the_parents_marker_dates_a_fork_whose_journal_is_gone() {
        use crate::collector::copilot::ledger::tests::set_event_ids;

        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                fork_start("parent", "2026-07-27T09:00:00.000Z"),
                fork_info(
                    "m-1",
                    "2026-07-27T11:00:00.000Z",
                    "Forked this session into fork.",
                ),
            ]),
        );
        write_store(
            temp.path(),
            &[
                call("parent", "2026-07-27T10:00:00.000Z", 100, 10),
                call("fork", "2026-07-27T10:00:00.000Z", 100, 10),
            ],
        );
        set_event_ids(temp.path(), &[(2, "usage-record:a")]);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110);
    }

    fn journal_link(session: &str, at: &str) -> String {
        json!({
            "type": "session.journal_link",
            "id": format!("l-{session}"),
            "timestamp": at,
            "data": {"version": 1, "sessionId": session, "previousFile": "events.1.jsonl"},
        })
        .to_string()
    }

    /// The review's case: the parent's first segment is gone, so its first
    /// exit still in view counts everything before it, receipt 1's call
    /// included; the fork's copy of receipt 1 doesn't count again.
    #[test]
    fn a_forks_copy_inside_a_truncated_parents_first_exit_doesnt_count() {
        use crate::collector::copilot::receipts::tests::{marked, receipt};

        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                journal_link("parent", "2026-07-27T10:20:00.000Z"),
                receipt("2026-07-27T10:30:00.000Z", "parent", 2, 90, 10),
                marked(
                    &shutdown("e-2", "2026-07-27T11:00:00.000Z", 180, 20),
                    r#"{"parent":2}"#,
                ),
            ]),
        );
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T10:15:00.000Z"),
                receipt("2026-07-27T10:00:00.000Z", "parent", 1, 90, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T10:15:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 200);
    }

    /// The review's case: a fork of the fork, cut before the first fork's
    /// marker, files the copied receipt under the first fork. The receipt
    /// still names the parent and a sequence the parent's first exit read
    /// covers, so it doesn't count again.
    #[test]
    fn a_nested_forks_copy_inside_a_truncated_parents_first_exit_doesnt_count() {
        use crate::collector::copilot::receipts::tests::{marked, receipt};

        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                journal_link("parent", "2026-07-27T10:20:00.000Z"),
                receipt("2026-07-27T10:30:00.000Z", "parent", 2, 90, 10),
                marked(
                    &shutdown("e-2", "2026-07-27T11:00:00.000Z", 180, 20),
                    r#"{"parent":2}"#,
                ),
            ]),
        );
        let copied = receipt("2026-07-27T10:00:00.000Z", "parent", 1, 90, 10);
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T10:15:00.000Z"),
                copied.clone(),
                fork_info(
                    "m-1",
                    "2026-07-27T10:15:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );
        write_session(
            temp.path(),
            "nested",
            &lines(&[
                fork_start("nested", "2026-07-27T10:16:00.000Z"),
                copied,
                fork_info(
                    "m-2",
                    "2026-07-27T10:16:00.000Z",
                    "Forked from fork before event m-1 as nested.",
                ),
            ]),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 200);
    }

    /// A receipt the exit's watermark holds is inside it, even dated after
    /// it by a clock that moved back.
    #[test]
    fn a_forks_copy_inside_a_truncated_parents_watermark_doesnt_count() {
        use crate::collector::copilot::receipts::tests::{marked, receipt};

        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                journal_link("parent", "2026-07-27T10:20:00.000Z"),
                receipt("2026-07-27T10:30:00.000Z", "parent", 2, 90, 10),
                marked(
                    &shutdown("e-2", "2026-07-27T11:00:00.000Z", 180, 20),
                    r#"{"parent":2}"#,
                ),
            ]),
        );
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T11:15:00.000Z"),
                receipt("2026-07-27T11:10:00.000Z", "parent", 1, 90, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T11:15:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 200);
    }

    /// The same for a copied exit, which names no session: filed under the
    /// first fork, it's dated before that fork existed.
    #[test]
    fn a_nested_forks_copied_exit_inside_a_truncated_parents_first_exit_doesnt_count() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                journal_link("parent", "2026-07-27T10:20:00.000Z"),
                shutdown("e-2", "2026-07-27T11:00:00.000Z", 180, 20),
            ]),
        );
        let copied = shutdown("e-1", "2026-07-27T10:05:00.000Z", 90, 10);
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T10:15:00.000Z"),
                copied.clone(),
                fork_info(
                    "m-1",
                    "2026-07-27T10:15:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );
        write_session(
            temp.path(),
            "nested",
            &lines(&[
                fork_start("nested", "2026-07-27T10:16:00.000Z"),
                copied,
                fork_info(
                    "m-2",
                    "2026-07-27T10:16:00.000Z",
                    "Forked from fork before event m-1 as nested.",
                ),
            ]),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 200);
    }

    /// The same with no receipts: the fork's copy of an exit the missing
    /// segment held is inside the parent's first exit still in view.
    #[test]
    fn a_forks_copied_exit_inside_a_truncated_parents_first_exit_doesnt_count() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                journal_link("parent", "2026-07-27T10:20:00.000Z"),
                shutdown("e-2", "2026-07-27T11:00:00.000Z", 180, 20),
            ]),
        );
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T10:15:00.000Z"),
                shutdown("e-1", "2026-07-27T10:05:00.000Z", 90, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T10:15:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 200);
    }

    /// The review's case: the parent's exit still in view gives way to its
    /// store rows, so it holds nothing, and the fork's copy of the exit the
    /// missing segment held is the only count of it.
    #[test]
    fn a_truncated_parents_exit_superseded_by_its_rows_covers_nothing() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                journal_link("parent", "2026-07-27T10:20:00.000Z"),
                shutdown("e-2", "2026-07-27T11:30:00.000Z", 270, 30),
            ]),
        );
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T10:15:00.000Z"),
                shutdown("e-1", "2026-07-27T10:00:00.000Z", 90, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T10:15:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );
        write_store(
            temp.path(),
            &[call("parent", "2026-07-27T11:00:00.000Z", 180, 20)],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 300);
    }

    /// The store's rows from after the parent's exit still in view leave
    /// that exit standing, so the fork's copy still gives way.
    #[test]
    fn a_truncated_parents_exit_before_its_rows_still_covers_the_copy() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                journal_link("parent", "2026-07-27T10:20:00.000Z"),
                shutdown("e-2", "2026-07-27T11:00:00.000Z", 180, 20),
            ]),
        );
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T10:15:00.000Z"),
                shutdown("e-1", "2026-07-27T10:05:00.000Z", 90, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T10:15:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );
        write_store(
            temp.path(),
            &[call("parent", "2026-07-27T12:00:00.000Z", 45, 5)],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 250);
    }

    /// A copied receipt naming a third session, filed under the truncated
    /// parent: an exit without watermarks holds every receipt read before
    /// it, so the parent's exit holds it whatever session it names.
    #[test]
    fn a_copied_receipt_naming_another_session_gives_way_to_the_parents_exit() {
        use crate::collector::copilot::receipts::tests::receipt;

        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                journal_link("parent", "2026-07-27T10:20:00.000Z"),
                shutdown("e-2", "2026-07-27T11:00:00.000Z", 180, 20),
            ]),
        );
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T10:15:00.000Z"),
                receipt("2026-07-27T10:00:00.000Z", "grand", 1, 90, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T10:15:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 200);
    }

    /// The review's case: the parent's exit still in view counts only
    /// another model, so the fork's copy of the first model's exit is the
    /// only count of it.
    #[test]
    fn a_truncated_parents_exit_covers_only_its_own_models() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                journal_link("parent", "2026-07-27T10:20:00.000Z"),
                shutdown("e-2", "2026-07-27T11:00:00.000Z", 180, 20).replace("gpt-5-mini", "gpt-5"),
            ]),
        );
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T10:15:00.000Z"),
                shutdown("e-1", "2026-07-27T10:05:00.000Z", 90, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T10:15:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 300);
    }

    /// The review's case: counters lower than the copy's mean the parent's
    /// accounting restarted after it, so the exit still in view doesn't
    /// hold the copy.
    #[test]
    fn a_truncated_parents_exit_after_a_reset_doesnt_cover_the_copy() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "parent",
            &lines(&[
                journal_link("parent", "2026-07-27T10:20:00.000Z"),
                shutdown("e-2", "2026-07-27T11:00:00.000Z", 27, 3),
            ]),
        );
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T10:15:00.000Z"),
                shutdown("e-1", "2026-07-27T10:05:00.000Z", 90, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T10:15:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 130);
    }

    /// A session can't have used anything before it existed: a fork's
    /// receipt without an accounting identity dated before its start is a
    /// copy, whether the marker is missing or unreadable.
    #[test]
    fn a_forks_records_from_before_it_existed_dont_count() {
        for marker in [None, Some(r#"Forked from "broken as fork."#)] {
            let temp = TempDir::new().expect("test tempdir should be created");
            let bare = json!({
                "type": "session.usage_record",
                "id": "u-1",
                "timestamp": "2026-07-27T10:00:00.000Z",
                "data": {"usage": {"model": "gpt-5-mini", "inputTokens": 100, "outputTokens": 10, "cacheReadTokens": 0, "cacheWriteTokens": 0, "reasoningTokens": 0}},
            })
            .to_string();
            let mut fork = vec![fork_start("fork", "2026-07-27T12:00:00.000Z"), bare];
            fork.extend(
                marker.map(|message| fork_info("m-1", "2026-07-27T12:00:00.000Z", message)),
            );
            write_session(temp.path(), "fork", &lines(&fork));
            write_store(
                temp.path(),
                &[call("parent", "2026-07-27T10:00:00.500Z", 100, 10)],
            );

            let collection = collect(temp.path(), None, false, UtcOffset::UTC);

            assert_eq!(total_volume(&collection), 110, "{marker:?}");
        }
    }

    /// A fork of a marker-less fork: the copy of the first fork's own copied
    /// exit, relabelled to it, predates that fork and doesn't count.
    #[test]
    fn a_copy_dated_before_its_owner_existed_doesnt_count() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "f1",
            &lines(&[
                fork_start("f1", "2026-07-27T11:00:00.000Z"),
                shutdown("e-1", "2026-07-27T10:30:00.000Z", 100, 10),
                shutdown("e-2", "2026-07-27T12:00:00.000Z", 400, 40),
            ]),
        );
        write_session(
            temp.path(),
            "f2",
            &lines(&[
                fork_start("f2", "2026-07-27T13:00:00.000Z"),
                shutdown("e-1", "2026-07-27T10:30:00.000Z", 100, 10),
                shutdown("e-2", "2026-07-27T12:00:00.000Z", 400, 40),
                fork_info("m-1", "2026-07-27T13:00:00.000Z", "Forked from f1 as f2."),
            ]),
        );
        write_store(
            temp.path(),
            &[call("parent", "2026-07-27T10:00:00.000Z", 100, 10)],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 440);
    }

    /// A copied receipt without an accounting identity is the parent's once
    /// the marker names it, so the fork's exit takes it off as a copy.
    #[test]
    fn a_copied_receipt_without_an_identity_comes_off_the_forks_exit() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let bare = json!({
            "type": "session.usage_record",
            "id": "u-1",
            "timestamp": "2026-07-27T10:00:00.000Z",
            "data": {"usage": {"model": "gpt-5-mini", "inputTokens": 100, "outputTokens": 10, "cacheReadTokens": 0, "cacheWriteTokens": 0, "reasoningTokens": 0}},
        })
        .to_string();
        write_session(temp.path(), "parent", &lines(std::slice::from_ref(&bare)));
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T11:00:00.000Z"),
                bare,
                fork_info(
                    "m-1",
                    "2026-07-27T11:00:00.000Z",
                    "Forked from parent as fork.",
                ),
                shutdown("e-1", "2026-07-27T11:30:00.000Z", 100, 10),
            ]),
        );
        write_store(
            temp.path(),
            &[call("fork", "2026-07-27T12:00:00.000Z", 50, 5)],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 165);
    }

    /// A parent known only by caught-up rows, its journal gone, keeps them:
    /// the forks' copies, now the parent's, don't make it a session with
    /// receipts at hand, or every call the forks didn't copy would be lost.
    #[test]
    fn a_parent_known_by_caught_up_rows_keeps_them_beside_forks_copies() {
        use crate::collector::copilot::ledger::tests::set_event_ids;
        use crate::collector::copilot::receipts::tests::receipt;

        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "fork",
            &lines(&[
                fork_start("fork", "2026-07-27T12:00:00.000Z"),
                receipt("2026-07-27T10:00:00.000Z", "parent", 1, 100, 10),
                fork_info(
                    "m-1",
                    "2026-07-27T12:00:00.000Z",
                    "Forked from parent as fork.",
                ),
            ]),
        );
        write_store(
            temp.path(),
            &[
                call("parent", "2026-07-27T10:00:00.000Z", 100, 10),
                call("parent", "2026-07-27T11:00:00.000Z", 200, 20),
            ],
        );
        set_event_ids(temp.path(), &[(1, "usage-record:a"), (2, "usage-record:b")]);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 110 + 220);
    }
}
