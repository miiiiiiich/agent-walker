# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.14.0] - 2026-08-19

### Added

- **MODES**: the autonomy mix — how much of the month ran under each
  permission mode. Claude's `permissionMode` (default / acceptEdits / plan /
  bypass / auto) and Codex's `approval_policy` (never / on-request …), read
  alongside the effort mix: how much rope you actually give the agent.
- **COMPLETION**: interruption count in the title — turns you cut short.
  Claude esc markers and Codex `turn_aborted`, deduped across resume / fork
  copies. Interrupted turns no longer leak into the completion percentiles,
  so p50 / p90 describe turns that actually finished. Also in `--snapshot`
  as `completion_interrupted`.

### Fixed

- Cost no longer reads `$0` when pricing is unknown. If the LiteLLM pricing
  table can't be fetched (it lives on raw.githubusercontent.com) or a model
  id is missing from it, the share card shows `—` and drops the cost line
  from its caption, and the COST section shows `—` rows and names the
  unpriced volume — instead of silently summing those tokens as free.
- Release tweets no longer cut a sentence mid-word. Bullets that don't fit
  are dropped whole.

### Changed

- `resvg` 0.48: text rendering now on the fontations / harfrust stack.
  Share-card text may differ by a sub-pixel or two.

## [0.13.2] - 2026-08-14

### Added

- **MODES** (Claude tab): the reasoning-effort mix, read from the top-level
  `effort` field Claude Code writes since v2.1.212. Delegated (subagent)
  turns are included, so the mix covers what you hand to subagents too —
  the panel sits next to the thinking / fast dials, matching the Codex
  tab's effort mix.

### Changed

- Release notes are now written from this changelog instead of the
  generated installer boilerplate, and every release is announced on X
  automatically.

### Removed

- Intel macOS (`x86_64-apple-darwin`) prebuilt binaries. It was the
  slowest build in the pipeline for a platform Apple stopped shipping;
  Apple Silicon remains supported.

## [0.13.1] - 2026-07-30

### Added

- `THIRD_PARTY_NOTICES` — ccusage's copyright notice and full MIT license
  text, in recognition of its prior work on problems agent-walker also
  addresses (#50). The notice ships in every binary release archive, the
  npm package, and the cargo source package. The README gains an
  acknowledgements section.

## [0.13.0] - 2026-07-29

### Changed

- Chart density unified: every column chart (TOKENS PER DAY, BY HOUR,
  LIMITS, CREDITS) now renders exactly one character per column through a
  shared frame — the 2-chars-per-bar widening on wide terminals is gone,
  and left-rail charts stack with matching widths instead of jumping
  between densities. Horizontal stat bars share one track/row shape across
  sections. Purely visual; no numbers change.

## [0.12.0] - 2026-07-28

### Added

- Grok Build (xAI) support: a new auto-detected provider tab reading
  `~/.grok/sessions/*/*/updates.jsonl` (override with `--grok-dir` /
  `GROK_HOME`). Per-prompt token deltas with per-model splits come from the
  durable `turn_completed` records; fork copies deduplicate by `prompt_id`
  (their timestamps are rewritten, the id is not), and subagent session
  directories are excluded because the coordinator already folds their
  usage into its own totals. See docs/grok.md for the full contract.

## [0.11.0] - 2026-07-28

### Fixed

- Claude: advisor calls (`advisor_message` inside `usage.iterations`) are
  billed under their own model but absent from the top-level counters, and
  were silently dropped. They now surface as their own usage events.
  Failed fallback attempts stay excluded by design — the top level is the
  turn's billed usage for the serving model, and a failed attempt is not
  billed.

### Added

- GitHub Copilot CLI support: a new auto-detected provider tab reading
  `~/.copilot/session-state/*/events.jsonl` (override with `--copilot-dir` /
  `COPILOT_HOME`). Tokens come from the per-model totals the CLI writes on
  clean exit, counted as deltas between exits so resumed sessions neither
  double-count nor fall out of the analysis window. Sessions that never exit
  cleanly contribute activity but no tokens until they close — see
  docs/copilot.md for the full contract. Dotted model spellings
  (`claude-sonnet-4.6`) now resolve against dashed pricing keys. The tab
  also carries a Copilot-only CREDITS panel — daily AI-credit spend rebuilt
  from the cumulative nano-AIU ledger, which keeps tracking even for
  sessions that never exit cleanly — and COMPLETION turn durations from the
  CLI's explicit turn boundaries.

## [0.10.1] - 2026-07-21

### Fixed

- Codex fork/spawn child rollouts replay the parent session's history with
  timestamps rewritten to the fork instant, and the positional dedup key
  (session, timestamp, line index) counted the replayed events again —
  inflating token totals, LIMITS samples, and MODES effort mix ([#36]).
  Usage, rate-limit, and effort events are now deduplicated by content
  (usage vectors + running cumulative; `turn_id` for effort), re-emitted
  `token_count` events whose cumulative did not advance collapse too, and
  keyed duplicates keep the earliest observed timestamp, so scan order
  can't decide attribution. Fork-shaped rollouts (second `session_meta`)
  additionally skip the replay burst at the fork instant wholesale — so
  replayed history contributes nothing (tokens, rate-limit samples,
  efforts, durations, session touches) even when the parent rollout is
  outside the scan window, and Desktop thread branches without a
  `thread_spawn` marker are covered too.

## [0.10.0] - 2026-07-13

### Changed

- Codename: retuned the SS band so the top of the ladder is realistically
  reachable. Ranks below SS are unchanged; nobody moves down.

## [0.9.0] - 2026-07-08

Three new views of how you drive your agents, all cut on the fixed 30-day
window and all TUI-only — the share card is untouched.

### Added

- **SKILLS** (Claude tab): token volume per skill, read from the
  `attributionSkill` field recent Claude Code versions write. Shares are of
  attributed volume; the subtitle carries the honest denominator ("attributed
  N% of volume") since most tokens flow outside skills.
- **LIMITS** (Codex tab): daily peak of the plan's 5-hour window utilization,
  rebuilt from the `rate_limits` snapshots Codex rollouts record. Fixed
  0-100% axis, red bar on limit-hit days, faint dot for days without Codex
  use. History only, by design — this dashboard looks back, it doesn't
  monitor.
- **MODES** (per provider tab): each provider's own thinking dial — Claude
  shows how often extended thinking actually fired (block presence only; the
  text is never read) plus fast mode once it has data, Codex shows the
  reasoning-effort mix from `turn_context`.

### Fixed

- Duplicate Claude log lines for the same message now merge their metadata
  (attribution, model, project) instead of keeping only the larger-volume
  line's fields.
- `codex-auto-review` sessions are priced as the Codex default model instead
  of silently costing $0.
- The `--agy-dir` help text no longer claims Antigravity is activity-only —
  token usage has been decoded from its conversation store since v0.7.

## [0.8.0] - 2026-07-08

The codename becomes a rank you climb.

### Changed

- Codename reworked into a single-axis rank ladder. RANK (SS/S/A/B/C/D/E;
  unranked below E, or when the 30-day sample is too thin to rank) comes from
  30-day token throughput alone; each rank splits
  into log-uniform STEPs and every step is one animal, so all 24 animals are
  reachable milestones on one climb from Ant to Lion (the SS band extrapolates
  its ratio upward, putting Lion near 5B tokens/day). The orchestration-tier
  column (Scout/Tools/Parallel/Apex) and its parallel/tooling signals are gone
  from the codename — parallelism stays on the card and dashboard as plain
  stats.
- The rank is now displayed: a pill badge above the share card's title, in the
  slot the "CODENAME" label used to occupy (the label said nothing the card
  doesn't already show, so it is retired), a `────  RANK A  ────` nameplate
  under the TUI badge art, "— Rank A" in the share caption, and a `rank:`
  field in the plain-text output. The step position inside a rank is
  deliberately not surfaced — the animal itself is the step.
- Rank colours follow the 冠位十二階 ladder (603 AD, the oldest colour-coded
  rank system): SS 濃紫 / S 薄紫 / A 青 / B 赤 / C 黄 / D 白 / E 墨, hues
  adjusted for the dark card (the ink-black E is lifted on both surfaces).
  The card badge and the TUI nameplate both use them; the animal watermark
  stays OPS-coloured.
- Provider tabs rank on their own volume — the whole-person style inheritance
  (`for_summary_styled`) is removed.
- Sharing from the dashboard now always exports the Total card, whichever tab
  is open — one canonical card per person, matching the CLI `--share` path
  (per-tab cards had been the interactive behaviour since 0.2.0).

## [0.7.0] - 2026-06-26

Security and robustness hardening from a design/security review of 0.6.0.

### Added

- `--no-cursor` disables the Cursor collector — the only collector that sends a
  credential off the machine — stopping that egress (the anonymous LiteLLM
  pricing fetch still runs). Documented in the README and `docs/cursor.md`.

### Changed

- Model labels are sanitized before they can reach the shareable card /
  clipboard: a label containing path separators, control characters, or other
  out-of-set characters collapses to `Other` (so a crafted log can't smuggle a
  repo name, absolute path, or token-like string onto an artifact meant to carry
  none), while legitimate names are preserved — including local-model `:tags`
  (`qwen3:8b`) and known `provider/model` namespaces (`openai/gpt-4o`), whose
  recognized provider prefix is stripped for display.
- `SECURITY.md` now documents the Cursor session-cookie egress and lists every
  provider read; `docs/agy.md` corrects the Antigravity drift explanation —
  protobuf identifies fields by tag, so the per-row `#3 == #9 + #10` check is a
  mutual-consistency check on the output tags (it catches a re-meaning of those
  output tags, not of the input tags).

### Fixed

- The Cursor usage fetch no longer follows redirects, so the session cookie can
  only ever reach `cursor.com`, never a redirect target. The session token is
  restricted to the JWT character set and bridged-OAuth account ids are
  validated (rejecting CR/LF and cookie delimiters), closing a header-injection
  path through the JWT-decoded account id; `CursorConfig`'s `Debug` redacts the
  token so it can't leak into a log or panic message.
- A Cursor CSV row with an unparseable token cell (e.g. a thousands-separated
  `1,234`) or one truncated before the token columns now drops as a parse error
  instead of silently recording 0 tokens — degrade to less data, never a wrong
  number.

## [0.6.0] - 2026-06-26

### Added

- Cursor support. Cursor keeps no usage on disk, so — unlike every other
  provider — reading it reaches the network: agent-walker reads the local Cursor
  session token from `state.vscdb` and queries Cursor's own usage dashboard for
  per-model tokens (input / output / cache) and Cursor's reported cost. It's
  auto-detected when you're signed in and skipped (no request) when there's
  nothing to read — not installed, signed out, or the fetch doesn't return — so
  nothing is sent unless you're signed in. `CURSOR_TOKEN` supplies the token
  directly. Cursor exposes no project, tools, sessions, or durations, so those
  panels stay empty for it.
- Usage events can now carry a provider-reported cost (`reported_cost_usd`),
  preferred over the LiteLLM price when present — Cursor's models aren't in the
  LiteLLM table, so its own reported figure is used.
- Antigravity token usage. Previously activity-only, it now reads real per-model
  token counts (input / cache-read / output / thinking), model, and project from
  the CLI's per-conversation SQLite stores (`conversations/*.db`). The data is an
  unlabeled protobuf decoded by field number and **self-verified per row** (the
  stored output total must equal text + thinking), so a future format change
  degrades to less data, never wrong numbers. Cost is estimated like every other
  agent (see Changed).

### Changed

- Pricing is no longer restricted to `claude-*` / `gpt-*`. Any model whose id is
  in the LiteLLM table is priced (Gemini included), so new providers cost out
  with no per-provider code. Region/variant duplicates and absurd rates are
  still filtered, non-chat entries are dropped, and a prefix match now requires a
  date/version suffix so unrelated ids can't collide with a shorter base key.
- The no-data codename floor now renders as **Ant** (previously Chick).
- Bump `dirs` 5 → 6 (dependency-only; no behavior change for the directory
  lookups this uses).

### Fixed

- Codex `archived_sessions/` is now scanned. The desktop app *moves* (not copies)
  a session's JSONL from `sessions/` to `archived_sessions/` when archived, so
  those sessions silently dropped out of the totals; both directories are now
  read and deduplicated by relative path before parsing, so duplicates can't
  double-count session or duration stats.

## [0.5.0] - 2026-06-23

### Added

- OpenCode support. Its local SQLite store (`~/.local/share/opencode`, or
  `$OPENCODE_HOME` / `$XDG_DATA_HOME`) is auto-detected and read **read-only**
  (never locks, checkpoints, or writes your live store), contributing tokens,
  models, tools, projects, durations, and activity — near parity with Claude and
  Codex. Follows OpenCode's own resolver: `OPENCODE_DB` overrides the file, and
  per-channel `opencode-<channel>.db` stores are picked up alongside the default
  (deduped by row id). Reasoning tokens are folded into the output total.
  `--opencode-dir` points at a non-standard data dir.

## [0.4.0] - 2026-06-23

### Changed

- Reworked how the codename's working-style word is chosen so it lines up with
  how people actually run their agents. **Most people's codename will change.**
  The exact rule stays undisclosed by design — the title is the only thing
  surfaced.
- The grid now reads as **how much (row) × how well you orchestrate (column)**.
  The column is an ordered tier measured across *all* your agents combined and
  built so it doesn't simply climb with volume. It's the same tier on every tab —
  your identity — while each tab's animal is scaled by that provider's volume.
  The exact rule stays undisclosed by design.
- Renamed the "Combined" tab to "Total".
- Provider tabs are now data-driven: only agents you actually use get a tab, and
  they're ordered by how much you use them (most-used first, Total last). An
  empty agent no longer shows a blank tab.
- Antigravity is auto-detected — its tab appears whenever its logs are present,
  with no flag to set. It stays activity-only (no token usage), so it never
  affects the token totals and sorts last.

### Removed

- The `--agy` flag. Antigravity is now detected automatically; use `--agy-dir`
  to point at a non-standard location.

## [0.3.1] - 2026-06-22

### Changed

- Internal codename scoring tweaks. Most users see no change to their
  earned title; the exact tuning stays undisclosed by design.

## [0.3.0] - 2026-06-21

### Added

- Windows support. Prebuilt binaries are now published for
  `x86_64-pc-windows-msvc`, and the dashboard discovers logs under
  `%USERPROFILE%` (`.claude\projects`, `.codex\sessions`,
  `.gemini\antigravity-cli`).
- CI now exercises `cargo test` on `windows-latest` alongside macOS and Linux.
- Honor `CODEX_HOME` (Codex CLI's official override) and `CLAUDE_CONFIG_DIR`
  (Claude Code's de-facto override) so users with relocated agent state still
  see their usage instead of a blank dashboard.

### Changed

- Path resolution went through `dirs::home_dir`, so no part of the codebase
  reads `HOME` directly anymore. WSL installs keep working with their existing
  Linux-style paths.

## [0.2.1] - 2026-06-21

### Changed

- Refreshed the README demo recording so the dashboard preview matches the
  0.2.x codename ladder and apex animal.

## [0.2.0] - 2026-06-21

First public release with the codename system and the shareable stats card.

### Added

- Terminal dashboard aggregating local AI coding-agent logs (Claude Code and
  Codex CLI), parsed in parallel and cached per file so warm starts only
  reparse files whose `(mtime, size)` changed.
- API-equivalent cost windows (today / 7d / 30d / period), cache-aware and
  priced from the LiteLLM pricing database.
- Per-repository token breakdown, stacked per-model daily bars, a GitHub-style
  activity grid, and an hour-of-day profile in the local timezone.
- Autonomy view: a turn-duration histogram weighted toward the 20-minute-plus
  unattended range.
- Shareable "codename" stats card — a usage-based rank you can copy to the
  clipboard or save to your OS Downloads folder. Repository names never
  appear on it.
- Opt-in Antigravity CLI collection via `--agy` (activity only; its logs carry
  no token usage).
- Shell completions (`--completions`) and card export (`--share <path>`).
- Dual MIT / Apache-2.0 licensing; distributed via npm (`npx` / `bunx`), with
  GitHub Artifact Attestations on the release binaries.
- A Japanese README (`README.ja.md`), cross-linked with the English one.

### Known limitations

- Antigravity token and cost figures are not counted — its usage lives in an
  undocumented protobuf store.

## [0.1.0] - 2026-06-20

Initial npm packaging.

[#36]: https://github.com/miiiiiiich/agent-walker/issues/36
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
