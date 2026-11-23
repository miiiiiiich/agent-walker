use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::collector::FileEvents;
use crate::collector::json::parse_line;
use crate::collector::read::{Lines, open_regular};

/// Far past any session's rotation count; bounds the walk a crafted chain of
/// links could otherwise make.
const MAX_SEGMENTS: usize = 10_000;

/// A session's journal, oldest file first and ending with `head`, followed
/// back through each file's `session.journal_link`. Read file by file, each
/// new head's first shutdown would count the whole cumulative again. A
/// predecessor that is missing or unreadable ends the chain there (an
/// unreadable one as a parse error), so the journal degrades to the files
/// after it. `None` when the head itself can't be opened.
pub(super) fn journal(head: &Path, events: &mut FileEvents) -> Option<Vec<PathBuf>> {
    let dir = head.parent()?;
    let mut chain = vec![head.to_path_buf()];
    let mut previous = predecessor(head).ok()?;
    let mut below: Option<u64> = None;
    while let Some((number, name)) = previous.take() {
        // Copilot rejects a chain whose numbers don't fall; following one
        // could loop.
        if below.is_some_and(|below| number >= below) || chain.len() >= MAX_SEGMENTS {
            break;
        }
        let path = dir.join(name);
        let next = match predecessor(&path) {
            Ok(next) => next,
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    events.parse_errors += 1;
                }
                break;
            }
        };
        chain.push(path);
        below = Some(number);
        previous = next;
    }
    chain.reverse();
    Some(chain)
}

/// Whether `head` names a numbered predecessor, followed or not: its parse
/// then reads files besides `head`.
pub(super) fn links_back(head: &Path) -> bool {
    matches!(predecessor(head), Ok(Some(_)))
}

/// The numbered file `path`'s first record links back to, if any.
fn predecessor(path: &Path) -> io::Result<Option<(u64, String)>> {
    let file = open_regular(path)?;
    let mut scratch = FileEvents::default();
    let mut lines = Lines::new(BufReader::new(file));
    let link = lines
        .next(&mut scratch)
        .and_then(|(_, line)| parse_line(line, &mut scratch))
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("session.journal_link"))
        .and_then(|value| {
            let name = value.pointer("/data/previousFile")?.as_str()?;
            Some((segment_number(name)?, name.to_owned()))
        });
    Ok(link)
}

/// `previousFile` comes from the log, so only a bare `events.<n>.jsonl` in
/// the same directory is followed, never a path.
fn segment_number(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("events.")?.strip_suffix(".jsonl")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use time::UtcOffset;

    use super::links_back;
    use crate::collector::copilot::collect;
    use crate::collector::copilot::receipts::tests::receipt;
    use crate::collector::copilot::tests::{shutdown, total_volume, write_session};

    fn write_segment(root: &std::path::Path, session: &str, number: u64, lines: &str) {
        let path = root
            .join("session-state")
            .join(session)
            .join(format!("events.{number}.jsonl"));
        fs::write(path, lines).expect("segment should be written");
    }

    fn link(id: &str, previous: &str) -> String {
        format!(
            r#"{{"type":"session.journal_link","id":"{id}","timestamp":"2026-07-27T12:00:00.000Z","data":{{"version":1,"sessionId":"s1","previousFile":"{previous}"}}}}"#
        )
    }

    fn replayed(line: &str) -> String {
        line.replacen(r#"{"type""#, r#"{"checkpoint":true,"type""#, 1)
    }

    const START: &str = r#"{"type":"session.start","id":"st-1","timestamp":"2026-07-27T09:00:00.000Z","data":{"context":{"cwd":"/Users/me/code/app"}}}"#;

    fn tool(id: &str, at: &str) -> String {
        format!(
            r#"{{"type":"tool.execution_start","id":"{id}","timestamp":"{at}","data":{{"toolCallId":"{id}","toolName":"bash"}}}}"#
        )
    }

    /// The head's checkpoint copies are skipped, and its exit counts only
    /// what happened since the archived segment's exit.
    #[test]
    fn rotated_journal_counts_each_exit_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let archived = [
            START.to_owned(),
            tool("t-1", "2026-07-27T09:30:00.000Z"),
            shutdown("e-1", "2026-07-27T10:00:00.000Z", 1_000, 100),
        ];
        let head = [
            link("l-1", "events.1.jsonl"),
            replayed(START),
            replayed(&tool("t-1c", "2026-07-27T12:00:00.000Z")),
            replayed(&shutdown("e-1c", "2026-07-27T12:00:00.000Z", 1_000, 100)),
            tool("t-2", "2026-07-27T12:30:00.000Z"),
            shutdown("e-2", "2026-07-27T13:00:00.000Z", 1_500, 150),
        ];
        write_session(temp.path(), "s1", &(head.join("\n") + "\n"));
        write_segment(temp.path(), "s1", 1, &(archived.join("\n") + "\n"));

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 1_650);
        let hours: Vec<u8> = collection
            .usage_events
            .iter()
            .filter_map(|event| event.timestamp.map(time::OffsetDateTime::hour))
            .collect();
        assert_eq!(hours, vec![10, 13]);
        assert_eq!(collection.tool_events.len(), 2);
        assert_eq!(
            collection.usage_events[1].project.as_deref(),
            Some("Users/me/code/app")
        );
        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.stats.lines_seen, 9);
        assert_eq!(collection.stats.parse_errors, 0);
    }

    /// Receipts in the archive are covered by the marked exit in the head,
    /// so the exit adds only what no receipt carried.
    #[test]
    fn receipts_in_the_archive_are_covered_by_the_heads_exit() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n{}\n{}\n",
                link("l-1", "events.1.jsonl"),
                receipt("2026-07-27T12:00:00.000Z", "s1", 3, 300, 30),
                shutdown("e-1", "2026-07-27T13:00:00.000Z", 1_600, 160).replacen(
                    r#""data":{"#,
                    r#""data":{"usageAccountingWatermarks":{"s1":3},"#,
                    1
                ),
            ),
        );

        write_segment(
            temp.path(),
            "s1",
            1,
            &format!(
                "{}\n{}\n",
                receipt("2026-07-27T10:00:00.000Z", "s1", 1, 100, 10),
                receipt("2026-07-27T10:01:00.000Z", "s1", 2, 200, 20),
            ),
        );
        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 1_760);
        assert_eq!(collection.usage_events.len(), 4);
    }

    #[test]
    fn segments_are_read_oldest_first_along_the_links() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n{}\n",
                link("l-2", "events.2.jsonl"),
                shutdown("e-3", "2026-07-27T14:00:00.000Z", 2_000, 200)
            ),
        );
        write_segment(
            temp.path(),
            "s1",
            2,
            &format!(
                "{}\n{}\n",
                link("l-1", "events.1.jsonl"),
                shutdown("e-2", "2026-07-27T12:00:00.000Z", 1_500, 150)
            ),
        );
        write_segment(
            temp.path(),
            "s1",
            1,
            &format!(
                "{}\n",
                shutdown("e-1", "2026-07-27T10:00:00.000Z", 1_000, 100)
            ),
        );
        write_segment(
            temp.path(),
            "s1",
            7,
            &format!(
                "{}\n",
                shutdown("e-9", "2026-07-27T11:00:00.000Z", 9_000, 900)
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 2_200);
        assert_eq!(collection.usage_events.len(), 3);
    }

    /// `previousFile` is log content: a path, or a link whose number doesn't
    /// fall, is not followed.
    #[test]
    fn links_out_of_the_session_or_upward_are_not_followed() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "other",
            &format!(
                "{}\n",
                shutdown("o-1", "2026-07-27T10:00:00.000Z", 5_000, 500)
            ),
        );
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n{}\n",
                link("l-1", "../other/events.jsonl"),
                shutdown("e-2", "2026-07-27T12:00:00.000Z", 1_500, 150)
            ),
        );
        write_session(
            temp.path(),
            "s2",
            &format!(
                "{}\n{}\n",
                link("l-2", "events.2.jsonl"),
                shutdown("f-3", "2026-07-27T14:00:00.000Z", 3_000, 300)
            ),
        );
        write_segment(
            temp.path(),
            "s2",
            2,
            &format!(
                "{}\n{}\n",
                link("l-3", "events.3.jsonl"),
                shutdown("f-2", "2026-07-27T12:00:00.000Z", 2_000, 200)
            ),
        );
        write_segment(
            temp.path(),
            "s2",
            3,
            &format!(
                "{}\n",
                shutdown("f-1", "2026-07-27T10:00:00.000Z", 1_000, 100)
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let volume = |session: &str| -> u64 {
            collection
                .usage_events
                .iter()
                .filter(|event| event.session_id.as_deref() == Some(session))
                .map(|event| event.usage.token_volume())
                .sum()
        };
        assert_eq!(volume("other"), 5_500);
        assert_eq!(volume("s1"), 1_650);
        assert_eq!(volume("s2"), 3_300);
    }

    /// A head that names a predecessor links back even while the
    /// predecessor is missing, since it may appear later; a link that is
    /// never followed leaves the head on its own.
    #[test]
    fn a_head_links_back_whether_or_not_its_predecessor_is_there() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let exit = shutdown("e-2", "2026-07-27T12:00:00.000Z", 1_500, 150);
        write_session(
            temp.path(),
            "missing",
            &format!("{}\n{exit}\n", link("l-1", "events.1.jsonl")),
        );
        write_session(
            temp.path(),
            "outward",
            &format!("{}\n{exit}\n", link("l-1", "../other/events.jsonl")),
        );
        write_session(temp.path(), "plain", &format!("{exit}\n"));
        let head = |session: &str| {
            temp.path()
                .join("session-state")
                .join(session)
                .join("events.jsonl")
        };

        assert!(links_back(&head("missing")));
        assert!(!links_back(&head("outward")));
        assert!(!links_back(&head("plain")));
        assert!(!links_back(&head("absent")));
    }

    #[test]
    fn missing_predecessor_leaves_the_head_alone() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n{}\n",
                link("l-1", "events.4.jsonl"),
                shutdown("e-2", "2026-07-27T12:00:00.000Z", 1_500, 150)
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(total_volume(&collection), 1_650);
        assert_eq!(collection.stats.parse_errors, 0);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_predecessor_is_a_parse_error() {
        use crate::collector::read::tests::{finishes_quickly, make_fifo};

        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "s1",
            &format!(
                "{}\n{}\n",
                link("l-1", "events.1.jsonl"),
                shutdown("e-2", "2026-07-27T12:00:00.000Z", 1_500, 150)
            ),
        );
        make_fifo(&temp.path().join("session-state/s1/events.1.jsonl"));

        let root = temp.path().to_path_buf();
        let collection = finishes_quickly(move || collect(&root, None, false, UtcOffset::UTC));

        assert_eq!(total_volume(&collection), 1_650);
        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(collection.stats.unreadable_files, 0);
    }
}
