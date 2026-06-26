# Security Policy

## Reporting a vulnerability

Please report security issues privately via
[GitHub Security Advisories](https://github.com/miiiiiiich/agent-walker/security/advisories/new)
— do not open a public issue for vulnerabilities.

You can expect an initial response within a week.

## Scope notes

agent-walker reads local log/usage stores written by AI coding agents
(Claude Code, Codex CLI, Antigravity CLI, OpenCode, Cursor) and renders
aggregate statistics.

- **Network access.** Two kinds, both to read your own data — never any
  telemetry, log content, or usage upload:
  1. A per-run fetch of public model-pricing metadata from
     [LiteLLM's pricing database](https://github.com/BerriAI/litellm).
  2. **Cursor only:** Cursor keeps no usage on disk, so when you're signed in,
     agent-walker reads your local Cursor session cookie and sends it to
     `cursor.com` to fetch *your own* usage figures — the same request the
     dashboard makes. This is the one collector that transmits a credential off
     the machine. It is skipped when you're signed out, and `--no-cursor`
     disables it entirely (keeping agent-walker fully offline). The cookie goes
     only to `cursor.com` (redirects are not followed) and is never written to
     disk, logs, or the shareable card.
- Log lines and usage records are treated as untrusted input: malformed data is
  counted and skipped, never executed or evaluated. Numeric aggregation uses
  saturating arithmetic, and model labels are sanitized before they can reach the
  shareable card.
- Release binaries carry GitHub Artifact Attestations (SLSA build provenance);
  verify with `gh attestation verify <file> -R miiiiiiich/agent-walker`.
