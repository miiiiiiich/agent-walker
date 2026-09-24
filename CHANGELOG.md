# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
## [Unreleased]

## [0.19.1] - 2026-09-24

### Changed

- The npm package now carries provenance, linking each version to the commit
  and CI run that built it.

## [0.19.0] - 2026-09-23

### Changed

- Tokens, working time and cost lead the share card as large numbers with
  per-day rates. The strip below gains the turn count and tokens/min next to
  sessions and the cache share, and the Models panel shows how many models you used.
- The card's footer shows `bunx agent-walker` and the period it covers instead
  of the repository URL.

## [0.18.1] - 2026-09-19

### Changed

- Faster start-up.

### Fixed

- Cursor is auto-detected again, as it was before 0.18.0. `--cursor` is gone.

## [0.18.0] - 2026-09-17

### Changed

- Cursor is opt-in: pass `--cursor` to read its usage.
- The TUI no longer redraws while idle.
- README trimmed to match.

### Removed

- Most command-line flags. `--help` is down to five; logs are read from
  wherever each tool keeps them.

## [0.17.0] - 2026-09-16

### Added

- Experimental `--json` exports summaries and dated events for charts and
  LLMs, with `--days <N>` to widen the window. The schema may still change
  between minor releases.

### Changed

- The parse cache no longer keeps a copy per version; old copies are removed
  on startup.
- The price table is kept on disk and reused for the day, so costs still
  price offline.
- Claude turns carry their session id, so exported turns join to sessions on
  every provider. The parse cache rebuilds once.

### Removed

- `--days` for the TUI, share and render modes: every section reads the same
  30 days.
- The hidden `--snapshot` text output, replaced by `--json`.

## [0.16.0] - 2026-09-14

### Added

- WORKING TIME: how many hours the agent actually worked, not how long the
  session was open. It also shows context read per working minute, your peak
  day, and how fast you answer.
- WORKING TIME says what those hours are made of, model versus tools. A long
  batch wait is time the agent spent asleep, and now reads that way.

### Changed

- COMPLETION is renamed TURN LENGTH, and no longer counts the time you spent
  answering a question mid-turn.
- Sections reorder below the charts, pairing what you compare.
- Claude turns copied into a fork child no longer count twice, and are dated
  by when they ended.

### Fixed

- CONTEXT bars no longer stretch wider than other sections on a wide terminal.

## [0.15.0] - 2026-08-30

### Added

- CONTEXT: where your input-equivalent cost actually goes. Cache re-reads by
  context size, resumes after the cache expired, cold starts and ordinary new
  input all sit on one scale, so "keep going" and "start fresh" compare.
- The share card shows the cached share next to the cost, and the caption fits
  what X allows.

### Changed

- The right column reads CONTEXT, MODES, then COST, SIGNAL. How you drive the
  agent sits above the bookkeeping.

## [0.14.0] - 2026-08-19

### Added

- MODES: how much rope you give the agent, an autonomy mix by permission mode,
  next to the effort mix.
- COMPLETION: how many turns you cut short. Interrupted turns no longer skew
  the duration percentiles.

### Changed

- Share-card text may shift by a sub-pixel, from a new text-rendering stack.

### Fixed

- Cost shows a dash instead of `$0` when pricing is unknown, so an unpriced
  model never reads as free.
- Release tweets no longer cut sentences mid-word.

## [0.13.2] - 2026-08-14

### Added

- MODES on the Claude tab: the reasoning-effort mix, including the turns you
  hand to subagents.

### Changed

- Release notes are written from this changelog instead of installer
  boilerplate, and every release is announced on X automatically.

### Removed

- Intel macOS prebuilt binaries. Apple Silicon remains supported.

## [0.13.1] - 2026-07-30

### Added

- `THIRD_PARTY_NOTICES` carries ccusage's copyright and MIT license, in
  recognition of its prior work on problems agent-walker also addresses (#50).
  It ships in every release archive, the npm package and the cargo source
  package.

## [0.13.0] - 2026-07-29

### Changed

- Every column chart now renders one character per column, and stat bars share
  one shape across sections. Purely visual; no numbers change.

## [0.12.0] - 2026-07-28

### Added

- Grok Build (xAI) support: a new auto-detected tab with per-model token
  splits. Fork copies are deduplicated, and subagent sessions are left to the
  coordinator that already counts them.

## [0.11.0] - 2026-07-28

### Added

- GitHub Copilot CLI support: a new auto-detected tab, with a CREDITS panel
  for daily AI-credit spend. Tokens come from the totals the CLI writes on
  exit, so a session that never exits cleanly shows activity but no tokens
  until it closes.

### Fixed

- Claude advisor calls were billed under their own model but missing from the
  top-level counters, and were silently dropped. They now surface as their own
  usage events.

## [0.10.1] - 2026-07-21

### Fixed

- Codex fork and spawn children replay their parent's history with rewritten
  timestamps, and the replay was counted again — inflating token totals,
  LIMITS samples and the effort mix (#36). Events are now deduplicated by
  content, so a replay contributes nothing.

## [0.10.0] - 2026-07-13

### Changed

- Retuned the top of the codename ladder so it is realistically reachable.
  Lower ranks are unchanged; nobody moves down.

## [0.9.0] - 2026-07-08

### Added

- SKILLS on the Claude tab: token volume per skill. The subtitle carries the
  honest denominator, since most tokens flow outside skills.
- LIMITS on the Codex tab: the daily peak of your plan's rolling window.
  History only, by design — this dashboard looks back, it doesn't monitor.
- MODES on each provider tab: Claude shows how often extended thinking fired,
  Codex shows the reasoning-effort mix.

### Fixed

- Duplicate Claude log lines for the same message merge their metadata instead
  of keeping only one line's fields.
- `codex-auto-review` sessions are priced as the Codex default model instead of
  silently costing nothing.

## [0.8.0] - 2026-07-08

### Changed

- The codename becomes a rank you climb. Rank comes from 30-day token
  throughput alone, and each rank splits into steps with one animal per step,
  so all 24 animals are milestones on a single climb from Ant to Lion.
- The rank is now shown: a pill on the share card, a nameplate under the TUI
  badge, and a line in the caption. The step inside a rank stays hidden — the
  animal is the step.
- Rank colours run from purple at the top down to ink at the bottom, adjusted
  for the dark card. The animal watermark keeps its own colour.
- Provider tabs rank on their own volume instead of inheriting one rank.
- Sharing from the dashboard always exports the Total card, whichever tab is
  open — one canonical card per person.

### Removed

- The orchestration tier from the codename. Parallelism stays on the card and
  dashboard as plain stats.

## [0.7.0] - 2026-06-26

### Added

- `--no-cursor` disables the Cursor collector, the only one that sends a
  credential off your machine.

### Changed

- Model labels are sanitized before they can reach the card or clipboard, so a
  crafted log can't smuggle a repo name or a path onto an artifact meant to
  carry none. Legitimate names survive, including local-model tags and known
  provider namespaces.
- SECURITY.md documents the Cursor cookie egress and lists every provider read.

### Fixed

- The Cursor fetch no longer follows redirects, so the session cookie can only
  ever reach Cursor itself. The token is redacted in debug output.
- A Cursor row with an unreadable or truncated token cell now drops as a parse
  error instead of silently recording zero — degrade to less data, never a
  wrong number.

## [0.6.0] - 2026-06-26

### Added

- Cursor support. Cursor keeps no usage on disk, so reading it reaches the
  network: agent-walker reads your local session token and asks Cursor's own
  dashboard. Nothing is sent unless you are signed in.
- Antigravity token usage, previously activity-only. Each row is self-verified,
  so a future format change degrades to less data rather than wrong numbers.
- Usage events can carry a cost the provider reported, preferred over the
  price table when present.

### Changed

- Any model in the LiteLLM table is priced, Gemini included, so new providers
  cost out with no per-provider code.
- The no-data codename floor renders as Ant.

### Fixed

- Codex archived sessions are now scanned. Archiving moves a session out of the
  live directory, so archived work silently dropped out of the totals.

## [0.5.0] - 2026-06-23

### Added

- OpenCode support. Its local store is auto-detected and read read-only —
  never locking or writing your live database — contributing tokens, models,
  tools, projects, durations and activity.

## [0.4.0] - 2026-06-23

### Changed

- Reworked how the codename's working-style word is chosen, so it lines up
  with how people actually run their agents. Most codenames will change.
- The grid reads as how much you use against how well you orchestrate. The
  tier is the same on every tab; each tab's animal follows that provider's
  volume.
- Only agents you actually use get a tab, ordered by how much you use them.
- Antigravity is auto-detected. It stays activity-only, so it never moves the
  token totals.
- Renamed the "Combined" tab to "Total".

### Removed

- The `--agy` flag, now that Antigravity is detected automatically.

## [0.3.1] - 2026-06-22

### Changed

- Internal codename scoring tweaks. Most people see no change to their earned
  title.

## [0.3.0] - 2026-06-21

### Added

- Windows support, with prebuilt binaries and log discovery under the Windows
  user profile.
- `CODEX_HOME` and `CLAUDE_CONFIG_DIR` are honoured, so relocated agent state
  shows up instead of a blank dashboard.

### Changed

- No part of the codebase reads `HOME` directly anymore. WSL installs keep
  working with their existing paths.

## [0.2.1] - 2026-06-21

### Changed

- Refreshed the README recording so the preview matches the current ladder.

## [0.2.0] - 2026-06-21

First public release, with the codename system and the shareable stats card.

### Added

- A terminal dashboard over your local Claude Code and Codex CLI logs, parsed
  in parallel and cached per file so warm starts only reparse what changed.
- API-equivalent cost windows, cache-aware and priced from the LiteLLM
  database.
- Per-repository tokens, stacked per-model daily bars, an activity grid and an
  hour-of-day profile in your timezone.
- A turn-duration histogram weighted toward the unattended end.
- A shareable codename card you can copy or save. Repository names never
  appear on it.
- Opt-in Antigravity collection via `--agy`, activity only — its logs carry no
  token usage.
- Shell completions and card export via `--share <path>`.
- Dual MIT / Apache-2.0 licensing, distributed via npm with attestations on
  the release binaries.
- A Japanese README, cross-linked with the English one.

## [0.1.0] - 2026-06-20

Initial npm packaging.

[0.19.1]: https://github.com/miiiiiiich/agent-walker/compare/v0.19.0...v0.19.1
[0.19.0]: https://github.com/miiiiiiich/agent-walker/compare/v0.18.1...v0.19.0
[0.18.1]: https://github.com/miiiiiiich/agent-walker/compare/v0.18.0...v0.18.1
[0.18.0]: https://github.com/miiiiiiich/agent-walker/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/miiiiiiich/agent-walker/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/miiiiiiich/agent-walker/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/miiiiiiich/agent-walker/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/miiiiiiich/agent-walker/compare/v0.13.2...v0.14.0
[0.13.2]: https://github.com/miiiiiiich/agent-walker/compare/v0.13.1...v0.13.2
[0.13.1]: https://github.com/miiiiiiich/agent-walker/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/miiiiiiich/agent-walker/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/miiiiiiich/agent-walker/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/miiiiiiich/agent-walker/compare/v0.10.1...v0.11.0
[0.10.1]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.10.1
[0.10.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.10.0
[0.9.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.9.0
[0.8.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.8.0
[0.7.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.7.0
[0.6.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.6.0
[0.5.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.5.0
[0.4.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.4.0
[0.3.1]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.3.1
[0.3.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.3.0
[0.2.1]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.2.1
[0.2.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.2.0
[0.1.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.1.0
