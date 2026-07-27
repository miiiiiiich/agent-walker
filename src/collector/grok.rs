use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use crate::collector::{
    FileEvents, KeyedToolEvent, KeyedUsageEvent, merge_into, parse_files_cached, project_from_cwd,
};
use crate::model::{
    Collection, DurationEvent, Provider, SessionTouch, SourceKind, TokenUsage, ToolEvent,
    UsageEvent,
};

/// Grok Build (xAI's agentic CLI, OSS at `xai-org/grok-build`) writes one
/// directory per session under `<root>/sessions/<encoded-cwd>/<session-id>/`,
/// with an ACP update stream in `updates.jsonl`. Schema verified against the
/// source and live logs (v0.2.x):
///
/// - Every prompt ends with a durable `turn_completed` update carrying a full
///   per-prompt usage delta (input / output / cachedRead / reasoning, plus a
///   per-model split in `modelUsage`) and a `prompt_id`.
/// - Subagent runs get their own session directory, marked by
///   `summary.json`'s `session_kind: "subagent*"`, while the coordinator
///   folds their usage into its own turn totals — so subagent directories
///   are EXCLUDED wholesale or the fold would double-count.
/// - Forking copies `updates.jsonl` into the new session directory with
///   envelope timestamps rewritten to the fork instant (the same shape as
///   the Codex fork replay, GH-36) but `prompt_id` preserved — so usage is
///   deduplicated globally by `prompt_id`, under which a fork copy collapses
///   into its original and the earliest timestamp wins.
/// - Resuming appends to the same directory; no copy is involved.
pub fn collect(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    use_cache: bool,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Grok, root.to_path_buf());
    let sessions = root.join("sessions");
    if !sessions.exists() {
        return collection;
    }

    // Enumerate `<cwd>/<session>/updates.jsonl` explicitly — session dirs
    // also hold subagent/terminal/compaction artifacts a recursive glob
    // would pick up.
    let mut files: Vec<PathBuf> = Vec::new();
    let Ok(cwd_dirs) = std::fs::read_dir(&sessions) else {
        collection.stats.unreadable_dirs += 1;
        return collection;
    };
    for cwd_dir in cwd_dirs {
        let Ok(cwd_dir) = cwd_dir else {
            collection.stats.unreadable_files += 1;
            continue;
        };
        let Ok(session_dirs) = std::fs::read_dir(cwd_dir.path()) else {
            continue;
        };
        for session_dir in session_dirs.flatten() {
            let dir = session_dir.path();
            // Subagent sessions are folded into their coordinator's turn
            // totals; counting their own directory too would double-count.
            // An unreadable/corrupt summary is skipped for the same reason —
            // it might be hiding a subagent marker (fail closed) — and
            // surfaced in the stats. This also means every parsed session
            // had a readable summary, so the project/cwd read in parse_file
            // can only go stale in the brief window before a brand-new
            // session writes its summary (bounded by the next turn append
            // invalidating the updates.jsonl cache entry).
            match session_kind(&dir) {
                Ok(Some(kind)) if kind.starts_with("subagent") => continue,
                Ok(_) => {}
                Err(()) => {
                    collection.stats.unreadable_files += 1;
                    continue;
                }
            }
            let path = dir.join("updates.jsonl");
            let meta = match std::fs::metadata(&path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    collection.stats.unreadable_files += 1;
                    continue;
                }
            };
            if let (Some(floor), Ok(mtime)) = (mtime_floor, meta.modified())
                && mtime < floor
            {
                continue;
            }
            files.push(path);
        }
    }
    if files.is_empty() {
        return collection;
    }
    files.sort();

    let per_file = parse_files_cached(use_cache.then_some("grok"), &files, local_offset, |path| {
        parse_file(path, local_offset)
    });
    merge_into(&mut collection, per_file);
    collection
}

/// `session_kind` from the sibling `summary.json`. `Ok(None)` covers both a
/// missing file and a summary without the field — ordinary sessions. An
/// existing-but-unreadable or corrupt summary is `Err`: the caller must NOT
/// fail open and treat the directory as an ordinary session, because if it
/// was actually a subagent its usage is already folded into the coordinator
/// and counting it would double-count.
fn session_kind(session_dir: &Path) -> Result<Option<String>, ()> {
    let raw = match std::fs::read_to_string(session_dir.join("summary.json")) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let summary = serde_json::from_str::<Value>(&raw).map_err(|_| ())?;
    Ok(summary
        .get("session_kind")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned))
}

fn parse_file(path: &Path, local_offset: UtcOffset) -> Option<FileEvents> {
    let file = File::open(path).ok()?;
    let session_dir = path.parent()?;
    let session_id = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned);
    // The raw cwd lives in summary.json (the directory name is a lossy
    // encoding); fall back to no project when it can't be read.
    let summary = std::fs::read_to_string(session_dir.join("summary.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    let project = summary.as_ref().and_then(|summary| {
        summary
            .get("info")
            .and_then(|info| info.get("cwd"))
            .and_then(Value::as_str)
            .map(project_from_cwd)
    });
    // Fork/worktree sessions start as a copy of their parent's update stream.
    // Usage dedupes by prompt_id and tools by call id, but duration events
    // carry no key — so forks skip duration collection entirely rather than
    // re-count the copied turns (the fork's own new turns lose their
    // durations; the copied majority would otherwise double).
    let is_fork_copy = summary
        .as_ref()
        .and_then(|summary| summary.get("session_kind"))
        .and_then(Value::as_str)
        .is_some();
    let mut events = FileEvents::default();
    let reader = BufReader::new(file);

    for line in reader.lines() {
        events.lines_seen += 1;
        let Ok(line) = line else {
            events.parse_errors += 1;
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            events.parse_errors += 1;
            continue;
        };

        // Envelope timestamps are UNIX seconds.
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_u64)
            .and_then(|secs| OffsetDateTime::from_unix_timestamp(i64::try_from(secs).ok()?).ok());
        if let (Some(timestamp), Some(session_id)) = (timestamp, session_id.as_ref()) {
            events.session_touches.push(SessionTouch {
                timestamp,
                session_id: session_id.clone(),
            });
        }

        let Some(update) = value.get("params").and_then(|params| params.get("update")) else {
            continue;
        };
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("turn_completed") => {
                collect_turn_usage(
                    update,
                    timestamp,
                    session_id.as_ref(),
                    project.as_deref(),
                    is_fork_copy,
                    &mut events,
                );
            }
            Some("tool_call") => {
                collect_tool_event(update, timestamp, session_id.as_ref(), &mut events);
            }
            _ => {}
        }
    }

    events.compress_touches(local_offset);
    Some(events)
}

/// Usage from one `turn_completed`: a per-prompt delta (not a cumulative),
/// split per model via `modelUsage`. `inputTokens` includes
/// `cachedReadTokens` (the source comments the cached figure as a subset),
/// so fresh input is the difference — the same convention as Codex and
/// Copilot; `reasoningTokens` is a subset of `outputTokens`. Keyed by
/// `prompt_id`, which fork copies preserve while their envelope timestamps
/// are rewritten — the copy collapses into the original.
fn collect_turn_usage(
    update: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    project: Option<&str>,
    skip_durations: bool,
    events: &mut FileEvents,
) {
    let Some(prompt_id) = update.get("prompt_id").and_then(Value::as_str) else {
        return;
    };
    let Some(usage) = update.get("usage") else {
        return;
    };
    // Per-model entries when present; the totals as a single unattributed
    // event otherwise. Both cannot be counted together — modelUsage sums to
    // the totals.
    let model_usage = usage.get("modelUsage").and_then(Value::as_object);
    let entries: Vec<(Option<String>, &Value)> = match model_usage {
        Some(models) if !models.is_empty() => models
            .iter()
            .map(|(model, entry)| (Some(model.clone()), entry))
            .collect(),
        _ => vec![(None, usage)],
    };
    for (model, entry) in entries {
        let input = u64_field(entry, "inputTokens");
        let output = u64_field(entry, "outputTokens");
        let cached = u64_field(entry, "cachedReadTokens");
        let usage = TokenUsage {
            input_tokens: input.saturating_sub(cached),
            output_tokens: output,
            reasoning_output_tokens: u64_field(entry, "reasoningTokens"),
            cache_read_input_tokens: cached,
            ..TokenUsage::default()
        };
        if usage.token_volume() == 0 {
            continue;
        }
        let key = match &model {
            Some(model) => format!("grok:{prompt_id}:{model}"),
            None => format!("grok:{prompt_id}"),
        };
        events.usage_events.push(KeyedUsageEvent {
            key: Some(key),
            event: UsageEvent {
                timestamp,
                session_id: session_id.cloned(),
                model,
                source_kind: SourceKind::Main,
                attribution_agent: None,
                attribution_skill: None,
                project: project.map(ToOwned::to_owned),
                usage,
                reported_cost_usd: None,
            },
        });
    }

    // `apiDurationMs` is the time models spent working on this prompt — the
    // closest durable duration signal the log carries.
    if !skip_durations
        && let Some(duration_ms) = usage.get("apiDurationMs").and_then(Value::as_u64)
        && duration_ms > 0
    {
        events.duration_events.push(DurationEvent {
            timestamp,
            session_id: session_id.cloned(),
            duration_ms,
            status: Some("turn".to_owned()),
        });
    }
}

fn collect_tool_event(
    update: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    events: &mut FileEvents,
) {
    let Some(tool_name) = update.get("title").and_then(Value::as_str) else {
        return;
    };
    // Keyed GLOBALLY by call id (a UUID, `call-<uuid>-<n>`): a fork copies
    // the update stream into a new session directory with the call ids
    // preserved, so a session-scoped key would count every copied tool call
    // again under the new session id.
    let key = update
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(|id| format!("grok-tool:{id}"));
    events.tool_events.push(KeyedToolEvent {
        key,
        event: ToolEvent {
            timestamp,
            session_id: session_id.cloned(),
            tool_name: tool_name.to_owned(),
            subagent_type: None,
            source_kind: SourceKind::Main,
        },
    });
}

/// Token counts are untrusted log data; clamp to a generous sanity bound so
/// downstream sums of a handful of fields can never overflow u64.
fn u64_field(value: &Value, key: &str) -> u64 {
    const MAX_SANE_TOKENS: u64 = 1 << 50;
    value
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_SANE_TOKENS)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_session(root: &Path, cwd: &str, session: &str, updates: &str, summary: Option<&str>) {
        let dir = root.join("sessions").join(cwd).join(session);
        fs::create_dir_all(&dir).expect("test dirs should be created");
        fs::write(dir.join("updates.jsonl"), updates).expect("fixture should be written");
        if let Some(summary) = summary {
            fs::write(dir.join("summary.json"), summary).expect("summary should be written");
        }
    }

    const TURN: &str = r#"{"timestamp":1785170203,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","stop_reason":"end_turn","usage":{"inputTokens":264488,"outputTokens":4276,"totalTokens":268764,"cachedReadTokens":148864,"reasoningTokens":1789,"modelCalls":8,"apiDurationMs":85907,"modelUsage":{"grok-4.5":{"inputTokens":264488,"outputTokens":4276,"totalTokens":268764,"cachedReadTokens":148864,"reasoningTokens":1789,"modelCalls":8,"apiDurationMs":85907}},"numTurns":8}}}}"#;

    #[test]
    fn collects_turn_usage_tools_and_project() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let updates = format!(
            "{TURN}\n{}\n",
            r#"{"timestamp":1785170100,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"read_file"}}}"#,
        );
        write_session(
            temp.path(),
            "cwd",
            "s1",
            &updates,
            Some(
                r#"{"info":{"id":"s1","cwd":"/Users/me/code/app"},"current_model_id":"grok-4.5"}"#,
            ),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        let event = &collection.usage_events[0];
        assert_eq!(event.model.as_deref(), Some("grok-4.5"));
        // inputTokens includes cachedReadTokens: fresh = 264,488 - 148,864.
        assert_eq!(event.usage.input_tokens, 115_624);
        assert_eq!(event.usage.cache_read_input_tokens, 148_864);
        // Volume = input(incl. cached) + output; reasoning stays a subset.
        assert_eq!(event.usage.token_volume(), 264_488 + 4_276);
        assert_eq!(event.usage.reasoning_output_tokens, 1_789);
        assert_eq!(event.project.as_deref(), Some("Users/me/code/app"));
        assert_eq!(collection.tool_events.len(), 1);
        assert_eq!(collection.tool_events[0].tool_name, "read_file");
        assert_eq!(collection.duration_events.len(), 1);
        assert_eq!(collection.duration_events[0].duration_ms, 85_907);
    }

    /// A fork copies `updates.jsonl` into a new session directory (possibly
    /// under a different encoded cwd) with envelope timestamps rewritten to
    /// the fork instant but `prompt_id` and tool-call ids preserved — usage
    /// and tools must collapse into the originals (keeping the original's
    /// earlier timestamp), and the copied turns must not re-emit durations.
    #[test]
    fn fork_copy_dedupes_usage_tools_and_durations() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let tool = r#"{"timestamp":1785170100,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"read_file"}}}"#;
        write_session(temp.path(), "cwd", "s1", &format!("{TURN}\n{tool}\n"), None);
        // The fork copy in a DIFFERENT encoded cwd: same updates, rewritten
        // timestamps, new dir, kind marker.
        let copy = format!("{TURN}\n{tool}\n")
            .replace("\"timestamp\":1785170203", "\"timestamp\":1785999999");
        write_session(
            temp.path(),
            "other-cwd",
            "s2-fork",
            &copy,
            Some(r#"{"session_kind":"fork","parent_session_id":"s1"}"#),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        let event = &collection.usage_events[0];
        assert_eq!(event.usage.token_volume(), 264_488 + 4_276);
        // The original's timestamp survives, not the fork instant.
        assert_eq!(
            event.timestamp.map(OffsetDateTime::unix_timestamp),
            Some(1_785_170_203)
        );
        // Tool call ids are global, so the copied call collapses too.
        assert_eq!(collection.tool_events.len(), 1);
        // Durations carry no dedup key: the original's counts, the fork's
        // copied turn does not re-emit.
        assert_eq!(collection.duration_events.len(), 1);
    }

    /// A fork whose parent is gone (deleted or outside the mtime window)
    /// still counts once — `prompt_id` dedup simply has nothing to collide
    /// with.
    #[test]
    fn orphan_fork_counts_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "cwd",
            "s2-fork",
            &format!("{TURN}\n"),
            Some(r#"{"session_kind":"fork","parent_session_id":"gone"}"#),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(
            collection.usage_events[0].usage.token_volume(),
            264_488 + 4_276
        );
    }

    /// A corrupt or unreadable summary.json fails CLOSED: the directory
    /// might be hiding a subagent marker, so it is skipped and surfaced in
    /// the stats instead of being counted as an ordinary session.
    #[test]
    fn corrupt_summary_fails_closed() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "cwd",
            "s1",
            &format!("{TURN}\n"),
            Some("not json at all"),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 0);
        assert_eq!(collection.stats.unreadable_files, 1);
    }

    /// `subagent_fork` / `subagent_resume` variants are excluded like plain
    /// `subagent`.
    #[test]
    fn subagent_kind_variants_are_excluded() {
        let temp = TempDir::new().expect("test tempdir should be created");
        for (session, kind) in [("a", "subagent_fork"), ("b", "subagent_resume")] {
            write_session(
                temp.path(),
                "cwd",
                session,
                &format!("{TURN}\n"),
                Some(&format!(r#"{{"session_kind":"{kind}"}}"#)),
            );
        }

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 0);
    }

    /// Subagent sessions are folded into their coordinator's totals — their
    /// own directories must be excluded wholesale.
    #[test]
    fn subagent_sessions_are_excluded() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_session(
            temp.path(),
            "cwd",
            "sub1",
            &format!("{TURN}\n"),
            Some(r#"{"session_kind":"subagent"}"#),
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 0);
    }

    /// A multi-model turn splits per model from `modelUsage`; the totals are
    /// never counted on top of the split.
    #[test]
    fn multi_model_turn_splits_per_model() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let line = r#"{"timestamp":1785170203,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","usage":{"inputTokens":300,"outputTokens":30,"cachedReadTokens":0,"reasoningTokens":0,"modelUsage":{"grok-4.5":{"inputTokens":200,"outputTokens":20,"cachedReadTokens":0,"reasoningTokens":0},"grok-4.5-mini":{"inputTokens":100,"outputTokens":10,"cachedReadTokens":0,"reasoningTokens":0}}}}}}"#;
        write_session(temp.path(), "cwd", "s1", &format!("{line}\n"), None);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 220 + 110);
    }

    /// A malformed line is skipped without aborting the file; a
    /// `turn_completed` without usage contributes nothing.
    #[test]
    fn malformed_lines_and_missing_usage_are_skipped() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let updates = format!(
            "not json\n{}\n{TURN}\n",
            r#"{"timestamp":1785170000,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"turn_completed","prompt_id":"p0","stop_reason":"cancelled"}}}"#,
        );
        write_session(temp.path(), "cwd", "s1", &updates, None);

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.parse_errors, 1);
        assert_eq!(collection.usage_events.len(), 1);
    }
}
