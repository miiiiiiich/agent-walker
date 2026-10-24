# Grok Build

## Where the data comes from

`~/.grok/sessions/<encoded-cwd>/<session-id>/updates.jsonl` — one directory
per session, written by Grok Build (xAI's agentic CLI, OSS at
`xai-org/grok-build`). The root is overridable with `GROK_HOME`.
Auto-detected: the Grok tab appears only when session logs
exist. Everything is read locally; nothing leaves the machine.

## What's captured

- **Token usage** from the durable `turn_completed` update every prompt ends
  with: a per-prompt delta carrying
  `inputTokens` / `outputTokens` / `cachedReadTokens` / `cacheCreationTokens` /
  `reasoningTokens`, split per model via `modelUsage`, mapped to the same
  schema as the other providers:

  ```
  tokens = (input − cached_read − cache_creation) + output + cached_read + cache_creation
  ```

  On this wire `inputTokens` is the full prompt: it includes the cached reads
  and the cache-creation tokens (Grok Build's `PromptUsageModel`), so fresh
  input is the difference — the same convention as Codex and Copilot. Older
  logs without `cacheCreationTokens` read it as 0. `reasoningTokens` is a
  subset of `outputTokens` and is tracked without being double-added.
  `costUsdTicks` is not read: cost is priced from LiteLLM like every other
  provider, so a missing or scrubbed tick count can't turn into $0.

- **Model** per entry in `modelUsage` (e.g. `grok-4.5`).
- **Turn durations** from `apiDurationMs` — the time models spent working on
  the prompt, the closest durable duration signal the log carries.
- **Project** from the session's working directory (`summary.json`).
- **Tools** from `tool_call` updates, deduplicated by call id (fork copies keep it).
- **Activity** from every timestamped update in the session stream.

## Deduplication

- **Forks** copy `updates.jsonl` into the new session directory with
  envelope timestamps rewritten to the fork instant — but `prompt_id` is
  preserved, so usage is deduplicated globally by prompt id and a fork copy
  collapses into its original (keeping the original's timestamp).
- **Subagent sessions** (parallel `task` runs) get their own top-level
  session directories (`sessions/<encoded child cwd>/<subagent id>/`), marked
  `session_kind: "subagent…"` in `summary.json`, while the coordinator folds
  their usage into its own turn totals. The parent's
  `<session>/subagents/<subagent id>/` holds only the run's `meta.json` and
  `output.json`, no update stream, so it is not walked. Their usage and
  turn durations are therefore suppressed (the parent already carries the
  tokens) — but their unique tool calls and activity ARE kept, attributed
  as subagent work.
- **Resume** appends to the same directory; nothing is copied.
- Files beside the session directories (`sessions/session_search.sqlite`, the
  `.cwd` marker of a hash-encoded cwd directory) are skipped, not reported as
  unreadable.

## Caveats

- Usage is recorded per prompt at turn completion; a prompt cancelled before
  its `turn_completed` lands contributes activity but no tokens.
- The CLI's billing-credit view (`/usage`) is a backend API, not a local
  ledger — agent-walker reads only the local token records.
