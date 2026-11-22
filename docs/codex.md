# Codex CLI

## Where the data comes from

`~/.codex/sessions/**/*.jsonl` and `~/.codex/archived_sessions/*.jsonl` —
rollout session logs. Read-only. The Codex desktop app *moves* (not copies) a
session's JSONL from `sessions/<year>/<month>/<day>/` to `archived_sessions/`
(no date directories) when it's archived, so both are scanned; a session that
briefly exists in both (a stale copy) is deduplicated by file name before
parsing. The name carries the thread's start time and id. `CODEX_HOME`
overrides the root; the `archived_sessions` sibling is derived from the
sessions directory.

**Compressed rollouts.** With `local_thread_store_compression` on, Codex
compresses rollouts whose mtime is at least 7 days old, in both directories,
into a sibling `<name>.jsonl.zst`: one zstd frame (level 3, content size in the
header, no checksum) carrying the original mtime. It writes a temp file,
publishes the `.zst`, then deletes the `.jsonl`; resuming a thread restores the
plain file (mtime again preserved) and deletes the `.zst`. So both can exist
for a moment, and Codex itself prefers the plain one. agent-walker reads both
(pure-Rust decoder) and treats `x.jsonl` and `x.jsonl.zst` as one rollout,
with Codex's rule: the plain file wins whenever one exists (in either
directory); between two of the same kind the larger file wins. The frame
header's content size is not trusted for this choice. Keys come from the plain
name, so a file that flips representation between runs is not counted twice.

Decoding is bounded: a line longer than 64 MiB, or one holding more than
262,144 JSON array elements and object entries, is skipped as a parse error
without being parsed. A `.zst` is decoded to its end like the plain rollout it
came from. Only regular files are opened; a symlink to one is followed, while a
FIFO or device counts as an unreadable file. A timestamp before 1970 or from
year 3000 on is treated as missing. An id or name longer than 1,024 bytes, or a
path longer than 4,096, is treated as missing.

## What's captured

- **Token usage** from `token_usage_record` rows: one per model response,
  keyed by `response_id`, including compaction and incomplete responses that
  never get a `token_count`. Rollouts written before those rows existed fall
  back to `token_count` events (`last_token_usage`). A `token_count` whose
  usage equals an earlier record in the same file is that record's echo
  (sometimes deferred, sometimes byte-identical twice) and is not counted
  again. A `token_count` with only `total_tokens` set is Codex refilling the
  context window after a compaction, not a call, and is skipped. A record
  whose `thread_id` is not the rollout's own thread is a copy and is skipped.
- **Subagents** from the rollout's own (first) `session_meta.source`:
  `{"subagent": {"thread_spawn": …}}` is attributed to its `agent_role`
  (`default` when unset; nicknames are random per spawn), `{"subagent":
  "review" | "compact" | …}` and `{"subagent": {"other": "guardian"}}` to that
  kind, `{"internal": "guardian" | …}` likewise. `cli`, `vscode`, `exec`,
  `mcp` and `{"custom": …}` are main sessions. A spawned fork's second
  `session_meta` is the parent's copy and does not change this.
- **Sessions.** A subagent's calls, tools, turns and activity belong to the
  session it works for, as Claude Code files a subagent under its parent
  session: the root thread its `session_meta.session_id` names. Older CLIs
  wrote the thread's own id there; their subagents take the parent the
  `session_meta` names (`thread_spawn.parent_thread_id`, else
  `parent_thread_id`). Its usage takes that session's project, from the
  parent's `session_meta` a fork copies in. Any other thread is its own
  session, a user's fork included, and keeps the project it was opened in.
- **Tools** from `response_item` payloads (`exec_command`, `apply_patch`,
  `write_stdin`, …). Shell executions (`exec_command` and friends) are
  decomposed to the real command basename (`grep`, `cargo`, …) so the tool list
  reflects what actually ran, not one undifferentiated shell bucket. Code
  Mode's `exec` runs a script instead; the shell commands it starts are
  recorded as `CommandExecution` items, and each counts as a tool named the
  same way.
- **Durations** from `task_complete` events (`duration_ms`) → the TURN LENGTH
  section.
- **Pace** in main sessions: from a `task_complete` to the next prompt.
  Legacy rollouts record the prompt as an `event_msg` `user_message`;
  paginated ones write no `user_message`, so the prompt is the completed
  `UserMessage` item. A subagent's prompts come from the agent that runs it,
  not a person, and are not counted.
- **Model / project** from `session_meta` and `turn_context`.

## How tokens are counted

OpenAI reports `cached_input_tokens` *inside* `input_tokens`. agent-walker
subtracts it, so `input` means **fresh (uncached) input**, and stores the cached
part as **cache_read** — the same schema as Claude:

```
tokens = input + output + cache_read
```

So the Total view adds up consistently with Claude. Two real differences
(behaviour, not accounting):

- **Lower cache ratio.** OpenAI caches less aggressively than Claude Code's
  full-context-every-turn, so Codex's cache_read share is smaller — Codex totals
  are more "fresh work" per token.
- **Cache writes.** Codex reports `cache_write_input_tokens` inside
  `input_tokens`, like cached input; agent-walker moves them to
  **cache_creation**, so `tokens` stays `input_tokens + output_tokens` as Codex
  reports it.
- `reasoning_output_tokens` is a **subset of** `output` (already counted inside
  it), not added on top.

## Caveats

- **`exec_command` is decomposed to the real command.** Codex runs most
  reads / runs / searches through a shell wrapper (`exec_command`, usually
  `bash -lc "…"`); agent-walker extracts the effective command basename
  (`grep`/`cat`/`cargo`/…) so the tool list sees the real work instead of one
  undifferentiated shell bucket. A bare `exec_command` remains only when the
  wrapped command can't be parsed (the fallback bucket).
- **Code Mode `exec`.** One `exec` call can start several shell commands,
  each a `CommandExecution` item (`command`, else `parsed_cmd`) that names no
  call. The first command after an `exec` names that call and each later one
  counts as its own tool, so the tool list counts commands, as it does for
  `exec_command`. An `exec` that started no shell command (only MCP calls,
  file changes or image views) stays `exec`. A shell function call's own
  `CommandExecution` item, and a command run from the composer
  (`source: "user_shell"`), add nothing.
- `apply_patch` = file edits.
- **Fork copies.** A fork or spawned child that copies its parent's records
  writes them within milliseconds of the fork instant: the child's
  `session_meta` (when it carries `forked_from_id` without `history_base`) or
  the copied parent `session_meta`. Rows stamped from that second through one
  second after the instant are skipped, so the parent's usage is counted once
  even when the parent rollout is not in the scan. Rows that name this thread
  as their owner — its own `session_meta` and `token_usage_record`s with its
  `thread_id` — are kept whatever their timestamp. Codex's rollout migration
  rewrote older children with a single `session_meta`; the child's own
  `forked_from_id` still marks them. A fork with `history_base` references
  the parent's rollout instead of copying it. Spawned children never copy
  `token_usage_record` rows.
- **Not on disk.** Background (async) compaction requests are charged to
  Codex's internal budget only; no record or `token_count` is written, so
  agent-walker cannot count them.

## Cost

Same cache-aware formula as Claude. Cache-read rates come from LiteLLM for each
model (0.05×–0.1× input on current GPT models); cache writes use LiteLLM's
cache-creation rate for the model. Where LiteLLM lists no cache-write rate (or
lists 0), writes price at the model's input rate: they are prompt tokens moved
out of `input`, and a $0 rate would make them free. Rates come from LiteLLM, and a model id resolves to its key by the same
rule as Claude: a newer version LiteLLM does not list yet is unpriced (—), not
priced as an older one. Sessions that log only the provider name, and
`codex-auto-review`, are priced as the current Codex default model
(gpt-6.2-sol). The same API-equivalent caveat applies — it is not your ChatGPT
subscription bill.

Long-context tiers are priced per request, by the same rule as Claude: each
`token_count` / `token_usage_record` row is one model response, and a response
whose prompt (fresh input + cached input + cache writes) is over the model's
LiteLLM threshold pays the tier rates on all of its tokens. OpenAI bills
"prompts with more than 272K input tokens" at 2× input and cache rates and
1.5× output for the full request; LiteLLM's `_above_272k_tokens` keys carry
those rates. A write rate the tier leaves out follows the tier's input rate.

Fast and Ultrafast calls pay their service tier's rates. Codex records a
thread's tier in its `thread_settings_applied` events (`service_tier`:
`priority` is Codex's Fast, and `ultrafast`), and each call takes the tier its
thread last applied. It then pays LiteLLM's `_priority` or `_ultrafast` rates
for its model, long-context ones included
(`input_cost_per_token_above_272k_tokens_priority`); a rate the tier does not
list keeps the standard value. A model LiteLLM lists no rates for in that
tier, and any other tier (`default`, `flex`), prices at the standard rates. A
settings event that names another thread does not change the tier; older
events name no thread and count for the rollout's own.

## Why this won't match Codex's own usage views

Same idea as Claude — different window and unit:

- **`codex /status`** shows tokens **remaining** against rolling 5-hour and
  weekly plan limits (a percentage of your quota), not a cumulative N-day count.
- **OpenAI's Platform usage dashboard** is per monthly billing cycle; cached
  tokens are shown but folded into the totals.

agent-walker reports a raw, cache-inclusive token count over a trailing N-day
window, so it won't equal either. Use it for cross-agent comparison, not to
reconcile your OpenAI bill.
