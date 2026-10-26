use std::path::Path;

use serde_json::Value;
use time::OffsetDateTime;

use crate::collector::ident::ident;
use crate::collector::json::parse_bounded;
use crate::collector::{FileEvents, KeyedToolEvent};
use crate::model::ToolEvent;

use super::fork::Origin;
use super::string_path;

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
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return;
    }
    let Some(raw_name) = string_path(value, &["payload", "name"]) else {
        return;
    };
    // Codex runs most reads and writes through a generic shell wrapper
    // (`exec_command` etc., usually `bash -lc "..."`); resolving the wrapper to
    // the real command basename lets the tool list show `grep`/`cargo` instead of
    // one undifferentiated "exec" bucket.
    let tool_name = if is_shell_wrapper(&raw_name) {
        exec_command_basename(value).unwrap_or(raw_name)
    } else {
        raw_name
    };
    let key = string_path(value, &["payload", "call_id"]).unwrap_or_else(|| {
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
    let command = parsed.get("command").or_else(|| parsed.get("cmd"))?;
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
