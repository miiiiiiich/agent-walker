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

- ✅ **Tokens** — per generation: input (the prompt tokens not read from
  cache), cache-read, cache-write, output (response text), and
  thinking/reasoning. Thinking is folded into output so it counts toward the
  total (same convention as OpenCode).
- ✅ **Model** — always in the pricing id form, e.g. `gemini-4-neon` or
  `gemini-3.8-flash` (see [Cost](#cost)).
- ✅ **Project** — the workspace path recorded in the conversation.
- ✅ **Sessions / activity** — session timestamps come from every dated
  generation, every `history.jsonl` line that names a conversation, and each
  logged user input or tool confirmation → sessions, active days, streak, daily
  session counts, longest session, and the concurrency (PARALLEL AGENTS) sweep.
  A conversation's id is its DB file name, which is also its `conversationId`
  in `history.jsonl`, so a CLI conversation seen in both counts as one session.
  App conversations, and CLI conversations with no history or log lines, are
  dated by their generations alone.
  - A `history.jsonl` line without a `conversationId` (slash commands log
    some) opens no session.
  - A user input is logged just before the line naming its conversation, so it
    joins the next conversation a log line names: `Sending user message to
    conversation <id>`, `Created conversation <id>`, `Print mode:
    conversation=<id>`, or a tool confirmation's `convID=`. An input no later
    line names a conversation for stays with the last one named, or with the
    log file when the log names none. These markers count only at the start of
    a log message (after the `file.go:line] ` header), so prompt or tool text
    quoted inside another message can't pass for one.
- ✅ **Subagents** — a subagent runs as a conversation (and DB) of its own,
  whose trajectory metadata (`trajectory_metadata_blob`) names its parent
  conversation (`#5`). Its tokens and tools count as **Subagent** work under the
  subagent's name (`#8.#1`, e.g. `research`) and in the top-level
  conversation's session — the root (`#6`), else the parent — the way Claude's
  subagents count in the parent session. Top-level conversations carry `#6`
  too, set to their own id, so only `#5` marks a subagent. Tokens are not
  shared between the two DBs, so nothing is counted twice.
- ✅ **Some tools** — from the logs, the tools you approved and the ones auto
  mode approved for you. Headless print-mode runs log neither user input nor
  tool confirmations, so they add tokens but no tools.
  - An approved tool confirmation counts as `command:<name>` when its grant
    (`persistGrants`, or `turnGrants` in older builds) is a command, else as
    `ToolConfirmation`. Only the executable's file name is kept, never its
    directory or arguments, and a non-command grant's argument (the path in
    `read_file(…)`) is never read. A declined confirmation (`approved=false`)
    counts no tool, and neither does a grant in `denyGrants`.
  - A file write auto mode approves without asking (`auto mode: auto-approving
    file write "<Tool>"`) counts under that tool's name, e.g. `Edit`.

## The tokens are unlabeled protobuf — and self-verified

`gen_metadata` rows are Antigravity's internal **protobuf with no field names**,
and there's no public schema. agent-walker reads the wire format directly and
pulls the few fields it needs by number. What each number means comes from the
protobuf descriptor compiled into the agy binary, read as data. In the usage
message (`#1.#4`):

| Field | Meaning | Read as |
|---|---|---|
| `#1` | model enum | not read (it is not a token count) |
| `#2` | input tokens | input |
| `#3` | output total | the self-check below |
| `#4` | cache-write tokens | cache-write |
| `#5` | cache-read tokens | cache-read |
| `#9` | thinking output | reasoning, folded into output |
| `#10` | response output | output |
| `#11` | response id | the dedupe key |

Because the numbers are unofficial and could shift on an Antigravity update,
**every row is checked against the stored output total** (`#3`): it must equal
thinking + response (`#9` + `#10`). A row that fails is skipped and counted as
a parse error rather than contributing garbage tokens.

What that check does and doesn't guarantee, stated honestly. Protobuf
identifies fields by an explicit tag number on the wire, not by position, so
adding or removing *unrelated* fields doesn't change what `#9`/`#10` decode to —
the decoder simply skips tags it doesn't read. The `#3 == #9 + #10` check is
therefore not a positional guard; it's a **mutual-consistency** check on the
three output fields:

- It catches a **re-meaning of the output fields**: if Antigravity reassigns the
  output total (`#3`) or its parts (`#9` thinking, `#10` response) to different
  tag numbers, our reads stop agreeing, the equality breaks, and the row is
  dropped.
- It can't tell `#9` from `#10`: the sum is the same either way, so a swap of
  the two parts passes. Output and cost stay right; only the reasoning share
  would be wrong.
- It does **not** verify the input-side fields (input `#2`, cache write `#4`,
  cache read `#5`): there's no stored input total to check them against, so a
  misread input field passes the output check undetected. Reading the model enum
  `#1` as system-prompt tokens is such a misread; only the descriptor rules it
  out.

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

Cost is estimated from the LiteLLM table, the same as every other agent.
Every generation, however old, is priced at the rates in the table agent-walker
loaded for the run, not at the rates in force when it ran. A rate change — a
Gemini price increase, say — therefore applies to all history, and the same
window reports a different cost once the table updates.

The gen blob carries two model labels: an internal id (`#1.#19`, e.g.
`gemini-3-flash-a` — a version-mismatched codename) and a display name
(`#1.#21`, e.g. `Gemini 4 Neon (High)`). Every row is labelled in the **pricing
id form** (`gemini-4-neon`) whichever of the two it carries, so one model shows
under one label and that label is the key it prices under.

The collector prefers the **display name**: it names the user's selection —
rows served under different internal ids (`gemini-3-flash-a`, `gemini-default`)
carried the same display name. It is normalized the way the pricing lookup
normalizes names: lowercase, drop a `(tier)` suffix, hyphenate spaces
(`Gemini 4 Neon (High)` → `gemini-4-neon`).

Some agy builds don't write `#1.#21`, and rows on a `-tiered` selection (the
app, and subagents) still lack it in builds that do. The internal id then
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
