use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;
use time::OffsetDateTime;

use crate::collector::ident::ident;
use crate::collector::json::parse_bounded;
use crate::collector::{FileEvents, KeyedToolEvent};
use crate::model::ToolEvent;

use super::fork::Origin;
use super::string_path;

/// Code Mode's `exec` runs a script, and each shell command the script
/// starts is an `item_completed` `CommandExecution` that names no call. The
/// first command names the latest `exec` and each later one counts as its
/// own tool, so an `exec` that started no shell command stays `exec`.
#[derive(Default)]
pub(super) struct ToolState {
    /// The latest `exec`'s key in `execs`.
    latest_exec: Option<String>,
    /// Each `exec`'s first tool event in this file, by its key, and whether a
    /// command has named it yet. A replayed `exec` call goes back to it: given
    /// a fresh slot, the next command renamed the replay's copy, which the
    /// merge drops for the first.
    execs: HashMap<String, (usize, bool)>,
    /// Ids a `CommandExecution` item adds nothing under: a shell function
    /// call's item reuses its call id, already counted under its command, and
    /// an item that already named a command. A replay of the command that
    /// named an `exec` added a second tool under its own id.
    counted: HashSet<String>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "Per-line parse context; bundling into a struct adds noise for one caller."
)]
pub(super) fn collect_tool_event(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    origin: &Origin,
    path: &Path,
    line_index: usize,
    state: &mut ToolState,
    events: &mut FileEvents,
) {
    match value.get("type").and_then(Value::as_str) {
        Some("response_item") => {}
        Some("event_msg") => {
            collect_command_item(
                value, timestamp, session_id, origin, path, line_index, state, events,
            );
            return;
        }
        _ => return,
    }
    let Some(raw_name) = string_path(value, &["payload", "name"]) else {
        return;
    };
    // Codex runs most reads and writes through a generic shell wrapper
    // (`exec_command` etc., usually `bash -lc "..."`); resolving the wrapper to
    // the real command basename lets the tool list show `grep`/`cargo` instead of
    // one undifferentiated "exec" bucket.
    let is_shell_call = is_shell_wrapper(&raw_name);
    let is_code_mode_exec = raw_name == "exec"
        && string_path(value, &["payload", "type"]).as_deref() == Some("custom_tool_call");
    let tool_name = if is_shell_call {
        exec_command_basename(value).unwrap_or(raw_name)
    } else {
        raw_name
    };
    let call_id = string_path(value, &["payload", "call_id"]);
    if is_shell_call && let Some(call_id) = &call_id {
        state.counted.insert(call_id.clone());
    }
    let key = call_id.unwrap_or_else(|| {
        format!(
            "{}:{}:{}",
            path.display(),
            line_index + 1,
            tool_name.as_str()
        )
    });
    if is_code_mode_exec {
        state
            .execs
            .entry(key.clone())
            .or_insert((events.tool_events.len(), false));
        state.latest_exec = Some(key.clone());
    }
    events.tool_events.push(KeyedToolEvent {
        key: Some(key),
        event: ToolEvent {
            timestamp,
            session_id: session_id.cloned(),
            tool_name,
            subagent_type: None,
            source_kind: origin.source_kind(),
        },
    });
}

#[allow(
    clippy::too_many_arguments,
    reason = "Per-line parse context; bundling into a struct adds noise for one caller."
)]
fn collect_command_item(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    origin: &Origin,
    path: &Path,
    line_index: usize,
    state: &mut ToolState,
    events: &mut FileEvents,
) {
    let Some(payload) = value.get("payload") else {
        return;
    };
    if payload.get("type").and_then(Value::as_str) != Some("item_completed") {
        return;
    }
    let Some(item) = payload.get("item") else {
        return;
    };
    if item.get("type").and_then(Value::as_str) != Some("CommandExecution") {
        return;
    }
    // A command the person ran from the composer is not the model's tool.
    if item.get("source").and_then(Value::as_str) == Some("user_shell") {
        return;
    }
    let item_id = string_path(item, &["id"]);
    if item_id
        .as_ref()
        .is_some_and(|id| state.counted.contains(id))
    {
        return;
    }
    let Some(tool_name) = item
        .get("command")
        .and_then(command_name)
        .or_else(|| item.pointer("/parsed_cmd/0/cmd").and_then(command_name))
    else {
        return;
    };
    // Settled once it names a command, with or without an `exec` to count
    // under: an item that named none, settled, dropped a later copy that did,
    // and one from before any `exec`, left open, named the next on a replay.
    if let Some(id) = &item_id {
        state.counted.insert(id.clone());
    }
    let Some((exec_index, named)) = state
        .latest_exec
        .as_ref()
        .and_then(|key| state.execs.get_mut(key))
    else {
        return;
    };
    if !*named && let Some(exec) = events.tool_events.get_mut(*exec_index) {
        exec.event.tool_name = tool_name;
        *named = true;
        return;
    }
    let key = item_id.unwrap_or_else(|| {
        format!(
            "{}:{}:{}",
            path.display(),
            line_index + 1,
            tool_name.as_str()
        )
    });
    events.tool_events.push(KeyedToolEvent {
        key: Some(key),
        event: ToolEvent {
            timestamp,
            session_id: session_id.cloned(),
            tool_name,
            subagent_type: None,
            source_kind: origin.source_kind(),
        },
    });
}

fn is_shell_wrapper(name: &str) -> bool {
    matches!(
        name,
        "exec_command" | "shell" | "local_shell" | "unified_exec"
    )
}

fn exec_command_basename(value: &Value) -> Option<String> {
    // Command text, not an id, so not held to the id bound (`string_path`).
    let arguments = value
        .pointer("/payload/arguments")
        .and_then(Value::as_str)?;
    // JSON inside a string, so the line's own structure count never saw it.
    let parsed = parse_bounded(arguments)?;
    command_name(parsed.get("command").or_else(|| parsed.get("cmd"))?)
}

/// The basename of the program a command runs, from an argv array or a
/// command line.
fn command_name(command: &Value) -> Option<String> {
    let tokens = command_tokens(command)?;
    basename(effective_command(&tokens)?)
}

/// Leading tokens a command is resolved from. The command sits among the
/// first few (after `env`, assignments or a shell's `-c`), so the rest is
/// never split: a 48 MiB `cmd` of `a a a …` became ~25M owned strings.
const MAX_COMMAND_TOKENS: usize = 64;

fn command_tokens(command: &Value) -> Option<Vec<&str>> {
    let tokens: Vec<&str> = match command {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .take(MAX_COMMAND_TOKENS)
            .collect(),
        Value::String(text) => text.split_whitespace().take(MAX_COMMAND_TOKENS).collect(),
        _ => return None,
    };
    (!tokens.is_empty()).then_some(tokens)
}

fn is_shell_command_flag(token: &str) -> bool {
    token.starts_with('-') && !token.starts_with("--") && token.contains('c')
}

const RUN_PREFIXES: [&str; 4] = ["env", "sudo", "time", "nice"];

fn is_var_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn effective_command<'a>(tokens: &[&'a str]) -> Option<&'a str> {
    let first = tokens.first()?;
    let is_shell = matches!(
        basename(first).as_deref(),
        Some("bash" | "sh" | "zsh" | "dash")
    );
    if is_shell
        && let Some(flag_index) = tokens.iter().skip(1).position(|t| is_shell_command_flag(t))
    {
        // `position` is relative to the skipped slice; +1 realigns to `tokens`,
        // and the script string is the token right after the flag.
        let script = tokens.get(flag_index + 2)?;
        return script.split_whitespace().next().map(trim_quotes);
    }

    let effective = tokens.iter().find(|token| {
        let bare = trim_quotes(token);
        !is_var_assignment(bare) && !RUN_PREFIXES.contains(&basename(bare).as_deref().unwrap_or(""))
    })?;
    Some(trim_quotes(effective))
}

fn trim_quotes(token: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = token
            .strip_prefix(quote)
            .and_then(|t| t.strip_suffix(quote))
        {
            return inner;
        }
    }
    token
}

fn basename(command: &str) -> Option<String> {
    let command = trim_quotes(command);
    if command.is_empty() || command.starts_with('(') || command.contains('=') {
        return None;
    }
    let base = command.rsplit('/').next()?;
    (!base.is_empty())
        .then_some(base)
        .and_then(ident)
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use time::UtcOffset;

    use crate::collector::codex::collect;
    use crate::collector::codex::tests::write_lines;
    use crate::collector::json::tests::over_structured_line;

    #[test]
    fn decomposes_exec_command_to_real_command_basename() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        let exec = |call: &str, args: &str| {
            format!(
                r#"{{"timestamp":"2026-06-01T00:00:00Z","type":"response_item","payload":{{"type":"function_call","call_id":"{call}","name":"exec_command","arguments":{args}}}}}"#,
            )
        };
        let arg = |inner: &str| serde_json::Value::String(inner.to_owned()).to_string();
        let mut body = String::from(
            r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
        );
        body.push('\n');
        for (call, inner) in [
            ("c1", r#"{"command":["bash","-lc","grep -rn foo src"]}"#),
            ("c2", r#"{"command":["cargo","build"]}"#),
            ("c3", r"not json"),
            ("c4", r#"{"command":["/usr/bin/cat","README.md"]}"#),
            ("c5", r#"{"command":["bash","-c","grep x"]}"#),
            ("c6", r#"{"command":["sudo","cargo","build"]}"#),
            ("c7", r#"{"command":["env","FOO=1","grep","x"]}"#),
            ("c8", r#"{"command":["bash","--norc","-lc","cat y"]}"#),
            ("c9", r#"{"cmd":"ls -la"}"#),
        ] {
            body.push_str(&exec(call, &arg(inner)));
            body.push('\n');
        }
        fs::write(day.join("rollout-session.jsonl"), body).expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let names: Vec<&str> = collection
            .tool_events
            .iter()
            .map(|event| event.tool_name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "grep",         // bash -lc "grep -rn foo src"
                "cargo",        // cargo build
                "exec_command", // non-JSON args -> wrapper fallback
                "cat",          // /usr/bin/cat -> basename
                "grep",         // bash -c "grep x"
                "cargo",        // sudo cargo build -> skip the sudo prefix
                "grep",         // env FOO=1 grep x -> skip env + assignment
                "cat",          // bash --norc -lc "cat y" -> --norc is not the -c flag
                "ls",           // {"cmd":"ls -la"} -> cmd fallback field
            ]
        );
    }

    /// Code Mode's `exec` names no command; the shell commands it starts are
    /// `CommandExecution` items. A shell function call's own item, a command
    /// run from the composer, and one before any `exec` add nothing.
    #[test]
    fn code_mode_exec_counts_the_shell_commands_it_started() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let exec = |call: &str| {
            format!(
                r#"{{"timestamp":"2026-06-01T00:00:01Z","type":"response_item","payload":{{"type":"custom_tool_call","call_id":"{call}","name":"exec","input":"run()"}}}}"#
            )
        };
        let command = |id: &str, source: &str, command: &str| {
            format!(
                r#"{{"timestamp":"2026-06-01T00:00:02Z","type":"event_msg","payload":{{"type":"item_completed","thread_id":"s1","turn_id":"t1","started_at_ms":1,"completed_at_ms":2,"item":{{"type":"CommandExecution","id":"{id}","source":"{source}",{command}}}}}}}"#
            )
        };
        let zsh = |script: &str| format!(r#""command":["/bin/zsh","-lc","{script}"]"#);
        let lines = [
            r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#.to_owned(),
            command("exec-0", "unified_exec_startup", &zsh("pwd")),
            r#"{"timestamp":"2026-06-01T00:00:01Z","type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"exec_command","arguments":"{\"cmd\":\"ls -la\"}"}}"#.to_owned(),
            command("c1", "unified_exec_startup", &zsh("ls -la")),
            exec("e1"),
            command("exec-a", "unified_exec_startup", &zsh("rg foo src")),
            command("exec-b", "unified_exec_startup", &zsh("cargo test")),
            exec("e2"),
            command("exec-u", "user_shell", &zsh("git status")),
            exec("e3"),
            command(
                "exec-c",
                "unified_exec_startup",
                r#""command":42,"parsed_cmd":[{"type":"read","cmd":"cat README.md"}]"#,
            ),
        ];
        write_lines(&temp.path().join("rollout-s1.jsonl"), &lines);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let names: Vec<&str> = collection
            .tool_events
            .iter()
            .map(|event| event.tool_name.as_str())
            .collect();
        assert_eq!(names, vec!["ls", "rg", "cargo", "exec", "cat"]);
    }

    /// A command item written again counts once: the first command names the
    /// `exec`, so its replay neither adds a tool under its own id nor names
    /// the next `exec`, and a command from before any `exec` stays outside.
    #[test]
    fn a_replayed_command_counts_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let exec = |call: &str| {
            format!(
                r#"{{"timestamp":"2026-06-01T00:00:01Z","type":"response_item","payload":{{"type":"custom_tool_call","call_id":"{call}","name":"exec","input":"run()"}}}}"#
            )
        };
        let command = |id: &str, script: &str| {
            format!(
                r#"{{"timestamp":"2026-06-01T00:00:02Z","type":"event_msg","payload":{{"type":"item_completed","thread_id":"s1","turn_id":"t1","item":{{"type":"CommandExecution","id":"{id}","source":"unified_exec_startup","command":["/bin/zsh","-lc","{script}"]}}}}}}"#
            )
        };
        let lines = [
            r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#.to_owned(),
            command("exec-0", "pwd"),
            exec("e1"),
            command("exec-a", "rg foo src"),
            command("exec-a", "rg foo src"),
            command("exec-b", "cargo test"),
            command("exec-b", "cargo test"),
            exec("e2"),
            command("exec-0", "pwd"),
            command("exec-a", "rg foo src"),
        ];
        write_lines(&temp.path().join("rollout-s1.jsonl"), &lines);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let names: Vec<&str> = collection
            .tool_events
            .iter()
            .map(|event| event.tool_name.as_str())
            .collect();
        assert_eq!(names, vec!["rg", "cargo", "exec"]);
    }

    /// An item that names no command adds nothing, so a later copy of it that
    /// does still counts.
    #[test]
    fn a_command_item_without_a_command_does_not_settle_its_id() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let item = |id: &str, command: &str| {
            format!(
                r#"{{"timestamp":"2026-06-01T00:00:02Z","type":"event_msg","payload":{{"type":"item_completed","thread_id":"s1","turn_id":"t1","item":{{"type":"CommandExecution","id":"{id}","source":"unified_exec_startup",{command}}}}}}}"#
            )
        };
        let lines = [
            r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#.to_owned(),
            r#"{"timestamp":"2026-06-01T00:00:01Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"e1","name":"exec","input":"run()"}}"#.to_owned(),
            item("exec-a", r#""command":["/bin/zsh","-lc","ls"]"#),
            item("exec-c", r#""command":42"#),
            item("exec-c", r#""command":["/bin/zsh","-lc","rg foo src"]"#),
        ];
        write_lines(&temp.path().join("rollout-s1.jsonl"), &lines);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let names: Vec<&str> = collection
            .tool_events
            .iter()
            .map(|event| event.tool_name.as_str())
            .collect();
        assert_eq!(names, vec!["ls", "rg"]);
    }

    /// An `exec` call written again goes back to its first tool event: a
    /// command after the replay is the call's next command, not its first.
    #[test]
    fn a_replayed_exec_call_keeps_its_named_tool() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let exec = |call: &str| {
            format!(
                r#"{{"timestamp":"2026-06-01T00:00:01Z","type":"response_item","payload":{{"type":"custom_tool_call","call_id":"{call}","name":"exec","input":"run()"}}}}"#
            )
        };
        let command = |id: &str, script: &str| {
            format!(
                r#"{{"timestamp":"2026-06-01T00:00:02Z","type":"event_msg","payload":{{"type":"item_completed","thread_id":"s1","turn_id":"t1","item":{{"type":"CommandExecution","id":"{id}","source":"unified_exec_startup","command":["/bin/zsh","-lc","{script}"]}}}}}}"#
            )
        };
        let meta = r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#.to_owned();
        let replayed = [
            meta,
            exec("e1"),
            command("exec-a", "rg foo src"),
            exec("e1"),
            command("exec-a", "rg foo src"),
        ];
        for (lines, expected) in [
            (replayed.to_vec(), vec!["rg"]),
            (
                [replayed.as_slice(), &[command("exec-b", "cargo test")]].concat(),
                vec!["rg", "cargo"],
            ),
        ] {
            write_lines(&temp.path().join("rollout-s1.jsonl"), &lines);

            let collection = collect(temp.path(), None, false, UtcOffset::UTC);

            let names: Vec<&str> = collection
                .tool_events
                .iter()
                .map(|event| event.tool_name.as_str())
                .collect();
            assert_eq!(names, expected);
        }
    }

    /// Arguments are JSON inside a string, invisible to the line's own
    /// structure count, so they are bounded again when parsed.
    #[test]
    fn over_structured_arguments_fall_back_to_the_wrapper() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let inner = format!(r#"{{"command":["ls"],"pad":{}}}"#, over_structured_line());
        let line = format!(
            r#"{{"timestamp":"2026-06-01T00:00:00Z","type":"response_item","payload":{{"type":"function_call","call_id":"c1","name":"exec_command","arguments":{}}}}}"#,
            serde_json::Value::String(inner)
        );
        fs::write(temp.path().join("rollout-s1.jsonl"), line).expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.tool_events[0].tool_name, "exec_command");
    }

    /// Arguments are command text, not an id: a long command still names its
    /// tool.
    #[test]
    fn long_arguments_still_name_the_command() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let script = format!("grep x {}", "y".repeat(4 << 10));
        let inner = serde_json::json!({ "command": ["bash", "-lc", script] }).to_string();
        let line = format!(
            r#"{{"timestamp":"2026-06-01T00:00:00Z","type":"response_item","payload":{{"type":"function_call","call_id":"c1","name":"exec_command","arguments":{}}}}}"#,
            serde_json::Value::String(inner)
        );
        fs::write(temp.path().join("rollout-s1.jsonl"), line).expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.tool_events[0].tool_name, "grep");
    }

    #[test]
    fn only_the_leading_tokens_are_split() {
        let huge = serde_json::Value::String("a ".repeat(4 << 20));
        let tokens = super::command_tokens(&huge).expect("tokens");
        assert_eq!(tokens.len(), super::MAX_COMMAND_TOKENS);

        let buried = serde_json::Value::String(format!(
            "{}grep x",
            "A=1 ".repeat(super::MAX_COMMAND_TOKENS)
        ));
        let tokens = super::command_tokens(&buried).expect("tokens");
        assert_eq!(super::effective_command(&tokens), None);
    }
}
