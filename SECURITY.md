# Security Policy

## Reporting a vulnerability

Please report security issues privately via
[GitHub Security Advisories](https://github.com/miiiiiiich/agent-walker/security/advisories/new)
— do not open a public issue for vulnerabilities.

You can expect an initial response within a week.

## Scope notes

agent-walker reads local log files written by AI coding agents
(Claude Code, Codex CLI, Antigravity CLI) and renders aggregate statistics.

- The only network access is a daily fetch of public model-pricing metadata
  from [LiteLLM's pricing database](https://github.com/BerriAI/litellm).
  No usage data, log content, or telemetry is ever transmitted.
  `--offline` disables all network access.
- Log lines are treated as untrusted input: malformed JSON is counted and
  skipped, never executed or evaluated.
- Release binaries carry GitHub Artifact Attestations (SLSA build provenance);
  verify with `gh attestation verify <file> -R miiiiiiich/agent-walker`.
