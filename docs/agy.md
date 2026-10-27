# Antigravity (agy · Gemini)

## Where the data comes from

Two stores under `~/.gemini` (read-only, auto-detected), both shown as the one
Antigravity provider:

- `antigravity-cli/` — the agy CLI.
  - `history.jsonl` + `log/cli-*.log` → session timestamps and confirmed
    **tools**.
  - `conversations/<uuid>.db` (SQLite) → **token usage, model, project, and
    session timestamps**, one row per generation in the `gen_metadata` table;
    the `steps` table supplies a row's time when the row itself no longer
    carries one.
- `antigravity/` — the Antigravity app (IDE). Its `conversations/<uuid>.db`
  files have the same schema and are read the same way. The app keeps no CLI
  logs, so its conversations add tokens, active days, and sessions but no
  tools.

The `.pb` files next to the DBs (and the `.pb`-only `antigravity-ide/` and
`antigravity-backup/` directories) are not read.

A generation is counted once across both stores: rows are keyed by their
response id (`#1.#4.#11`), or by conversation id (the file name) plus row index
when a row has none. A conversation present in both stores, even with one copy
further along, contributes each generation once. When the copies date a
generation differently, the one with the strongest time evidence (the order in
[Row time](#row-time)) is kept whichever store is read first, and only then is
the analysis window applied — so a copy dated from its conversation start can't
crowd out an in-window copy dated from its step.

A 0-byte `.db` is a conversation agy has created but not written yet. It is
skipped without being opened (it's still in rollback-journal mode, where even a
read lock would block agy's first write). Conversations agy is still writing are
in WAL mode and are read alongside the writer.

**Input bounds.** Only regular files are opened; a symlink to one is followed,
while a FIFO or device counts as an unreadable file. A line longer than 64 MiB,
or one holding more than 262,144 JSON array elements and object entries, is
skipped as a parse error. The same timestamp range as [Row time](#row-time)
applies to `history.jsonl` and logs. An id or name longer than 1,024 bytes, or
a path longer than 4,096, is treated as missing.

Each conversation DB is opened read-only: SQLite never writes, checkpoints or
recovers it. A WAL-mode store still gets `-wal` / `-shm` files beside it when
they are missing (any reader creates them, and a read-only one can't remove
them); `immutable` mode, which would avoid that, reads torn pages while the
agent writes. A store, `-wal`, `-shm` or `-journal` that is not a regular file
(with a single link, checked on unix) is not opened at all: SQLite truncates
`-shm` even on a read-only open, so a linked one would damage the other file.
Only ordinary tables and their stored columns are read under the names queried
(a view, virtual table or generated column of that name is ignored), a value
over 64 MiB is a read error, a store's queries stop after returning 1 GiB, and
SQLite reads across all of a provider's stores stop after 30 s. These are cheap
checks, not a sandbox: a crafted store planted in an agent's directory is
outside the threat model, since writing there already means running code as the
user.

## What's captured

- ✅ **Tokens** — per generation: input (system prompt + new input), cache-read,
  output (text), and thinking/reasoning. Thinking is folded into output so it
  counts toward the total (same convention as OpenCode).
- ✅ **Model** — e.g. `Gemini 3.5 Flash (High)` or `gemini-3.8-flash` (see
  [Cost](#cost)).
- ✅ **Project** — the workspace path recorded in the conversation.
- ✅ **Sessions / activity** — session timestamps come from every dated
  generation, every `history.jsonl` line, and each logged user input or tool
  confirmation → sessions, active days, streak, daily session counts, longest
  session, and the concurrency (PARALLEL AGENTS) sweep. A conversation's id is
  its DB file name, which is also its `conversationId` in `history.jsonl`, so
  a CLI conversation seen in both counts as one session. App conversations, and
  CLI conversations with no history or log lines, are dated by their
  generations alone.
- ✅ **Some tools** — the commands you *confirmed* in approval dialogs
  (`command:<exe>`) from the logs. Headless print-mode runs log neither
  user input nor tool confirmations, so they add tokens but no tools.

## The tokens are unlabeled protobuf — and self-verified

`gen_metadata` rows are Antigravity's internal **protobuf with no field names**,
and there's no public schema. agent-walker reads the wire format directly and
pulls the few fields it needs by number (cross-checked against the `tokscale`
project, which maps them identically). Because the numbers are unofficial and
could shift on an Antigravity update, **every row is checked against the stored
output total** (`#3`): it must equal text + thinking (`#9` + `#10`). A row that
fails is skipped and counted as a parse error rather than contributing garbage
tokens.

What that check does and doesn't guarantee, stated honestly. Protobuf
identifies fields by an explicit tag number on the wire, not by position, so
adding or removing *unrelated* fields doesn't change what `#9`/`#10` decode to —
the decoder simply skips tags it doesn't read. The `#3 == #9 + #10` check is
therefore not a positional guard; it's a **mutual-consistency** check on the
three output fields:

- It catches a **re-meaning of the output fields**: if Antigravity reassigns the
  output total (`#3`) or its parts (`#9` text, `#10` thinking) to different tag
  numbers, our reads stop agreeing, the equality breaks, and the row is dropped.
- It does **not** verify the input-side fields (system `#1`, new input `#2`,
  cache read `#5`): there's no stored input total to check them against, so a
  reassignment of those tags would be read as wrong numbers and pass the output
  check undetected. That has never been observed, but it's the residual risk.

So a re-meaning of the *output* fields degrades to "no/less data"; the gap is a
silent re-meaning of an *input* field, which no row-level invariant can catch
without an input checksum the format doesn't provide. See
`src/collector/agy/conv.rs`.

## Row time

Field paths in this section and the next start at the gen blob root, where the
generation is `#1` (so the output total above is `#1.#4.#3`).

Each generation is dated by the first of these that exists (a timestamp before
1970 or from year 3000 on counts as missing):

1. its own timestamp (`#1.#9.#4`);
2. the created-at (`metadata.#1`) of the `steps` row whose `metadata.#9` copies
   this row's usage, matched on the response id (`metadata.#9.#11` there,
   `#1.#4.#11` on the gen row) and never by position, because a conversation
   can hold more planner steps than gen rows;
3. the conversation's created-at (`trajectory_metadata_blob` `#2`);
4. the DB file's mtime.

Newer agy builds dropped `#1.#9.#4`; their `#1.#9` holds only context-window
metadata (`#1.#9.#10` = context used and the window size). Those are not token
counts and are never read as such. Without step 2 every row of a conversation
would share its start time, misplacing long conversations by hour and day.

## Cost

Cost is estimated from the LiteLLM table, the same as every other agent. The
gen blob carries two model labels: an internal id (`#1.#19`, e.g.
`gemini-3-flash-a` — a version-mismatched codename) and a display name
(`#1.#21`, e.g. `Gemini 3.5 Flash (High)`). The collector uses the **display
name** — it's the product-accurate label, and after normalization (drop the
`(tier)`, hyphenate spaces) it maps straight to LiteLLM's bare id
(`gemini-3.5-flash`). The display name named the user's selection: rows served
under different internal ids (`gemini-3-flash-a`, `gemini-default`) carried the
same display name.

Newer agy builds (and the app) no longer write `#1.#21`. The internal id then
names the served model, sometimes plus a server-side routing tag
(`gemini-3.8-flash-n`, `gemini-4-argon-trial`), and the tag gets relabelled
over time: the same selection with the same model enum (`#1.#3`, also spelled
out in `#1.#20` as `model_enum`) has been served both bare and with `-n`. The
selection itself is recorded once per run in the run config (`#3.#28` on one
gen row): the model plus a mode segment — `gemini-3.8-flash-high` in the CLI,
`gemini-3.8-flash-tiered` in the app, `-agent` in older builds. The label is
the selection minus that mode segment (`gemini-3.8-flash`), taken only when the
served id is that base or the base plus one segment; the app serves under the
selected id itself, so its rows qualify too. (`-tiered` rows carry a model enum
of their own, but an enum never identified a priced model: one display name,
`Gemini 3.5 Flash (High)`, spanned four of them.) One model therefore never splits
into several labels, and it prices whenever LiteLLM lists it. A served id that
lines up with no selection is kept as served — usually versionless
(`gemini-pro-default`) — and stays unpriced rather than guess a generation. A
model LiteLLM doesn't list is labelled correctly and reported as unpriced,
never as $0.

Residual risks. Every selection seen so far ends in a mode segment; one that
didn't would lose its last model word. And a served id that differs from the
selection in a model word rather than a routing tag (served `…-flash-lite`,
selected `…-flash-high`) is labelled with the selected base, the same way the
display name labelled every row by the selection whatever the internal id
said.
