# Antigravity CLI (agy · Gemini)

## Where the data comes from

`~/.gemini/antigravity-cli` (read-only, auto-detected; point elsewhere with
`--agy-dir`):

- `history.jsonl` + `log/cli-*.log` → the session / tool **activity** timeline.
- `conversations/<uuid>.db` (SQLite) → **token usage, model, and project**, one
  row per generation in the `gen_metadata` table.

## What's captured

- ✅ **Tokens** — per generation: input (system prompt + new input), cache-read,
  output (text), and thinking/reasoning. Thinking is folded into output so it
  counts toward the total (same convention as OpenCode).
- ✅ **Model** — e.g. `gemini-3-flash-agent`.
- ✅ **Project** — the workspace path recorded in the conversation.
- ✅ **Activity** — session timestamps → active days, hourly profile, streak.
- ✅ **Some tools** — the commands you *confirmed* in approval dialogs
  (`command:<exe>`) from the logs.

## The tokens are unlabeled protobuf — and self-verified

`gen_metadata` rows are Antigravity's internal **protobuf with no field names**,
and there's no public schema. agent-walker reads the wire format directly and
pulls the few fields it needs by number (cross-checked against the `tokscale`
project, which maps them identically). Because the numbers are unofficial and
could shift on an Antigravity update, **every row is self-verified**: the stored
output total must equal text + thinking. A row that fails is skipped and counted
as a parse error rather than contributing garbage tokens — so a future format
change degrades to "no/less data," never to wrong numbers. See
`src/collector/agy_conv.rs`.

## Cost

Cost is **not** shown yet. Antigravity's model ids (`gemini-3-flash-agent`, …)
don't match the LiteLLM pricing table's ids, and the pricing path is currently
limited to `claude-*` / `gpt-*` models, so gemini usage prices to nothing. Tokens
still count toward the totals; only the COST panel is blank for Antigravity (the
same way OpenCode's local Ollama models count tokens but show $0).
