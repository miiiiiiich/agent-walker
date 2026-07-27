# Grok Build

## Where the data comes from

`~/.grok/sessions/<encoded-cwd>/<session-id>/updates.jsonl` — one directory
per session, written by Grok Build (xAI's agentic CLI, OSS at
`xai-org/grok-build`). The root is overridable with `GROK_HOME` or
`--grok-dir`. Auto-detected: the Grok tab appears only when session logs
exist. Everything is read locally; nothing leaves the machine.

## What's captured

- **Token usage** from the durable `turn_completed` update every prompt ends
  with: a per-prompt delta carrying
  `inputTokens` / `outputTokens` / `cachedReadTokens` / `reasoningTokens`,
  split per model via `modelUsage`, mapped to the same schema as the other
  providers:

  ```
  tokens = (input − cached_read) + output + cached_read
  ```

  `inputTokens` includes the cached reads, so fresh input is the difference —
  the same convention as Codex and Copilot. `reasoningTokens` is a subset of
  `outputTokens` and is tracked without being double-added.

- **Model** per entry in `modelUsage` (e.g. `grok-4.5`).
- **Turn durations** from `apiDurationMs` — the time models spent working on
  the prompt, the closest durable duration signal the log carries.
- **Project** from the session's working directory (`summary.json`).
- **Tools** from `tool_call` updates, deduplicated per session and call id.
- **Activity** from every timestamped update in the session stream.

## Deduplication

- **Forks** copy `updates.jsonl` into the new session directory with
  envelope timestamps rewritten to the fork instant — but `prompt_id` is
  preserved, so usage is deduplicated globally by prompt id and a fork copy
  collapses into its original (keeping the original's timestamp).
- **Subagent sessions** (parallel `task` runs) get their own directories,
  marked `session_kind: "subagent…"` in `summary.json`, while the
  coordinator folds their usage into its own turn totals. Those directories
  are excluded wholesale — the parent already carries the tokens.
- **Resume** appends to the same directory; nothing is copied.

## Caveats

- Usage is recorded per prompt at turn completion; a prompt cancelled before
  its `turn_completed` lands contributes activity but no tokens.
- The CLI's billing-credit view (`/usage`) is a backend API, not a local
  ledger — agent-walker reads only the local token records.
