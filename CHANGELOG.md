# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.5.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.5.0
[0.4.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.4.0
[0.3.1]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.3.1
[0.3.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.3.0
[0.2.1]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.2.1
[0.2.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.2.0
[0.1.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.1.0
