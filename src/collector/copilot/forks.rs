use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;
use time::OffsetDateTime;

use crate::collector::FileEvents;
use crate::collector::ident::ident;

/// Key prefix of the event carrying a fork's own session and the time of
/// its fork, taken out by `take_fork_times` before anything else reads the
/// events.
pub(super) const FORK_PREFIX: &str = "copilot-fork:";

/// Likewise, from the parent's marker: used only for a fork whose own
/// marker isn't read, its journal gone.
pub(super) const FORKED_INTO_PREFIX: &str = "copilot-forked-into:";

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

/// Each fork's session and the time of its fork, taken out of the events:
/// from the fork's own marker, or failing that the parent's.
pub(super) fn take_fork_times(
    per_file: &mut [(PathBuf, Option<FileEvents>)],
) -> HashMap<String, OffsetDateTime> {
    let mut own: HashMap<String, OffsetDateTime> = HashMap::new();
    let mut named: HashMap<String, OffsetDateTime> = HashMap::new();
    for events in per_file
        .iter_mut()
        .filter_map(|(_, events)| events.as_mut())
    {
        events.usage_events.retain(|keyed| {
            let key = keyed.key.as_deref().unwrap_or_default();
            let forks = if key.starts_with(FORK_PREFIX) {
                &mut own
            } else if key.starts_with(FORKED_INTO_PREFIX) {
                &mut named
            } else {
                return true;
            };
            if let (Some(session), Some(time)) =
                (keyed.event.session_id.clone(), keyed.event.timestamp)
            {
                forks
                    .entry(session)
                    .and_modify(|at| *at = (*at).max(time))
                    .or_insert(time);
            }
            false
        });
    }
    for (fork, at) in named {
        own.entry(fork).or_insert(at);
    }
    own
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
            journal.push_str(&receipt("2026-07-27T10:00:00.000Z", "s1", sequence, 1, 1));
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
