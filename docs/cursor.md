# Cursor

Cursor is **auto-detected** like the other providers — its tab appears whenever
you're signed into Cursor locally — but it's the one provider that **reaches the
network** to do it. When you're signed out (no token on disk) nothing is sent;
`--no-cursor` disables it entirely.

## Why it needs the network

Unlike Claude / Codex / OpenCode, Cursor keeps **no per-request token counts on
disk**. Its local stores hold chat text, accepted-line attribution, and the auth
token — never usage. The token and cost figures live only behind Cursor's web
dashboard, so agent-walker reads them the same way the dashboard does.

## Where the data comes from

`GET https://cursor.com/api/dashboard/export-usage-events-csv?strategy=tokens`,
authenticated with the `WorkosCursorSessionToken` cookie that the browser uses.
The cookie is `<accountId>::<jwt>`:

- the **JWT** is read from Cursor's local Electron store
  (`…/Cursor/User/globalStorage/state.vscdb`, `ItemTable` key
  `cursorAuth/accessToken`) — read-only, so SQLite never writes your live store;
- the **accountId** comes from `~/.cursor/cli-config.json` (`authInfo.authId`),
  falling back to the JWT `sub` claim.

`--cursor-token` / `CURSOR_TOKEN` supplies the JWT directly (skip the local DB),
`--cursor-state-db` points at a non-standard `state.vscdb`, and `--no-cursor`
turns the whole thing off.

This endpoint is **undocumented** and can change or break without notice. A
`401`/`403` means the session expired — re-login in Cursor to refresh the token,
and the next run picks it up.

## What's captured

Per usage event, from the CSV columns (resolved by header name, since Cursor
adds columns over time):

- **Tokens** — `Input (w/o Cache Write)` → input, `Input (w/ Cache Write)` →
  cache-write, `Cache Read` → cache-read, `Output Tokens` → output. The two input
  columns are disjoint (`Total Tokens` is their sum plus cache-read and output),
  so cache-write is that column directly, not a difference. `Total Tokens` is
  used as a checksum; a mismatch is counted as a parse warning but the row is
  still kept.
- **Model** — `Model` (e.g. `composer-2.5-fast`).
- **Cost** — the CSV's own `Cost` column, carried as the event's reported cost.
- **Timestamp** — `Date`.

## Cost

Cursor's models aren't in the LiteLLM pricing table, so unlike every other
provider the cost is **not** API-equivalent — it's the figure Cursor itself
reports in the CSV (`reported_cost_usd`), which the COST panel prefers whenever
it's present.

## Caveats

- **No project breakdown.** Cursor's usage events carry no repo/workspace
  identifier, so the PROJECTS panel is empty for Cursor — there's no key to join
  usage to a repo.
- **No tools / sessions / durations.** The usage CSV is token accounting only;
  those panels stay empty for Cursor.
- **macOS** may require Full Disk Access for agent-walker to read Cursor's
  `state.vscdb` (another app's Application Support). If it can't, pass
  `CURSOR_TOKEN` instead.
- This is the only collector that sends anything off your machine — your own
  session cookie, to Cursor, to read your own usage.
