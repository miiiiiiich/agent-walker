# agent-walker

[![npm](https://img.shields.io/npm/v/agent-walker)](https://www.npmjs.com/package/agent-walker)
[![downloads](https://img.shields.io/npm/dt/agent-walker)](https://tanstack.com/stats/npm?packageGroups=%5B%7B%22packages%22%3A%5B%7B%22name%22%3A%22agent-walker%22%7D%5D%7D%5D&range=30-days)
[![CI](https://github.com/miiiiiiich/agent-walker/actions/workflows/ci.yml/badge.svg)](https://github.com/miiiiiiich/agent-walker/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**日本語: [README.ja.md](README.ja.md)**

![agent-walker dashboard](docs/demo.gif)

### Want to know how you use AI, not just how many tokens you burn?

agent-walker reads the logs Claude Code, Codex CLI and friends already leave on your machine, works out every metric it can, and puts them on one screen.
Compare with your friends, share it, have fun! #agent-walker

```sh
bunx agent-walker
# npx agent-walker
```

## What you learn

More than each agent's `status` or `usage` command tells you.

Every section reads the same window: the last 30 days.

| Question | Section |
|---|---|
| Which days you use it most, and at what hours | ACTIVITY / BY HOUR |
| How the daily burn splits across models | TOKENS PER DAY |
| Which projects, models and settings eat the tokens | PROJECTS / MODELS / MODES |
| Which tools and subagents do the work | TOOLS / SUBAGENTS |
| Which skills the tokens went to (Claude) | SKILLS |
| How many agents you run at once | PARALLEL AGENTS |
| How long you actually let an agent work | TURN LENGTH |
| How many hours the agent worked, and how fast you answer it | WORKING TIME |
| Whether you're wasting context | CONTEXT |
| What it would have cost on the API | COST |
| How close you got to the plan limit (Codex) | LIMITS |
| How many AI credits you spent (Copilot) | CREDITS |
| Your favourite model, best day, peak hour, longest session, streak | SIGNAL |

## Share

Press `s` and a shareable image lands on your clipboard. Nothing private on it, no project names.

Your rank and animal change with token volume. The colour changes with the hours you work.

![agent-walker codename card](docs/card.png)

## Supported agents

Claude Code / Codex CLI / OpenCode / GitHub Copilot CLI / Grok Build / Antigravity, all auto-detected. Cursor with `--cursor`.

Each one has a page on what is read and how: [Claude Code](docs/claude.md) / [Codex](docs/codex.md) / [OpenCode](docs/opencode.md) / [Cursor](docs/cursor.md) / [Copilot](docs/copilot.md) / [Grok](docs/grok.md) / [Antigravity](docs/agy.md)

## Privacy

agent-walker sends your logs nowhere. The only network use is fetching the price table (kept for the day once it lands), and Cursor's usage when you pass `--cursor`.

## Keys

| Key | Action |
|---|---|
| digits | switch tab |
| `r` | reload |
| `s` | copy share image |
| `q` / `Esc` / `Ctrl-C` | quit |

<details>
<summary>Flags</summary>

| Flag | What it does |
|---|---|
| `--share <path>` | write the share image to a PNG and exit |
| `--json` | export the summary and dated events as JSON (experimental) |
| `--days <N>` (with `--json` only) | window in days (default 30); TUI stays at 30 |
| `--cursor` | read Cursor's usage from its dashboard (sends your Cursor session cookie to cursor.com; off by default) |
| `--no-cache` | reread every log |

</details>

## JSON

`--json` writes the same numbers as the TUI, plus the dated events behind them, as one JSON document. Experimental: the schema may change between minor releases.

```sh
agent-walker --json | jq '{schema_version, window, total}'
```

## Note

Costs are API-equivalent estimates, priced with [LiteLLM](https://github.com/BerriAI/litellm)'s table.

## Acknowledgements

[ccusage](https://github.com/ccusage/ccusage) pioneered local usage tracking for AI coding agents and first identified and solved problems that agent-walker also addresses. See [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES).

## License

MIT or Apache-2.0, at your option.
