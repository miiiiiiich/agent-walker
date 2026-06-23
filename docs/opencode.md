# OpenCode

## Where the data comes from

`~/.local/share/opencode/opencode.db` — a local SQLite store (overridable with
`OPENCODE_HOME`, or `$XDG_DATA_HOME/opencode`). Read-only and **immutable**, so
agent-walker never locks the DB, never recovers its WAL, and never writes to your
OpenCode store. Storage location is documented at
[opencode.ai/docs/troubleshooting](https://opencode.ai/docs/troubleshooting/);
the schema lives in the OSS repo (`sst/opencode`). Auto-detected: the OpenCode
tab appears only when `opencode.db` exists.

## What's captured

- **Token usage** per assistant message, from the `message` table's `data` JSON
  (`tokens.input` / `output` / `reasoning` / `cache.read` / `cache.write`). Mapped
  to the same schema as Claude / Codex:

  ```
  tokens = input + output + cache_read + cache_creation(=cache.write)
  ```

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

- Only `opencode.db` is read; per-channel stores (`opencode-<channel>.db`) are
  not, for now.
- Immutable reads see the last-committed state, so a session still being written
  may lag by one checkpoint.
