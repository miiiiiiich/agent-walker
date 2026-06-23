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
the schema lives in the OSS repo (`sst/opencode`). Auto-detected: the OpenCode
tab appears only when a matching DB exists.

## What's captured

- **Token usage** per assistant message, from the `message` table's `data` JSON
  (`tokens.input` / `output` / `reasoning` / `cache.read` / `cache.write`). Mapped
  to the same schema as Claude / Codex:

  ```
  tokens = input + output + cache_read + cache_creation(=cache.write)
  ```

  OpenCode stores only the *visible* output in `output` and counts `reasoning`
  separately, so reasoning is folded into the output total (and priced at the
  output rate) to match the inclusive convention the other providers use.

- **Model** from `data.modelID` (e.g. `qwen3:8b`, `claude-...`).
- **Project** from `data.path.cwd` (the working directory), labeled like the other
  providers.
- **Durations** from `data.time.created` → `time.completed` per assistant message
  → the COMPLETION section.
- **Tools** from `part` rows of `type:"tool"` (the `tool` field: `glob` / `read` /
  `edit` / `bash` / an MCP name).
- **Sessions / activity / hourly** from message timestamps (`time.created`).

So OpenCode reaches near-parity with Claude / Codex: tokens, cost, model,
project, tools, sessions, durations, and hourly — all from the local DB.

## Cost

Same cache-aware, API-equivalent formula as the other providers: priced from the
LiteLLM database by model name. OpenCode also records its own per-message `cost`,
but agent-walker ignores it for consistency. Local models (e.g. Ollama) aren't in
the pricing database, so their cost shows as $0 even though their tokens count.

## Caveats

- The snapshot is taken at launch, so a session still being written shows up on
  the next run.
- A live DB whose WAL still needs recovery and has no `-shm` present can't be
  opened read-only; that DB is skipped rather than touched.
