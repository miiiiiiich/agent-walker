# Contributing

Thanks for your interest! agent-walker is a small, focused tool — issues and
PRs are welcome.

## Development

```sh
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo machete   # unused dependencies (cargo install cargo-machete)
typos           # spelling, exceptions in _typos.toml (cargo install typos-cli)
```

All of these must pass; CI enforces them (tool versions are pinned in
`.github/workflows/`) and additionally runs:

- `cargo deny check` — dependency policy in `deny.toml`
- `actionlint` (needs `shellcheck`) and
  `uvx --from 'zizmor==1.29.0' zizmor .github/workflows`, from the repo root —
  release.yml's exceptions are in `.github/actionlint.yaml` and `.github/zizmor.yml`
- when `scripts/` or a workflow that runs the scripts changes:
  `cd scripts && uv run pytest -q && uv run ruff check . && uv run ruff format --check . && uv run pyright`

The crate uses `clippy::pedantic` — narrow `#[allow]`s with a `reason` are fine
where the lint hurts readability.

## Useful dev flags

- `agw --render=120` — render every tab as plain text (no TTY needed)
- `agw --json` — summaries and dated events as JSON
- `agw --no-cache` — bypass the parse cache

## Conventions

- Collectors treat log files as untrusted input: skip and count malformed
  lines, never panic on them.
- Aggregating untrusted numbers (token counts, costs) uses saturating/checked
  arithmetic — never plain `+`/`sum()` that wraps silently in release builds.
- Time-of-day bucketing is in the user's local timezone, decided at startup.
