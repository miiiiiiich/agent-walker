# Contributing

Thanks for your interest! agent-walker is a small, focused tool — issues and
PRs are welcome.

## Development

```sh
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

All of these must pass; CI enforces them and additionally runs `cargo audit`. The crate uses `clippy::pedantic` —
narrow `#[allow]`s with a `reason` are fine where the lint hurts readability.

## Useful dev flags

- `agw --render=120` — render every tab as plain text (no TTY needed)
- `agw --snapshot` — aggregate stats as text
- `agw --no-cache` — bypass the parse cache

## Conventions

- Collectors treat log files as untrusted input: skip and count malformed
  lines, never panic on them.
- Aggregating untrusted numbers (token counts, costs) uses saturating/checked
  arithmetic — never plain `+`/`sum()` that wraps silently in release builds.
- Time-of-day bucketing is in the user's local timezone, decided at startup.
