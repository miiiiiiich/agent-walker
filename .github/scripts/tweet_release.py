#!/usr/bin/env python3
"""Compose and post the release announcement tweet.

Reads release.json ({tag_name, body}) produced by the workflow, builds a
<=280-char tweet from the Release Notes section, and posts it via the X v2
API (OAuth 1.0a user context). DRY_RUN=true prints the tweet and exits.
"""

import json
import os
import re
import sys

MAX_WEIGHT = 280
URL_WEIGHT = 23  # every URL counts as 23 chars on X


def weight(text: str) -> int:
    # CJK and most non-latin chars count as 2; keep it simple and safe.
    return sum(2 if ord(c) > 0x10FF else 1 for c in text)


def bullets_from(body: str) -> list[str]:
    # scope to the Release Notes section: strip its header, cut at the next H2
    body = re.sub(r"^## Release Notes\s*", "", body.strip())
    body = re.split(r"^## ", body, maxsplit=1, flags=re.M)[0]
    out = []
    for m in re.finditer(r"^- (.+?)(?=^[^\s]|\Z)", body, re.M | re.S):
        text = " ".join(m.group(1).split())  # unwrap hard-wrapped lines
        text = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", text)  # strip md links
        text = re.sub(r"https?://\S+", "", text)  # bare URLs would count as 23 on X
        text = text.replace("`", "").replace("**", "")
        text = re.sub(r"\s*\(#\d+\)", "", text)  # drop issue/PR refs
        text = " ".join(text.split())
        # keep the headline clause, drop the prose tail
        text = re.split(r"(?<=[.!?]) ", text)[0].rstrip(".")
        if len(text) > 90:
            text = text[:89].rstrip() + "…"
        out.append(text)
    return out


def effective_weight(tweet: str) -> int:
    # every URL counts as URL_WEIGHT regardless of its real length
    stripped, n = re.subn(r"https?://\S+", "", tweet)
    return weight(stripped) + n * URL_WEIGHT


def compose(tag: str, body: str) -> str:
    head = f"agent-walker {tag}"
    url = f"https://github.com/miiiiiiich/agent-walker/releases/tag/{tag}"
    budget = MAX_WEIGHT - weight(head) - 2 - (URL_WEIGHT + 2)
    lines = []
    for b in bullets_from(body):
        line = f"・{b}"
        w = weight(line) + 1
        if w > budget:
            if not lines and budget > 40:
                while weight(line) + 1 > budget:
                    line = line[:-8] + "…"
                lines.append(line)
            break
        lines.append(line)
        budget -= w

    def build(ls: list[str]) -> str:
        parts = [head] + ([""] + ls if ls else []) + ["", url]
        return "\n".join(parts)

    tweet = build(lines)
    while lines and effective_weight(tweet) > MAX_WEIGHT:  # belt and braces
        lines.pop()
        tweet = build(lines)
    if effective_weight(tweet) > MAX_WEIGHT:
        sys.exit(f"tweet still overweight ({effective_weight(tweet)}) with no bullets left")
    return tweet


def main() -> None:
    with open("release.json") as f:
        release = json.load(f)
    tweet = compose(release["tag_name"], release.get("body") or "")
    print(tweet)
    print(f"--- effective weight: {effective_weight(tweet)}/280", file=sys.stderr)

    if os.environ.get("DRY_RUN", "").lower() == "true":
        print("dry run — not posting", file=sys.stderr)
        return

    keys = ["X_API_KEY", "X_API_SECRET", "X_ACCESS_TOKEN", "X_ACCESS_TOKEN_SECRET"]
    missing = [k for k in keys if not os.environ.get(k)]
    if missing:
        sys.exit(f"missing secrets: {', '.join(missing)}")

    from requests_oauthlib import OAuth1Session

    session = OAuth1Session(
        os.environ["X_API_KEY"],
        client_secret=os.environ["X_API_SECRET"],
        resource_owner_key=os.environ["X_ACCESS_TOKEN"],
        resource_owner_secret=os.environ["X_ACCESS_TOKEN_SECRET"],
    )
    try:
        resp = session.post("https://api.x.com/2/tweets", json={"text": tweet}, timeout=30)
    except Exception as e:  # noqa: BLE001 — network stall must not hang the job
        sys.exit(f"post failed: {e}")
    if resp.status_code != 201:
        sys.exit(f"post failed: {resp.status_code} {resp.text}")
    print(f"posted: {resp.json()['data']['id']}", file=sys.stderr)


if __name__ == "__main__":
    main()
