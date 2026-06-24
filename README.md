# agent-walker

[![CI](https://github.com/miiiiiiich/agent-walker/actions/workflows/ci.yml/badge.svg)](https://github.com/miiiiiiich/agent-walker/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/rustc-1.93+-blue.svg)](https://www.rust-lang.org)

**日本語: [README.ja.md](README.ja.md)**

**Can you see where your AI agents have walked?**

A local terminal dashboard built from the logs your AI coding agents already
write to your machine — tokens, API-equivalent cost, projects, tools, and
autonomy, a month of runs in one screen. Put in the work and you'll earn a
codename.

*Only the one watching can deter it.*

![agent-walker terminal dashboard preview](docs/demo.gif)

## Why

Subscription AI tools don't show you what you actually use. agent-walker
reads the session logs on your disk and answers, in one screen:

- **How much would this cost on the API?** — cache-aware cost windows
  (today / 7d / 30d / period), so you know if the subscription pays for itself
- **Where do the tokens go?** — per-repository breakdown across agents
- **When and with which model?** — GitHub-style activity grass, stacked
  per-model daily bars, hour-of-day profile in your local timezone
- **Can it run unattended?** — turn-duration histogram weighted toward the
  20-minute-plus autonomy range

## Privacy

- Your logs **never leave your machine** — agent-walker does not phone home
  with usage or error data.
- The only always-on outbound traffic is a `GET` of public model-pricing
  metadata from [LiteLLM's pricing database](https://github.com/BerriAI/litellm)
  (MIT-licensed) when the dashboard loads or reloads a report. It carries no
  log content and no usage data, but it does let GitHub's CDN see your IP,
  the same as any `git pull`. Block it with a firewall rule or run offline if
  that matters to you.
- **Cursor is the one exception, and it is opt-in.** Only when you pass
  `--cursor` does agent-walker read your local Cursor session token and query
  Cursor's own usage dashboard (Cursor keeps no usage on disk). That sends your
  session cookie to Cursor to read your own usage; nothing Cursor-related runs
  without the flag. See [docs/cursor.md](docs/cursor.md).
- Release binaries carry [GitHub Artifact Attestations](https://docs.github.com/en/actions/concepts/security/artifact-attestations):

```sh
gh attestation verify agent-walker-aarch64-apple-darwin.tar.xz -R miiiiiiich/agent-walker
```

## Use

No install — `npx` / `bunx` fetch the prebuilt binary for your platform
(macOS arm64/x64, Linux arm64/x64 GNU, Windows x64) and run it:

```sh
npx agent-walker
# or
bunx agent-walker
```

| Key | Action |
|---|---|
| `←` `→` / `h` `l` / `Tab` | switch provider (only those with data; most-used first; Total last) |
| `↑` `↓` / `j` `k` / `PgUp` `PgDn` | scroll |
| `1`–`9` | jump to tab |
| `r` | reload |
| `s` | share your stats card (copy image / save to your OS Downloads folder) |
| `q` / `Esc` / `Ctrl-C` | quit |

Flags:

| Flag | What it does |
|---|---|
| `--days <N>` | Analysis window for the graphs (default 30; the codename's throughput level is always taken from the last 30 days, so the rank doesn't drift with `--days`) |
| `--share <path>` | Render the stats card to a PNG, print its caption, and exit |
| `--no-cache` | Rescan every log file, ignoring the parse cache |
| `--claude-dir` / `--codex-dir` / `--agy-dir` / `--opencode-dir` | Point at non-standard log locations (also honors `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and `OPENCODE_HOME` when set) |
| `--cursor` | Opt in to Cursor — reads your local Cursor session token and queries Cursor's usage dashboard over the network (off by default; see [Privacy](#privacy)). `--cursor-token` / `CURSOR_TOKEN` supplies the token directly |
| `--completions <shell>` | Print shell completions (e.g. `--completions zsh`) and exit |

## Share your codename

Press `s` in the dashboard (or run `agent-walker --share card.png`) to render a
shareable stats card. Instead of leading with a raw token count, the card reads
*how* you work: a GitHub-style activity grid, your hour-of-day rhythm, and a
per-model split, with parallelism and task-time as plain numbers.

Your usage also earns a **codename** — a rank you climb as you go. Exactly how
it's earned is left as a puzzle, and repository names never appear on the
card — just glance before you post.

![an agent-walker codename stats card](docs/card.png)

## What it reads

| Agent | Location | Notes |
|---|---|---|
| Claude Code | `~/.claude/projects/**/*.jsonl` | tokens, models, tools, subagents, projects, turn durations — [details](docs/claude.md) |
| Codex CLI | `~/.codex/sessions/**/*.jsonl` | tokens, models, tools, task durations, projects — [details](docs/codex.md) |
| OpenCode | `~/.local/share/opencode/opencode*.db` | auto-detected (SQLite, read-only snapshot; honors `OPENCODE_DB`); tokens, models, tools, durations, projects — [details](docs/opencode.md) |
| Cursor | Cursor usage dashboard (network) | **opt-in** (`--cursor`); tokens, models, and Cursor-reported cost — no project/tools (Cursor exposes none). Reads the session token from `state.vscdb` — [details](docs/cursor.md) |
| Antigravity CLI | `~/.gemini/antigravity-cli` | auto-detected (tab shows only if logs exist); sessions and tool flow only — no token data in its logs — [details](docs/agy.md) |

Each agent counts and caches tokens differently. The per-agent pages above
explain what's read, how tokens/cache/cost are counted, and the caveats.

Everything is parsed in parallel and cached per file
(`~/.cache/agent-walker/`), so warm starts skip the bulk of the work and only
reparse log files whose `(mtime, size)` changed. Log lines are treated as
untrusted input — malformed records are counted and skipped, never evaluated.

### Keep more than 30 days of history

Claude Code **deletes transcripts after 30 days by default**, so on first
use the dashboard will only show the last month no matter how long the
window is. To keep a year of history, add this to `~/.claude/settings.json`:

```json
{ "cleanupPeriodDays": 365 }
```

Cleanup runs on session start, so anything older than 30 days that was
already deleted is gone — set this early. Codex CLI keeps its sessions
indefinitely; no setting needed.

### Notes & limitations

- Cost figures are **API-equivalent estimates** (what the same usage would
  cost on metered pricing), not your actual bill. Rates come from LiteLLM
  with cache reads/writes priced separately; the pricing-fetch date shows up
  in the COST panel when pricing loads and the terminal has room for it.
- Token totals are **cache-inclusive** (input + output + cache writes + cache
  reads). Claude Code re-sends the full context every turn, so for heavy
  users the bulk of the total tends to be cache reads — real but cheaply
  billed context re-reads, not new work. See [docs/claude.md](docs/claude.md).
- **Antigravity tokens/cost are not counted** — its usage lives in an
  undocumented protobuf format. Its tab is auto-detected, but the Total token
  count still excludes it (activity only); see [docs/agy.md](docs/agy.md).
- These numbers **won't match each agent's own in-app usage display**: those use
  different time windows and count cache reads differently. See the per-agent
  pages above.
- Antigravity log timestamps carry no timezone and are assumed to be local.
- Windows binaries are published from 0.3 onward; the dashboard runs in
  Windows Terminal / PowerShell and reads logs from `%USERPROFILE%`. WSL
  installs work too — paths just stay in their Linux form.

## License

MIT or Apache-2.0, at your option.
