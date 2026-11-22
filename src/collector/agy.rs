use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use time::UtcOffset;

use crate::collector::{FileEvents, list_files, merge_into, parse_files_cached};
use crate::model::{Collection, Provider, SessionTouch, SourceKind};

mod conv;
mod history;
mod log;

const CLI_STORE: &str = "antigravity-cli";
const APP_STORE: &str = "antigravity";

pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Agy, root.to_path_buf());
    if !root.exists() {
        return collection;
    }

    let cli = root.join(CLI_STORE);
    let mut files = Vec::new();
    let history = cli.join("history.jsonl");
    if history.exists() {
        files.push(history);
    }
    let log_dir = cli.join("log");
    if log_dir.exists() {
        files.extend(list_files(
            &log_dir,
            "log",
            mtime_floor,
            &mut collection.stats,
        ));
    }

    let per_file = parse_files_cached(use_cache.then_some("agy"), &files, local_offset, |path| {
        parse_file(path, local_offset)
    });
    merge_into(&mut collection, per_file);

    let usage = conv::collect_usage(
        &[cli, root.join(APP_STORE)],
        mtime_floor,
        local_offset,
        &mut collection.stats,
    );
    attribute_subagents(&mut collection, &usage.subagents);
    // App conversations, and CLI conversations with no history or log lines,
    // have nothing but their generations: without a touch per generation their
    // sessions count in the total but never reach daily sessions, spans or
    // concurrency. Every generation touches, not only untouched conversations:
    // the DB name is the history's conversationId, so the analyzer merges both
    // into one session.
    collection
        .session_touches
        .extend(usage.events.iter().filter_map(|event| {
            Some(SessionTouch {
                timestamp: event.timestamp?,
                session_id: event.session_id.clone()?,
            })
        }));
    collection.usage_events.extend(usage.events);
    collection.stats.usage_events = collection.usage_events.len();
    collection
}

/// Log and history lines are parsed per file, without the conversation DBs,
/// so a line naming a subagent conversation is moved to its top-level session
/// here, the way its generations are.
fn attribute_subagents(collection: &mut Collection, subagents: &HashMap<String, conv::Subagent>) {
    if subagents.is_empty() {
        return;
    }
    for touch in &mut collection.session_touches {
        if let Some(subagent) = subagents.get(&touch.session_id) {
            touch.session_id.clone_from(&subagent.session_id);
        }
    }
    for tool in &mut collection.tool_events {
        let Some(subagent) = tool.session_id.as_ref().and_then(|id| subagents.get(id)) else {
            continue;
        };
        tool.session_id = Some(subagent.session_id.clone());
        tool.source_kind = SourceKind::Subagent;
        tool.subagent_type.clone_from(&subagent.name);
    }
}

fn parse_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    if path.file_name().is_some_and(|name| name == "history.jsonl") {
        history::parse_history_file(path, local_offset)
    } else {
        log::parse_log_file(path, local_offset)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use time::macros::{date, datetime};

    use super::conv::tests::{
        served_only_gen_blob, step_metadata, subagent_trajectory, write_dated_conversation,
        write_trajectory_conversation,
    };
    use super::*;
    use crate::collector::ident::tests::overlong_ident;
    use crate::collector::read::tests::oversized_json;

    const INPUT_LOG_LINE: &str = r#"I0609 11:27:22.646388  8577 input_loop.go:34] HandleUserInput called with text: "redacted""#;

    #[test]
    fn collects_antigravity_history_model_events_and_tool_confirmations() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        let log_dir = cli.join("log");
        fs::create_dir_all(&log_dir).expect("test dirs should be created");
        let app_conversations = temp.path().join(APP_STORE).join("conversations");
        fs::create_dir_all(&app_conversations).expect("app store should be created");
        rusqlite::Connection::open(app_conversations.join("c2.db"))
            .and_then(|conn| {
                conn.execute_batch("CREATE TABLE gen_metadata (idx INTEGER, data BLOB);")
            })
            .expect("app conversation should be created");
        fs::write(
            cli.join("history.jsonl"),
            r#"{"timestamp":1780726748000,"workspace":"/tmp","conversationId":"c1"}"#,
        )
        .expect("history fixture should be written");
        fs::write(
            log_dir.join("cli-20260609_112719.log"),
            concat!(
                r#"I0609 11:27:21.569672  8577 model_config_manager.go:157] Propagating selected model override to backend: label="Gemini 3.5 Flash (High)""#,
                "\n",
                r#"I0609 11:27:22.646388  8577 input_loop.go:34] HandleUserInput called with text: "redacted""#,
                "\n",
                r#"I0609 11:38:30.959991  8577 input_loop.go:451] Responding to tool confirmation: convID=c1, stepIdx=38, approved=true, sandboxOverride=false, turnGrants=[command(bun run)]"#,
                "\n"
            ),
        )
        .expect("log fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        // history + log + the app store's conversation
        assert_eq!(collection.stats.files_seen, 3);
        assert!(collection.usage_events.is_empty());
        assert_eq!(collection.tool_events[0].tool_name, "command:bun");
        assert!(collection.session_touches.len() >= 2);
    }

    #[test]
    fn conversation_generations_count_as_sessions_once_per_day() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        let app = temp.path().join(APP_STORE);
        for store in [&cli, &app] {
            fs::create_dir_all(store.join("conversations"))
                .expect("conversations dir should be created");
        }
        let started_ms = 1_780_966_800_000; // 2026-06-09 01:00 UTC
        let hour_ms = 3_600_000;
        let row = |rid: &str| served_only_gen_blob("gemini-3.8-flash", rid, None);
        // c1 is in the CLI history and has a generation an hour later.
        fs::write(
            cli.join("history.jsonl"),
            format!(r#"{{"timestamp":{started_ms},"conversationId":"c1"}}"#),
        )
        .expect("history fixture should be written");
        write_dated_conversation(
            &cli.join("conversations").join("c1.db"),
            started_ms,
            &[row("r1")],
            &[step_metadata(started_ms + hour_ms, "r1")],
        );
        // c2 exists only in the app store: no history, no logs.
        write_dated_conversation(
            &app.join("conversations").join("c2.db"),
            started_ms,
            &[row("r2"), row("r3")],
            &[
                step_metadata(started_ms + 2 * hour_ms, "r2"),
                step_metadata(started_ms + 4 * hour_ms, "r3"),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);
        let summary = crate::analyzer::summarize(
            &collection,
            datetime!(2026-06-09 23:00 UTC),
            7,
            UtcOffset::UTC,
        );

        assert_eq!(collection.usage_events.len(), 3);
        assert_eq!(summary.sessions, 2);
        let sessions_per_day: Vec<_> = summary
            .daily_sessions
            .iter()
            .map(|day| (day.date, day.sessions))
            .collect();
        assert_eq!(sessions_per_day, [(date!(2026 - 06 - 09), 2)]);
        assert_eq!(
            summary
                .longest_session
                .expect("the app conversation should have a span")
                .duration_secs(),
            7_200
        );
    }

    #[test]
    fn subagent_work_counts_in_the_parent_session() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        let log_dir = cli.join("log");
        let conversations = cli.join("conversations");
        fs::create_dir_all(&log_dir).expect("log dir should be created");
        fs::create_dir_all(&conversations).expect("conversations dir should be created");
        let started_ms = 1_780_966_800_000; // 2026-06-09 01:00 UTC
        write_dated_conversation(
            &conversations.join("parent.db"),
            started_ms,
            &[served_only_gen_blob("gemini-3.8-flash", "r1", None)],
            &[step_metadata(started_ms + 60_000, "r1")],
        );
        write_trajectory_conversation(
            &conversations.join("child.db"),
            &subagent_trajectory(started_ms, "parent", "parent", "research"),
            &[served_only_gen_blob("gemini-3.8-flash", "r2", None)],
            &[step_metadata(started_ms + 120_000, "r2")],
        );
        fs::write(
            log_dir.join("cli-20260609_010000.log"),
            "I0609 01:03:00.000000  8577 input_loop.go:451] Responding to tool confirmation: convID=child, stepIdx=3, approved=true, turnGrants=[command(bun run)]\n",
        )
        .expect("log fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);
        let summary = crate::analyzer::summarize(
            &collection,
            datetime!(2026-06-09 23:00 UTC),
            7,
            UtcOffset::UTC,
        );

        assert_eq!(summary.sessions, 1);
        let tool = &collection.tool_events[0];
        assert_eq!(tool.session_id.as_deref(), Some("parent"));
        assert_eq!(tool.source_kind, SourceKind::Subagent);
        assert_eq!(tool.subagent_type.as_deref(), Some("research"));
        let agents: Vec<_> = summary
            .agents
            .iter()
            .map(|agent| (agent.name.as_str(), agent.usage.token_volume()))
            .collect();
        assert_eq!(agents, [("research", 4270)]);
    }

    #[test]
    fn history_and_log_rows_past_the_plausible_range_are_undated() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        let log_dir = cli.join("log");
        fs::create_dir_all(&log_dir).expect("test dirs should be created");
        fs::write(
            cli.join("history.jsonl"),
            concat!(
                r#"{"timestamp":1780726748000,"conversationId":"c1"}"#,
                "\n",
                r#"{"timestamp":253402290000000,"conversationId":"c2"}"#,
            ),
        )
        .expect("history fixture should be written");
        fs::write(log_dir.join("cli-99991231_235900.log"), INPUT_LOG_LINE)
            .expect("log fixture should be written");

        let tokyo = UtcOffset::from_hms(9, 0, 0).expect("offset");
        let collection = collect(temp.path(), None, false, tokyo);

        assert_eq!(collection.session_touches.len(), 1);
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn history_and_log_lines_past_the_cap_are_parse_errors() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        let log_dir = cli.join("log");
        fs::create_dir_all(&log_dir).expect("test dirs should be created");
        let history = r#"{"timestamp":1780726748000,"conversationId":"c1"}"#;
        fs::write(
            cli.join("history.jsonl"),
            format!("{}\n{history}\n", oversized_json(history)),
        )
        .expect("history fixture should be written");
        let padded_log_line = format!(
            "{INPUT_LOG_LINE}{}",
            " ".repeat(usize::try_from(crate::collector::read::MAX_LINE_BYTES).expect("fits"))
        );
        fs::write(
            log_dir.join("cli-20260609_112719.log"),
            format!("{padded_log_line}\n{INPUT_LOG_LINE}\n"),
        )
        .expect("log fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 2);
        assert_eq!(collection.session_touches.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn history_that_is_a_fifo_is_not_opened() {
        use crate::collector::read::tests::{finishes_quickly, make_fifo};

        let temp = TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join(CLI_STORE);
        fs::create_dir_all(&cli).expect("test dirs should be created");
        make_fifo(&cli.join("history.jsonl"));

        let root = temp.path().to_path_buf();
        let collection = finishes_quickly(move || collect(&root, None, false, UtcOffset::UTC));

        assert_eq!(collection.stats.unreadable_files, 1);
    }

    #[test]
    fn generation_view_is_not_read() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let conversations = temp.path().join(CLI_STORE).join("conversations");
        fs::create_dir_all(&conversations).expect("test dirs should be created");
        let conn = rusqlite::Connection::open(conversations.join("c1.db"))
            .expect("fixture db should open");
        conn.execute_batch(
            "CREATE TABLE hidden (idx INTEGER PRIMARY KEY, data BLOB);
             CREATE VIEW gen_metadata AS SELECT idx, data FROM hidden;",
        )
        .expect("fixture schema should be created");
        conn.execute(
            "INSERT INTO hidden VALUES (0, ?1)",
            [served_only_gen_blob("gemini-3.8-flash", "r1", None)],
        )
        .expect("generation row should insert");
        drop(conn);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert!(collection.usage_events.is_empty());
    }

    #[test]
    fn overlong_conversation_id_falls_back_to_the_log() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let log_dir = temp.path().join(CLI_STORE).join("log");
        fs::create_dir_all(&log_dir).expect("test dirs should be created");
        let long = overlong_ident();
        fs::write(
            log_dir.join("cli-20260609_112719.log"),
            format!(
                "I0609 11:38:30.959991  8577 input_loop.go:451] Responding to tool confirmation: convID={long}, stepIdx=38, approved=true, turnGrants=[command(bun run)]\n"
            ),
        )
        .expect("log fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(
            collection.tool_events[0].session_id.as_deref(),
            Some("cli-20260609_112719")
        );
    }

    #[test]
    fn generated_generation_column_is_not_read() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let conversations = temp.path().join(CLI_STORE).join("conversations");
        fs::create_dir_all(&conversations).expect("test dirs should be created");
        let conn = rusqlite::Connection::open(conversations.join("c1.db"))
            .expect("fixture db should open");
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY,
                 data BLOB GENERATED ALWAYS AS (zeroblob(65536)) VIRTUAL);
             WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 128)
             INSERT INTO gen_metadata (idx) SELECT n FROM r;",
        )
        .expect("fixture schema should be created");
        drop(conn);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.stats.lines_seen, 0);
    }
}
