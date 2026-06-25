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

Cost is estimated from the LiteLLM table, the same as every other agent. The
gen blob carries two model labels: an internal id (`#19`, e.g.
`gemini-3-flash-a` — a version-mismatched codename) and a display name (`#21`,
e.g. `Gemini 3.5 Flash (High)`). The collector uses the **display name** — it's
the product-accurate label, and after normalization (drop the `(tier)`,
hyphenate spaces) it maps straight to LiteLLM's bare id (`gemini-3.5-flash`).
A row with no display name falls back to the internal id, which is usually
versionless (`gemini-pro-default`) and stays $0 rather than guess a generation.
