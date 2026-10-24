# OpenCode

## Where the data comes from

`~/.local/share/opencode/opencode.db` — a local SQLite store (the data dir is
overridable with `OPENCODE_HOME`, or `$XDG_DATA_HOME/opencode`). agent-walker
follows OpenCode's own database resolver:

- if `OPENCODE_DB` is set, that file is read (an absolute path as-is, a relative
  one under the data dir; `:memory:` can't be read from another process, so it's
  skipped);
- otherwise every `opencode*.db` in the data dir is read, covering both the
  default `opencode.db` and the `opencode-<channel>.db` a non-stable install
  (e.g. a `dev` build) writes.

It opens each DB **read-only** and reads it directly. The read-only flag means
SQLite can't write, checkpoint, or recover your live store; in WAL mode the read
doesn't block OpenCode's writes, and a short busy-timeout rides out the rare
exclusive moment. Reading directly (rather than copying the whole DB first) keeps
memory proportional to the recent rows actually parsed, not the full history.
Storage location is documented at
[opencode.ai/docs/troubleshooting](https://opencode.ai/docs/troubleshooting/);
the schema lives in the OSS repo (`anomalyco/opencode`, formerly
`sst/opencode`). Auto-detected: the OpenCode tab appears only when a matching
DB exists.

## Schema generations

The same `opencode.db` can hold two generations side by side, and both are
read:

- **Legacy (1.x)**: `message` rows (one JSON `data` per message) and `part`
  rows (tools, text, …), session metadata in `session`.
- **OpenCode 2**: `session_message` rows with a `type` column (`assistant`,
  `compaction`, `user`, …) and a JSON `data` column, session metadata in
  `session_v2`. 1.x already creates `session_message` but only writes
  model/agent switch rows there, never usage.

Upgrading to 2.0 migrates legacy sessions into `session_message` under the
**same message ids** and leaves the legacy tables in place. agent-walker
reads the legacy tables first and skips any `session_message` row whose id
it already counted, so migrated history counts once (and its tools, already
counted from `part`, aren't counted again). A legacy row that fails to parse
doesn't claim its id, so an intact migrated copy still counts. 2.1 renamed
the provider replay fields on text/reasoning/tool content (`state` /
`providerState` → `native`); none of the fields read here moved, and the
legacy `part.state` shape is unchanged.

## What's captured

- **Token usage** per assistant message, from the `data` JSON of `message`
  (legacy) or of `session_message` rows of type `assistant` (OpenCode 2), both
  `tokens.input` / `output` / `reasoning` / `cache.read` / `cache.write`.
  OpenCode 2 records a compaction request's usage only on its `compaction`
  row, so those rows count too. Mapped to the same schema as Claude / Codex:

  ```
  tokens = input + output + cache_read + cache_creation(=cache.write)
  ```

  OpenCode stores only the *visible* output in `output` and counts `reasoning`
  separately, so reasoning is folded into the output total (and priced at the
  output rate) to match the inclusive convention the other providers use.

- **Model** from `data.modelID` (legacy) or `data.model.id` (OpenCode 2), e.g.
  `qwen3:8b`, `claude-...`.
- **Project** from `data.path.cwd` (legacy) or the session's `directory`
  (`session_v2`, falling back to `session`), labeled like the other providers.
- **Durations** from `data.time.created` → `time.completed` per assistant message
  → the TURN LENGTH section.
- **Tools** from `part` rows of `type:"tool"` (the `tool` field: `glob` / `read` /
  `edit` / `bash` / an MCP name), or from an OpenCode 2 assistant message's
  `content` items of `type:"tool"` (the `name` field).
- **Sessions / activity / hourly** from message timestamps (`time.created`).

## Forks

An OpenCode 2 fork (`session_v2.fork_session_id` + `fork_boundary`) copies
the parent's settled rows into the fork under new ids but the **same `seq`**,
up to the highest parent `seq` before (`"before"`) or through (`"through"`)
the boundary message. agent-walker resolves that cutoff from the parent's
rows the same way and skips the fork's rows at or below it, so only the turns
the fork generated itself count under the fork. Nested forks resolve against
their own parent fork. When the parent or its boundary row is gone, every row
of the fork is kept: the copies may be the only surviving record.

So OpenCode reaches near-parity with Claude / Codex: tokens, cost, model,
project, tools, sessions, durations, and hourly — all from the local DB.

## Cost

Same cache-aware, API-equivalent formula as the other providers: priced from the
LiteLLM database by model name. OpenCode also records its own per-message `cost`,
but agent-walker ignores it for consistency. Local models (e.g. Ollama) aren't in
the pricing database, so their tokens count but their cost stays unpriced
(shown as —, never $0).

## Caveats

- The snapshot is taken at launch, so a session still being written shows up on
  the next run.
- A live DB whose WAL still needs recovery and has no `-shm` present can't be
  opened read-only; that DB is skipped rather than touched.
- Session title generation adds its usage to the `session_v2` totals only,
  with no message row, so it isn't counted (a small request once per session).
  The `session_v2` / `session` aggregates themselves are never read: they are
  cumulative and can't be dated per turn.
