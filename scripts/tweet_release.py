import json
import os
import re
import sys

MAX_WEIGHT = 280
URL_WEIGHT = 23  # every URL counts as 23 chars on X


def weight(text: str) -> int:
    # X counts chars above U+10FF as 2
    return sum(2 if ord(c) > 0x10FF else 1 for c in text)


def bullets_from(body: str) -> list[str]:
    body = re.sub(r"^## Release Notes\s*", "", body.strip())
    body = re.split(r"^## ", body, maxsplit=1, flags=re.MULTILINE)[0]
    out = []
    # [preamble, section1, chunk1, section2, chunk2, …]
    parts = re.split(r"^### +(.+)$", body, flags=re.MULTILINE)
    for section, chunk in zip(["", *parts[1::2]], [parts[0], *parts[2::2]]):
        for m in re.finditer(r"^- (.+?)(?=^[^\s]|\Z)", chunk, re.MULTILINE | re.DOTALL):
            text = " ".join(m.group(1).split())
            text = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", text)
            text = re.sub(r"https?://\S+", "", text)
            text = text.replace("`", "").replace("**", "")
            text = re.sub(r"\s*\(#\d+\)", "", text)
            text = " ".join(text.split())
            text = re.split(r"(?<=[.!?]) ", text)[0].rstrip(".")
            out.append(f"{section.strip()}: {text}" if section.strip() else text)
    return out


def effective_weight(tweet: str) -> int:
    stripped, n = re.subn(r"https?://\S+", "", tweet)
    return weight(stripped) + n * URL_WEIGHT


def compose(tag: str, body: str) -> str:
    # package-qualified tags (agent-walker/0.14.0) would double the name
    head = f"agent-walker {tag.rsplit('/', 1)[-1]}"
    url = f"https://github.com/miiiiiiich/agent-walker/releases/tag/{tag}"
    budget = MAX_WEIGHT - weight(head) - 2 - (URL_WEIGHT + 2)
    lines = []
    for b in bullets_from(body):
        line = f"・{b}"
        w = weight(line) + 1
        if w > budget:
            # whole sentences only — a bullet that doesn't fit is skipped,
            # never cut mid-text; the release link carries the rest
            continue
        lines.append(line)
        budget -= w

    def build(ls: list[str]) -> str:
        parts = [head] + ([""] + ls if ls else []) + ["", url]
        return "\n".join(parts)

    tweet = build(lines)
    while lines and effective_weight(tweet) > MAX_WEIGHT:
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
    except Exception as e:  # noqa: BLE001 — any network failure must fail the job, not hang it
        sys.exit(f"post failed: {e}")
    if resp.status_code != 201:
        sys.exit(f"post failed: {resp.status_code} {resp.text}")
    print(f"posted: {resp.json()['data']['id']}", file=sys.stderr)


if __name__ == "__main__":
    main()
