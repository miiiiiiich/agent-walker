# Claude Code

## Where the data comes from

`~/.claude/projects/**/*.jsonl` — one JSONL transcript per session. agent-walker
reads them directly and read-only. Nothing is uploaded.

**Input bounds.** Only regular files are opened; a symlink to one is followed,
while a FIFO or device counts as an unreadable file. A line longer than 64 MiB,
or one holding more than 262,144 JSON array elements and object entries, is
skipped as a parse error. A timestamp before 1970 or from year 3000 on is
treated as missing. An id or name longer than 1,024 bytes, or a path longer
than 4,096, is treated as missing.

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
  cache read. A reply that consulted the advisor records the advisor's call as
  an `advisor_message` entry in `usage.iterations`, apart from the message's
  own counters; it is counted as a call of its own, under the advisor's model
  and priced at that model's rates.
- **Tools**: real tool names (`Bash`, `Read`, `Edit`, `Write`, `Grep`, `Agent`, …).
- **Subagents**: the `Agent` tool and subagent-attributed usage → the SUBAGENTS
  section.
- **Sessions / activity**: session ids and timestamps → sessions, active days,
  streak, hourly profile, and the concurrency (PARALLEL AGENTS) sweep.
- **Projects**: derived from the working directory the session ran in.
- **Models**: the model id on each message.
- **Turns** (TURN LENGTH, working time, pace, permission modes): a turn starts
  at a prompt you send. Prompts Claude Code writes itself (background-task
  notifications, messages from other sessions, scheduled-task runs, compaction
  summaries, shell and local-command output) don't start a new turn while one
  is in progress; the work they trigger counts toward that turn. When no turn
  is in progress (a scheduled run, or a notification after a background job
  longer than the 30-minute silence cutoff), such a prompt opens a turn of its
  own, marked `generated` in the JSON export. It counts toward TURN LENGTH and
  working time like any turn, but adds no pace sample or permission mode. A
  prompt the model never answers, such as a local slash command or a request
  cancelled before the first reply, is not a turn. A manual `/compact` is the
  exception: the summary is model work, so it counts once as a turn.

**Background helpers.** A transcript under a directory whose name contains
`claude-mem-observer` belongs to the claude-mem plugin's observer, which keeps
sessions open in the background. Its calls count toward tokens, cost, models,
projects and the codename's volume, like any other call. They count toward
none of the activity: sessions, active days, streak, hourly profile, turns,
working time, PARALLEL AGENTS, tools and modes. In the CONTEXT section they
fall in the `other` row, and in the JSON export their usage rows have
`"source": "background"`. The observer's own cost-state totals (below) are
added the same way.

Tool calls come only from the transcript's `user` and `assistant` rows, and
so does usage, apart from the calls only the `cost-state` rows record (below).
The request-log rows (`api-request`, `api-request-blob`, `api-request-shape`)
replay the conversation being sent, so they are skipped.

## Calls only the cost-state records

Claude Code writes `{"type":"cost-state"}` rows into a session's main
transcript: the running total of the process, per model, of input, output,
cache writes (split into 5-minute and 1-hour) and cache reads. The rows carry
no timestamp. The total also counts calls that never get an `assistant` row of
their own. The log does not say which calls these are.

- **What is added.** Per session, model and field, the part of the total over
  the session's transcript: `max(0, total − transcript)`. The transcript side
  is the session's main and subagent files with advisor calls, each
  `message.id` once within the session. A field the transcript is ahead on
  (a total that started after some of the session's calls) adds nothing. A
  model keyed with a context suffix, `claude-opus-5[1m]`, is its plain model,
  under which its calls are logged.
- **Several processes.** A resumed session can be run by more than one
  process, each with its own `startTime`. A process that carries the previous
  total forward continues it, and the last total is compared with all the
  calls. A process whose total starts again from zero begins a new run, and
  each run's last total is compared with the calls made before it was
  written. When calls follow the last total, their process has recorded no
  total yet; the total is compared with the calls before it, and the later
  calls add nothing until their process writes one. If two main files hold
  totals for the same session, only the one written last is used.
- **Forks.** A session forked from another starts from the other's total
  (with its `startTime`) and copies only part of its calls. The other
  session's last total written before the fork's first call of its own is
  taken off the fork's totals, and the copied calls that total covers are left
  out of the fork's side, so the other session's uncounted part is not added
  twice. Sessions that only share a `startTime`, with no call in common, keep
  separate totals and are compared as they are.
- **Where it lands.** The added usage is spread over the local hours the
  session made calls in, in proportion to its transcript tokens in each hour,
  and each part is dated at the session's last call in that hour. It adds no
  session, active day, turn or working time, the hourly profile follows the
  session's own hours, and the share of a long session from before the window
  stays outside it. A session with no dated call adds nothing.
- **How it counts.** Each part is a summed row: priced at base rates, never a
  long-context tier, and at standard speed and geography, which the total
  does not record. It counts as one call in its model's row, falls in the
  CONTEXT section's `other` row, and goes to the project of the call it is
  dated at.
- So that a long session is compared whole, its subagent files are read even
  when they are older than the files the window would otherwise read; the
  rows in them are older than any window.

## How tokens are counted (important)

```
tokens = input + output + cache_creation + cache_read
```

Claude Code re-sends the **entire conversation context every turn**, so almost
all of it is served from the prompt cache and lands in **cache_read**. In
practice cache_read is **~90–96%** of the total. Those tokens are real (the model
reads them) and they are billed — just cheaply (0.025×–0.1× input on current
models) — but they are the *same context re-read*, not new work.

So the headline token number is a **scale** signal, not "fresh work":

- fresh work ≈ `input + cache_creation + output` (a few hundred million for a
  heavy user)
- the rest is context re-reads

This is why the total can be ~20–25× what a cache-excluding counter shows.
agent-walker counts cache-inclusive on purpose, so the total reflects everything
the model actually processed.

The **CONTEXT** section reports the cached share over the fixed 30-day window
(the share card quotes it only on a 30-day report). On its own the share sits
near 95% for most people; what distinguishes users is where the *rest* goes,
so the section breaks the input-equivalent volume down by context-size band,
expired resumes, cold starts, ordinary uncached input, and an `other` row for
volume that is not attributable to a call (subagent rows, the calls only the
cost-state records, providers whose logs record aggregates) — see `src/analyzer/context.rs` for the definitions.

## Deduplication

Claude writes the same usage line into multiple files when a session is
forked/resumed. agent-walker dedupes by `message.id` so volume is not
double-counted. A streamed reply can also be written as several rows whose
usage grows; of all the copies of one `message.id`, the one with the most
tokens is kept, dated at the earliest timestamp any copy carries.

## Known gaps

- **Subagent output before Claude Code 2.1.307 is undercounted.** Until that
  release a subagent transcript recorded a placeholder output count (a few
  tokens) and an empty stop reason for a streamed reply. Input and cache
  counts are real. No row holds the real output count, so it cannot be
  recovered.

## Cost

COST is **API-equivalent** and **cache-aware**:

```
input×rate + output×rate + cache_read×cache_read_rate + cache_write×write_rate
```

Per-model rates come from the LiteLLM pricing database, cache rates included:
on current Claude models a cache read costs 0.025×–0.1× the input rate, and a
cache write 1.25× (5-minute cache) or 2× (1-hour cache).

Which cache a write went to comes from `usage.cache_creation`; a write it does
not split is priced at the 5-minute rate. On an advisor turn that split covers
only the reply's first `message` iteration while the write total covers all of
them, so the split is taken as the sum of the `message` iterations' splits
instead.

**A model prices only at its own rates.** A logged model id uses the LiteLLM
key it equals, or the key it extends by `-latest` or by a date (`-20251001`,
`-2026-10-01`). Any other id shows as unpriced (—) until LiteLLM lists it,
including a newer version of a listed model: `claude-opus-5-6` does not price
as `claude-opus-5`.

**Long-context tiers are priced per request.** Where LiteLLM lists a tier for a
model (`input_cost_per_token_above_<N>k_tokens`, with the same
`_above_<N>k_tokens` suffix on the output, cache-read and cache-write rates,
and `cache_creation_input_token_cost_above_1hr_above_<N>k_tokens` for 1-hour
writes), a request whose prompt is over N tokens pays the tier rates on every
one of its tokens, not only the part past N. The prompt counts fresh input,
cache reads and cache writes, as Anthropic does; each request is judged on its
own, never a day's or a session's sum. A rate the tier does not list keeps its
base value, as in LiteLLM's own calculator. Claude Haiku 5.5 is tiered this way
(over 100,000 prompt tokens); Opus and Sonnet 5.5 price their whole 1M window
at the base rates, and LiteLLM lists no tier for them.

**Fast mode and US-only inference are priced where the log records them.** A
call whose `usage.speed` is `fast` pays the fast-mode multiplier LiteLLM lists
for its model (`provider_specific_entry.fast`), and one whose
`usage.inference_geo` is `us` pays the US multiplier (`provider_specific_entry.us`).
Each scales every rate of the request, long-context rates included, and the two
stack, as Anthropic bills them. A model LiteLLM lists no multiplier for keeps
its rates. An advisor iteration records neither field of its own, so it prices
at its model's standard rates.

**Rates are bounds-checked**, for every provider. A rate must be a number from
0 to $0.01 per token ($10,000 per million tokens). That is about 16× the
highest rate agent-walker reads from LiteLLM's table (o1-pro output, $600 per
million), so no listed model is refused, but a corrupted or tampered entry
cannot price a month of tokens in the millions. A base rate outside the bounds
drops its model, which then shows as unpriced (—), never as $0. A tier rate
outside the bounds drops only the tier, and the model prices at its base
rates. A tier threshold under 1,000 prompt tokens is ignored, and the next
listed threshold is used. A multiplier must be from 1 to 10 (fast mode, the
highest listed, is 2×); one outside that is ignored, and the call prices at
standard rates. The same checks run again when the stored copy,
`~/.cache/agent-walker/pricing.json`, is read back. A stored fetch stamp that
is not a date is dropped, so the table is fetched again.

The table is fetched over HTTPS only, redirects included (at most 3), and a
body over 32 MB once decoded is refused. Only the fields pricing uses are
kept while the table is parsed, and a table with more than 50,000 entries is
refused whole (LiteLLM's has about 4,300); the stored copy is read under the
same limit. When the server answers but the table
is refused, agent-walker prints a warning and keeps pricing with the stored
copy; the date in the COST panel ("rates YYYY-MM-DD") shows how old it is.
Being offline prints nothing (`--verbose` shows it).

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
