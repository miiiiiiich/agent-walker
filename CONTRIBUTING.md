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

- `agentwalker --render=120` — render every tab as plain text (no TTY needed)
- `agentwalker --snapshot` — aggregate stats as text
- `agentwalker --no-cache` — bypass the parse cache
- `cargo run -- --update-pricing-snapshot assets/pricing.json` — refresh the vendored pricing snapshot

## Conventions

- Collectors treat log files as untrusted input: skip and count malformed
  lines, never panic on them.
- Time-of-day bucketing is in the user's local timezone, decided at startup.
