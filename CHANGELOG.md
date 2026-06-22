# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-06-22

### Changed

- Reworked how the codename's working-style word is chosen so it lines up with
  how people actually run their agents. **Most people's codename will change.**
  The exact rule stays undisclosed by design — the title is the only thing
  surfaced.
- The working-style word now reads how you work across *all* your agents
  combined (so running several at once counts), and is measured so it doesn't
  simply climb with volume. It's the same word on every tab — your identity —
  while each tab's animal is scaled by that provider's volume.
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

[0.4.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.4.0
[0.3.1]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.3.1
[0.3.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.3.0
[0.2.1]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.2.1
[0.2.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.2.0
[0.1.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.1.0
