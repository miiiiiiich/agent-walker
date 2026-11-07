# agent-walker for Claude Code

A Claude Code mod that shows agent-walker in a pane.

```
/plugin install agent-walker --marketplace miiiiiiich/agent-walker
```

| Key | Action |
|---|---|
| `/agent-walker` | open the pane on Summary (the last 30 days) |
| `Tab` / `Shift+Tab` / `↑` `↓` / digits | switch between Summary and Session (this session) |
| `Esc` | close |
| `↑` `↓` | scroll when the content is taller than the pane |

`←` `→` cannot switch tabs: a pane does not receive them.

The pane is open only while it holds the keys. Typing, a click outside it, a question dialog or a sent prompt closes it, and what was typed stays in the prompt. Left open behind a conversation, Esc would go to the prompt, the turn or a dialog first and could not close it.

## The CLI and the mod

| | agent-walker | this mod |
|---|---|---|
| Does | read logs, aggregate, rank, the art and colors, the card | when to call, formatting (`9.1B`, `167h 26m`), labels, layout, the pane, keys |
| Does not | format text or lay it out | aggregate, rank, hold the art |

The mod calls the CLI for the tab on screen, and draws it for each surface.

| Tab | Calls | Terminal | Desktop, VS Code, mobile |
|---|---|---|---|
| Summary | `agent-walker summary --json` | codename and numbers, side by side when wide | the share card's SVG; the numbers if the card cannot be used |
| Session | `agent-walker session <id> --json` | numbers and one bar per turn, side by side when wide | the numbers, and the bars as an SVG |

Each open reads the logs afresh; an open pane does not update. A new width lays out again without calling the CLI. Output the mod cannot read is not drawn: the pane asks for an update instead.

## What runs

- `npx -y agent-walker@<version>`, where the version is the one in this mod's `.claude-plugin/plugin.json`. A release sets it to the crate's version, so the mod and the JSON it reads always come from the same release.
- It runs in the home directory, not the session's, so a project's `.npmrc` or `node_modules/.bin` is not picked up.
- `AGENT_WALKER_BIN`, when set, runs that binary instead of npx (for development).
- agent-walker's only network use is fetching the price table. Logs are never sent anywhere.

## Development

```sh
bun install
bun run check   # biome and tsc
bun run test    # claude plugin validate and test
```

`tsc` needs the API types Claude Code writes into `.claude-plugin/types/` once it has loaded the mod (`claude --plugin-dir claude-mod`); CI runs the rest without them.
