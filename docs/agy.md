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
could shift on an Antigravity update, **every row is checked against the stored
output total** (`#3`): it must equal text + thinking (`#9` + `#10`). A row that
fails is skipped and counted as a parse error rather than contributing garbage
tokens.

What that check does and doesn't guarantee, stated honestly:

- It catches **field renumbering** — the most likely kind of format drift.
  Inserting or removing a field anywhere ahead of the output fields shifts what
  `#9`/`#10` decode to, so the `#3 == #9 + #10` equality breaks and the row is
  dropped.
- It does **not** independently verify the input-side fields (system `#1`, new
  input `#2`, cache read `#5`): there's no stored input total to check them
  against. A pure *semantic* redefinition of those fields that left the wire
  layout untouched would pass the output check and isn't detectable by any
  checksum. That case has never been observed, but it's the residual risk.

So a typical format change degrades to "no/less data"; the one gap is a silent
input-field re-meaning, which we accept because no row-level invariant can catch
it. See `src/collector/agy_conv.rs`.

## Cost

Cost is estimated from the LiteLLM table, the same as every other agent. The
gen blob carries two model labels: an internal id (`#19`, e.g.
`gemini-3-flash-a` — a version-mismatched codename) and a display name (`#21`,
e.g. `Gemini 3.5 Flash (High)`). The collector uses the **display name** — it's
the product-accurate label, and after normalization (drop the `(tier)`,
hyphenate spaces) it maps straight to LiteLLM's bare id (`gemini-3.5-flash`).
A row with no display name falls back to the internal id, which is usually
versionless (`gemini-pro-default`) and stays $0 rather than guess a generation.
