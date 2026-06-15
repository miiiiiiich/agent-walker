# Codex CLI

## Where the data comes from

`~/.codex/sessions/**/*.jsonl` — rollout session logs. Read-only.

## What's captured

- **Token usage** from `token_count` events (`last_token_usage`): input, cached
  input, output, reasoning output, total.
- **Tools** from `response_item` payloads (`exec_command`, `apply_patch`,
  `write_stdin`, …).
- **Durations** from `task_complete` events (`duration_ms`) → the COMPLETION
  section.
- **Sessions / model / project** from `session_meta` and `turn_context`.

## How tokens are counted

OpenAI reports `cached_input_tokens` *inside* `input_tokens`. agent-walker
subtracts it, so `input` means **fresh (uncached) input**, and stores the cached
part as **cache_read** — the same schema as Claude:

```
tokens = input + output + cache_read
```

So the Combined total adds up consistently with Claude. Two real differences
(behaviour, not accounting):

- **Lower cache ratio.** OpenAI caches less aggressively than Claude Code's
  full-context-every-turn, so Codex's cache_read share is smaller — Codex totals
  are more "fresh work" per token.
- **No cache writes.** OpenAI does not bill cache creation, so there is no
  cache_creation component.
- `reasoning_output_tokens` is a **subset of** `output` (already counted inside
  it), not added on top.

## Caveats

- **`exec_command` is a shell catch-all.** It is the majority of Codex tool
  calls; read / run / search all collapse into it. The tool mix looks
  shell-heavy, but the commands inside are often review/read. Do not read
  "84% exec_command" as "84% shell work."
- `apply_patch` = file edits.

## Cost

Same cache-aware formula as Claude. GPT models price cache reads at ≈0.1× input
and have no cache-write cost (OpenAI caches automatically and bills no write);
rates come from LiteLLM. Sessions that log only the provider name are priced as
the current Codex default model. The same API-equivalent caveat applies — it is
not your ChatGPT subscription bill.

## Why this won't match Codex's own usage views

Same idea as Claude — different window and unit:

- **`codex /status`** shows tokens **remaining** against rolling 5-hour and
  weekly plan limits (a percentage of your quota), not a cumulative N-day count.
- **OpenAI's Platform usage dashboard** is per monthly billing cycle; cached
  tokens are shown but folded into the totals.

agent-walker reports a raw, cache-inclusive token count over a trailing N-day
window, so it won't equal either. Use it for cross-agent comparison, not to
reconcile your OpenAI bill.
