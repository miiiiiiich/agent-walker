# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-06-20

Initial public release.

### Added

- Terminal dashboard aggregating local AI coding-agent logs (Claude Code and
  Codex CLI), parsed in parallel and cached per file for ~100 ms warm starts.
- API-equivalent cost windows (today / 7d / 30d / period), cache-aware and
  priced from the LiteLLM pricing database.
- Per-repository token breakdown, stacked per-model daily bars, a GitHub-style
  activity grid, and an hour-of-day profile in the local timezone.
- Autonomy view: a turn-duration histogram weighted toward the 20-minute-plus
  unattended range.
- Shareable "codename" stats card — one of 24 ranks (`[OPS] [ANIMAL]`) placed on
  a 6×4 grid by tokens-per-day and working style — that you can copy to the
  clipboard or save to `~/Downloads`. Repository names never appear on it.
- Opt-in Antigravity CLI collection via `--agy` (activity only; its logs carry
  no token usage).
- Shell completions (`--completions`) and card export (`--share <path>`).
- Dual MIT / Apache-2.0 licensing; distributed via npm (`npx` / `bunx`), with
  GitHub Artifact Attestations on the release binaries.

### Known limitations

- Antigravity token and cost figures are not counted — its usage lives in an
  undocumented protobuf store.
- Windows is not yet supported; log discovery relies on `$HOME`.

[0.1.0]: https://github.com/miiiiiiich/agent-walker/releases/tag/v0.1.0
