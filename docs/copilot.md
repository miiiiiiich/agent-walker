# GitHub Copilot CLI

## Where the data comes from

`~/.copilot/session-state/<uuid>/events.jsonl` — one directory per session,
written by the agentic GitHub Copilot CLI (`@github/copilot`; not the retired
`gh copilot` extension). The root is overridable with `COPILOT_HOME` or
`--copilot-dir`. Auto-detected: the Copilot tab appears only when session logs
exist. Everything is read locally; nothing leaves the machine.

## What's captured

- **Token usage** from the `session.shutdown` event the CLI appends on clean
  exit (`/exit`, or a non-interactive run completing). It carries per-model
  cumulative totals for the whole session
  (`inputTokens` / `outputTokens` / `cacheReadTokens` / `cacheWriteTokens` /
  `reasoningTokens`). Each shutdown is counted as the delta since the previous
  one, dated at its own exit time, and mapped to the same schema as the other
  providers:

  ```
  tokens = (input − cache_read) + output + cache_read + cache_write
  ```

  `inputTokens` includes the cached reads (verified against the CLI's own
  on-screen totals), so fresh input is the difference — the same convention
  as Codex. `reasoningTokens` is a subset of `outputTokens` and is tracked
  without being double-added.

- **Model** per entry in `modelMetrics` (e.g. `gpt-5-mini`,
  `claude-sonnet-4.6`).
- **Project** from the session's working directory (`session.start`).
- **Tools** from `tool.execution_start` events, deduplicated by tool-call id.
- **Activity** from every timestamped event in the session stream.

## Caveats

- **Sessions that never exit cleanly have no token data.** The CLI writes
  token totals only at shutdown; a crashed or still-open session contributes
  activity but no tokens. The gap closes when the session eventually exits —
  the cumulative shutdown covers its whole lifetime.
- **Resumed sessions**: `--resume` appends to the same session, and each
  later clean exit appends another shutdown with cumulative totals. Every
  exit contributes only what happened since the previous one — cumulatives
  are never summed, and a counter going backwards (CLI update) starts a new
  epoch counted in full.
- **Day-level granularity is only as fine as your exit habits**: everything
  between two clean exits lands on the later exit's day.
- The CLI's AI-credit accounting (`totalNanoAiu`, premium requests) is not
  surfaced; agent-walker reports token volume and API-equivalent cost like
  every other tab.
