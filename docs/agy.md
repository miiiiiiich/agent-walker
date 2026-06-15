# Antigravity CLI (agy · Gemini)

## Where the data comes from

`~/.gemini/antigravity-cli` — `history.jsonl` (timestamps) and `log/cli-*.log`
(model name + confirmed commands). Read-only.

## What's captured — and what isn't

- ✅ **Activity**: session timestamps → active days, hourly profile, streak.
- ✅ **Model**: the model name from the CLI log.
- ✅ **Some tools**: only the commands you *confirmed* in approval dialogs
  (`command:<exe>`).
- ❌ **Tokens and cost are not captured.** agy reports ~0 tokens and $0.

## Why tokens are missing (design constraint)

Antigravity's real usage data lives in `.db` / `.pb` files as **protobuf blobs**
that need Antigravity's private `.proto` schema to decode. That schema is
undocumented and changes between versions, so parsing it would be fragile and
break often. agent-walker deliberately does **not** read it.

## What this means

Treat agy numbers as **activity-only**. If you use Antigravity meaningfully, its
tokens and cost are **not** reflected in agent-walker's totals — including the
Combined view. This is a known gap, not a bug: it is the price of not coupling to
an undocumented binary format.
