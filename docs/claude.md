# Claude Code

## Where the data comes from

`~/.claude/projects/**/*.jsonl` — one JSONL transcript per session. agent-walker
reads them directly and read-only. Nothing is uploaded.

## History retention (keep more than 30 days)

Claude Code **deletes transcripts after 30 days by default**, so on first use the
dashboard only shows the last month no matter how long the window is. To keep
more history, raise the limit in `~/.claude/settings.json`:

```json
{ "cleanupPeriodDays": 365 }
```

Cleanup runs on session start, so anything already older than 30 days is gone —
set this early. (Codex CLI keeps its sessions indefinitely; no setting needed.)

## What's captured

- **Token usage** per assistant message: input, output, cache creation (write),
  cache read.
- **Tools**: real tool names (`Bash`, `Read`, `Edit`, `Write`, `Grep`, `Agent`, …).
- **Subagents**: the `Agent` tool and subagent-attributed usage → the SUBAGENTS
  section.
- **Sessions / activity**: session ids and timestamps → sessions, active days,
  streak, hourly profile, and the concurrency (PARALLEL AGENTS) sweep.
- **Projects**: derived from the working directory the session ran in.
- **Models**: the model id on each message.

## How tokens are counted (important)

```
tokens = input + output + cache_creation + cache_read
```

Claude Code re-sends the **entire conversation context every turn**, so almost
all of it is served from the prompt cache and lands in **cache_read**. In
practice cache_read is **~90–96%** of the total. Those tokens are real (the model
reads them) and they are billed — just cheaply (≈0.1× input) — but they are the
*same context re-read*, not new work.

So the headline token number is a **scale** signal, not "fresh work":

- fresh work ≈ `input + cache_creation + output` (a few hundred million for a
  heavy user)
- the rest is context re-reads

This is why the total can be ~20–25× what a tool that excludes cache reads shows.
agent-walker counts cache-inclusive on purpose — the same convention `ccusage`
uses.

> cache% is intentionally **not** surfaced as a metric: it sits near 95% for
> everyone, so it distinguishes no one.

## Deduplication

Claude writes the same usage line into multiple files when a session is
forked/resumed. agent-walker dedupes by `(message.id, requestId)` so volume is
not double-counted.

## Cost

COST is **API-equivalent** and **cache-aware**:

```
input×rate + output×rate + cache_read×cache_read_rate + cache_write×write_rate
```

Per-model rates come from the LiteLLM pricing database (the source `ccusage`
uses). Claude cache reads price at ≈0.1×, cache writes at 1.25× (5-minute cache)
or 2× (1-hour cache).

If you are on a Claude subscription (Pro/Max), this is **not your bill** — it is
what the same usage would cost at pay-as-you-go API rates. Use it to answer "is
the subscription worth it," not "what did I pay."

## Why this won't match Claude's own usage views

agent-walker counts **cache-inclusive raw tokens over a trailing N-day window**.
Claude's own surfaces count differently, so the numbers won't line up — and the
difference is both *window* and *what's included*:

- **`/cost` and the Plan-usage bar don't show cache reads.** For a heavy user
  cache reads are ~95–99% of all tokens, so these displays look tiny next to
  agent-walker's total. (The same omission is why their cost line can read far
  below the real metered cost.)
- **The `context_window` status line includes cache reads but is a snapshot** —
  the current context size after the latest response, not a cumulative total.
- **The Anthropic Console usage report** splits tokens into uncached / cache
  read / cache write / output, but it is per billing window and needs an Admin
  API key.

Different window + different inclusion rules = different number. agent-walker is
one consistent cross-agent view, not a mirror of any single Claude screen.
