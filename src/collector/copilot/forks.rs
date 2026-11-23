use serde_json::Value;

use crate::collector::ident::ident;

/// A fork's journal opens with a copy of its parent's history, event ids
/// and all, then a `session.info` of `infoType` "fork" reading "Forked from
/// <parent> [before event <id>] as <fork>.", each session named by its id
/// or as `"<name>" (<id>)`. The parent's own marker reads "Forked this
/// session into …" and isn't one.
pub(super) struct ForkMarker {
    /// `None` when the marker doesn't name it in a form read here.
    pub(super) parent: Option<String>,
}

pub(super) fn fork_marker(value: &Value) -> Option<ForkMarker> {
    if value.get("type").and_then(Value::as_str) != Some("session.info") {
        return None;
    }
    let data = value.get("data")?;
    if data.get("infoType").and_then(Value::as_str) != Some("fork") {
        return None;
    }
    let named = data
        .get("message")?
        .as_str()?
        .strip_prefix("Forked from ")?;
    Some(ForkMarker {
        parent: session_ref(named).map(ToOwned::to_owned),
    })
}

/// The session id a reference starting `text` names. A name is a JSON
/// string, so it is parsed rather than split: it may hold " as " or quotes.
fn session_ref(text: &str) -> Option<&str> {
    let id = if text.starts_with('"') {
        let mut values = serde_json::Deserializer::from_str(text).into_iter::<String>();
        values.next()?.ok()?;
        let after = text.get(values.byte_offset()..)?.strip_prefix(" (")?;
        &after[..after.find(')')?]
    } else {
        let end = text.find([' ', '.']).unwrap_or(text.len());
        &text[..end]
    };
    let valid = !id.is_empty() && !id.contains(char::is_whitespace);
    ident(id).filter(|_| valid)
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

    fn marker(message: &str) -> Option<String> {
        fork_marker(
            &serde_json::from_str(&fork_info("m", "2026-07-27T12:00:00.000Z", message)).ok()?,
        )?
        .parent
    }

    #[test]
    fn a_marker_names_its_parent_by_id_or_by_name() {
        assert_eq!(marker("Forked from p-1 as f-1.").as_deref(), Some("p-1"));
        assert_eq!(
            marker("Forked from p-1 before event e-9 as f-1.").as_deref(),
            Some("p-1")
        );
        assert_eq!(
            marker(r#"Forked from "my \"plan\" as draft" (p-1) as "copy" (f-1)."#).as_deref(),
            Some("p-1")
        );
        assert!(marker("Forked this session into f-1.").is_none());
        assert_eq!(marker(r#"Forked from "unclosed (p-1) as f-1."#), None);
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
