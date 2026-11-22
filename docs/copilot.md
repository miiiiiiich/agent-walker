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

- **Token usage**, from three sources with a fixed precedence:
  - **Per-call rows** in `assistant_usage_events` (`input_tokens` /
    `output_tokens` / `cache_read_tokens` / `cache_write_tokens` /
    `reasoning_tokens`, `model`, `created_at`), each dated at its own call.
    They cover sessions that never exit cleanly, and calls made by
    sub-agents (`agent_id` set) are attributed as subagent work.
  - **Per-call receipts**: newer CLIs append a `session.usage_record` to the
    event stream for each model call (`data.usage`: `model`, `inputTokens` /
    `outputTokens` / `cacheReadTokens` / `cacheWriteTokens` /
    `reasoningTokens`), dated at the call and keyed by its accounting
    identity (`sourceSessionId` + `sequence`). They cover sessions with
    neither store rows nor a clean exit. A call from a sub-agent (`agentId`
    set, or `initiator` `sub-agent`) is attributed as subagent work.
  - **Shutdown snapshots**: the `session.shutdown` event the CLI appends on
    clean exit carries per-model cumulative totals for the whole session
    (`inputTokens` / `outputTokens` / `cacheReadTokens` / `cacheWriteTokens` /
    `reasoningTokens`). Each shutdown is counted as the delta since the
    previous one, dated at its own exit time, less what the receipts read
    since then already counted. Its `usageAccountingWatermarks` name, per
    source session, the highest receipt it covers; a receipt past the mark
    is left for the next exit, and so is a receipt whose model the exit
    carries no usable counters for. What remains, such as calls from before
    the CLI wrote receipts, counts at that exit.

  From a session's first recorded call on, its rows replace its snapshots;
  snapshots dated before that call still count (a session resumed across a
  CLI upgrade keeps the history from before the store existed). Its receipts
  give way to its rows from 60 s before its first live row on. Rows the CLI
  caught up from the log (`event_id` starting `usage-record:`) are copies:
  one dated from 60 s before the session's first live row on is dropped, as
  Copilot drops it from its own totals, and so is one dated from the
  session's first receipt read here on. A fork's copy of a receipt keeps
  the parent as its `sourceSessionId`, so it gives way to the parent's rows
  the same way even when the parent's log is gone, and the parent's
  caught-up rows give way to the copies over the span the fork copied. If
  any of a
  session's rows can't be read (a `created_at` that doesn't parse, a value of
  the wrong type), that session keeps its snapshots: superseding them from the
  readable rows alone would lose the unreadable row's usage. Its readable
  rows up to its latest exit are dropped (that exit's cumulative, with the
  receipts before it, already covers them), even when the exit added
  nothing. Only an exit with usable counters is a snapshot here: a receipt
  covers just its own call. After the exit the session keeps its receipts,
  since one may be the only record of the unreadable row, and its rows
  from 60 s before its first receipt there to 60 s after its last are
  dropped as their copies; its other rows still count, so calls since the
  last clean exit aren't lost. A session with neither an exit nor receipts
  keeps all its readable rows. A row that can't be tied to a session at all
  applies the same fallback to every session. An unreadable row dated after
  the latest exit and outside that span can't be counted anywhere.
  `created_at` is RFC 3339; the column default (SQLite `datetime('now')`,
  UTC without a zone suffix) is read as well.

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
