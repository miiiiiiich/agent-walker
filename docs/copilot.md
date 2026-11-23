# GitHub Copilot CLI

## Where the data comes from

Two local sources under `~/.copilot/` (overridable with `COPILOT_HOME`),
both written by the agentic GitHub Copilot CLI (`@github/copilot`; not the
retired `gh copilot` extension):

- `session-state/<uuid>/events.jsonl` — one directory per session, the
  session's event stream. Newer CLIs rotate a long stream: the old file is
  archived as `events.<n>.jsonl`, and the new head's first record
  (`session.journal_link`) names it. agent-walker follows those links back
  from `events.jsonl`, only to a bare `events.<n>.jsonl` with a lower number
  in the same directory, and reads the chain oldest first as one stream, so
  each exit still counts once. The replay checkpoint a new head opens with
  (records marked `checkpoint`) copies what the archive already holds and is
  skipped, apart from the session's start. A missing predecessor ends the
  chain there; an unreadable one also counts as a parse error. A head that
  links back is parsed again on every run rather than taken from the parse
  cache, which checks only the head's size and modification time, so a
  predecessor that appears or changes later is still read.
- `session-store.db` — the CLI's SQLite session store. Its
  `assistant_usage_events` table holds one row per model call (CLI 1.0.69 and
  later). The DB is opened **read-only**, like the OpenCode store.

Auto-detected: the Copilot tab appears only when either source exists.
Everything is read locally; nothing leaves the machine.

**Input bounds.** Only regular files are opened; a symlink to one is followed,
while a FIFO or device counts as an unreadable file. A line longer than 64 MiB,
or one holding more than 262,144 JSON array elements and object entries, is
skipped as a parse error. A timestamp before 1970 or from year 3000 on is
treated as missing. An id or name longer than 1,024 bytes, or a path longer
than 4,096, is treated as missing.

The session store is opened read-only: SQLite never writes, checkpoints or
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

- **Token usage**, from three sources:
  - **Per-call rows** in `assistant_usage_events` (`input_tokens` /
    `output_tokens` / `cache_read_tokens` / `cache_write_tokens` /
    `reasoning_tokens`, `model`, `created_at`), each dated at its own call.
    They cover sessions that never exit cleanly, and calls made by
    sub-agents (`agent_id` set) are attributed as subagent work.
  - **Per-call receipts**: newer CLIs append a `session.usage_record` to the
    event stream for each model call (`data.usage`: `model`, `inputTokens` /
    `outputTokens` / `cacheReadTokens` / `cacheWriteTokens` /
    `reasoningTokens`), dated at the call and keyed by its accounting
    identity (`sourceSessionId` + `sequence`), so a fork's copy counts once.
    A call from a sub-agent (`agentId` set, or `initiator` `sub-agent`) is
    attributed as subagent work.
  - **Shutdown snapshots**: the `session.shutdown` event the CLI appends on
    clean exit carries per-model cumulative totals for the whole session
    (`inputTokens` / `outputTokens` / `cacheReadTokens` / `cacheWriteTokens` /
    `reasoningTokens`). Each shutdown is counted as the delta since the
    previous one, dated at its own exit time, less the receipts it covers
    that count (below). Its `usageAccountingWatermarks` name, per source
    session, the highest receipt it covers; a receipt past the mark is left
    for the next exit, and so is a receipt whose model the exit carries no
    usable counters for.

  Each session that made calls takes them from one source, never a mix of
  the two. A session with a readable live row is on the store: its rows and
  exits count and its receipts don't. Otherwise its receipts count, and its
  exits only for what the receipts didn't carry; the rows the CLI caught
  up from its log (`event_id` starting `usage-record:`) copy those receipts
  and don't count, unless its journal holds no receipts (it is gone), and
  then they are the only record and count as its store rows. A receipt
  goes by its `sourceSessionId` and by every journal holding it: it
  doesn't count if any of them is on the store, so neither a copy without
  an accounting identity nor a receipt naming another session counts
  beside a store row. A copied receipt always comes off the fork's exit,
  since the parent counts that call.

  A fork's journal opens with a copy of its parent's history, event ids
  and all, then a `session.info` marker reading "Forked from <parent> …
  as <fork>." The usage read before the marker is the parent's, decided
  by the parent's store rows like the parent's own; tools, turns and
  activity stay with the fork. Copied exits whose parent can't be named
  don't count: under a marker that doesn't name it in a form read here,
  or, in a journal without a marker, dated before its own start (Copilot
  dates a fork's start at the fork; any record dated before the start
  marks such a journal as a fork). Copied receipts still go by their
  `sourceSessionId`. A fork can't have made a call before it existed, so
  its store rows dated up to its marker (without a marker naming it, up to
  its start) aren't read: whether the CLI caught them up from the copied
  receipts or the store copied them, they repeat the parent's history. Its
  journal's own usage dated up to then doesn't count either, except a
  receipt naming another session as its source. These rules were read
  from Copilot's fork path for custom session filesystems; the default
  on-disk fork runs natively and isn't verified, and if it writes no
  marker, this before-it-existed cut is what keeps each call counted
  once.
  With the fork's journal gone, the parent's own marker ("Forked this
  session into <fork>.") dates the fork instead; the parent's history stays
  its own. When a journal has lost its oldest segment, its first exit
  read counts everything before it, so another journal's records of that
  session (a fork's copies) dated up to that exit don't count.

  From a session's first recorded call on, or from 60 s before its first
  live row if that's earlier, its rows replace its snapshots: an exit a
  moment before the first live row may cover that row's call. Snapshots
  dated before still count (a session resumed across a CLI upgrade keeps
  the history from before the store existed); one in that last minute
  before the store doesn't, even if its calls had no row. A
  caught-up row dated from 60 s before the session's first live row on is
  dropped, as Copilot drops it from its own totals. If any of a session's
  rows can't be read (a `created_at` that doesn't parse, a value of the
  wrong type), that session keeps all its snapshots: superseding them from
  the readable rows alone would lose the unreadable row's usage. A model's
  readable rows up to the latest exit whose total or watermark for that
  model moved are dropped (that cumulative already covers them), and so are
  live rows stamped within 60 s after it, which may be a call it covers. An
  exit repeating both covers nothing new, so a call past its watermark
  keeps its row; an exit without a model's counters covers none of its
  rows. A session with no exit keeps all its readable rows. A row that
  can't be tied to a session at all applies the same fallback to every
  session. An unreadable row dated after the latest exit can't be counted
  anywhere.
  `created_at` is RFC 3339; the column default (SQLite `datetime('now')`,
  UTC without a zone suffix) is read as well.

  The rule is that when two records can't be told apart as one call or
  two, one is counted. It isn't guaranteed where a fork can't be told from
  its own journal: one with no marker and nothing dated before its start
  (Copilot writes both), whose copied exit counts beside the parent's
  store rows once the parent's journal is gone; and a fork's first exit
  carrying the parent's total from before the fork with no copied exit to
  measure from (CLIs that write receipts take the copied ones off it). The
  rule costs these undercounts. In a session with a live row, a call only in a receipt (its
  row unreadable or missing), and an exit in the last minute before the
  first live row whose calls had no row. In a session on its receipts, a
  call only in a caught-up row whose receipt is gone and that no later
  exit covers. Any receipt held by a journal on the store, such as one an
  older fork copied without a marker. A copied exit whose parent can't be
  named, when the parent's own journal is gone. A parent's calls before
  its first store row that its receipts carried, when the parent's journal
  is gone and only a fork's copy of the exit remains. A fork's copy of a
  receipt written after the snapshot of a parent's first exit read (past
  its watermark, but dated before it), when the parent's journal has lost
  its oldest segment.

  All three map to the same schema as the other providers:

  ```
  tokens = (input − cache_read − cache_write) + output + cache_read + cache_write
  ```

  `inputTokens` includes both the cached reads and the cache writes: the
  CLI's own billing breakdown (`tokenDetails.input`) equals
  `input − cache_read − cache_write`, per call and per session. So fresh
  input is the difference. `reasoningTokens` is a subset of `outputTokens`
  (the CLI's schema: "output tokens used for reasoning"; billing has no
  separate reasoning bucket) and is tracked without being double-added.

- **Model** per row, or per entry in `modelMetrics` (e.g. `gpt-5-mini`,
  `claude-sonnet-4.6`).
- **AI credits** (the CREDITS panel, Copilot tab only): daily spend history
  from the cumulative `totalNanoAiu` ledger (1 credit = 1e9 nano-AIU),
  carried on periodic `session.usage_checkpoint` events as well as shutdowns
  — so credits keep tracking even for sessions that never exit cleanly.
  Historical by design: a ledger of spend that already happened, never a
  remaining-quota meter.
- **Turn durations** from explicit `assistant.turn_start` / `turn_end`
  pairs (no heuristics — Copilot logs real turn boundaries).
- **Project** from the session's working directory (`session.start`, or the
  store's `sessions.cwd`).
- **Tools** from `tool.execution_start` events.
- **Activity** from every timestamped event in the session stream, and from
  the per-call rows.

## Deduplication

- **Forks** (`/fork`) copy the parent's events, ids and timestamps intact,
  into the new session's stream. Shutdowns, checkpoints, tool calls and turns
  are therefore keyed by the event `id` (a UUID), and receipts by their
  accounting identity, so a copy collapses into the parent's original. A copied shutdown that the parent's per-call rows
  already replace is dropped from the fork too.
- **Resumed sessions**: `--resume` appends to the same session, and each
  later clean exit appends another shutdown with cumulative totals. Every
  exit contributes only what happened since the previous one — cumulatives
  are never summed, and a counter going backwards (CLI update) starts a new
  epoch counted in full.
- OpenTelemetry file exports (`COPILOT_OTEL_FILE_EXPORTER_PATH`) are not
  read, so their re-exported spans can't double count here.

## Caveats

- **Without the session store or receipts, sessions that never exit cleanly
  have no token data**: an older CLI's event stream carries token totals
  only at shutdown. A crashed or still-open session then contributes
  activity and credits, but no tokens, until it eventually exits.
- **Day-level granularity of snapshot-only sessions is only as fine as your
  exit habits**: everything between two clean exits lands on the later
  exit's day. Per-call rows have no such limit.
- The per-model token snapshots newer CLIs put on `session.usage_checkpoint`
  (`providerModelMetrics`, `accountingSnapshot`) are not read: they cover the
  same calls as the receipts before them. Receipts add tokens only; credits
  still come from the `totalNanoAiu` ledger.
- A compaction's own usage (`session.compaction_complete`'s
  `compactionTokensUsed`) is not read. Whether a compaction call also writes
  a receipt is unverified (the receipt's `interactionType` lists
  `conversation-compaction`), so reading both could count it twice. Usage
  no receipt carries still counts in the next exit's remainder.
- A caught-up row is not matched to its receipt by id: the `event_id` after
  `usage-record:` has no documented format. The two are kept apart by date,
  as above.
- Premium-request counts are not surfaced; tokens, API-equivalent cost, and
  the credit ledger are.
