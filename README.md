# agent-walker

[![CI](https://github.com/miiiiiiich/agent-walker/actions/workflows/ci.yml/badge.svg)](https://github.com/miiiiiiich/agent-walker/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/rustc-1.93+-blue.svg)](https://www.rust-lang.org)
[![No telemetry](https://img.shields.io/badge/telemetry-none-brightgreen.svg)](#privacy)

**A terminal dashboard for your local AI coding-agent usage** — tokens, real
API-equivalent cost, projects, tools, and autonomy metrics, aggregated from
the logs that Claude Code, Codex CLI, and Antigravity CLI already write to
your machine.

![demo — synthetic data via `--demo`](docs/demo.gif)

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

- Your logs **never leave your machine**. No telemetry, no analytics.
- The only network access is a daily fetch of public model-pricing metadata
  from [LiteLLM's pricing database](https://github.com/BerriAI/litellm),
  cached locally. `--offline` disables all network access.
- Release binaries carry [GitHub Artifact Attestations](https://docs.github.com/en/actions/concepts/security/artifact-attestations):

```sh
gh attestation verify agent-walker-aarch64-apple-darwin.tar.xz -R miiiiiiich/agent-walker
```

## Install

```sh
# Homebrew (installs the `agent-walker` and `agw` commands)
brew install miiiiiiich/tap/agent-walker

# Run without installing (npm / bun)
npx agent-walker
bunx agent-walker

# Rust
cargo install agent-walker
```

## Use

```sh
agw             # short alias
agent-walker    # same thing
```

| Key | Action |
|---|---|
| `←` `→` / `h` `l` / `Tab` | switch provider (Claude / Codex / Agy / Combined) |
| `↑` `↓` / `j` `k` / `PgUp` `PgDn` | scroll |
| `1`–`4` | jump to tab |
| `r` | reload |
| `q` / `Esc` / `Ctrl-C` | quit |

Useful flags: `--days 30` (window, default 90), `--demo` (synthetic demo
dashboard — no logs read; all screenshots here use it), `--offline`,
`--claude-dir` / `--codex-dir` / `--agy-dir` (non-standard log locations),
`--completions zsh` (shell completions).

## What it reads

| Agent | Location | Notes |
|---|---|---|
| Claude Code | `~/.claude/projects/**/*.jsonl` | tokens, models, tools, subagents, projects, turn durations |
| Codex CLI | `~/.codex/sessions/**/*.jsonl` | tokens, models, tools, task durations, projects |
| Antigravity CLI | `~/.gemini/antigravity-cli` | sessions and tool flow (no token data in its logs) |

Everything is parsed in parallel and cached per file
(`~/.cache/agent-walker/`), so warm starts take ~100 ms even on gigabytes of
logs. Log lines are treated as untrusted input — malformed records are
counted and skipped, never evaluated.

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
  with cache reads/writes priced separately; the rates date is shown in the
  COST panel.
- Antigravity log timestamps carry no timezone and are assumed to be local.
- Windows is not supported yet (log discovery relies on `$HOME`).

## License

MIT or Apache-2.0, at your option.
