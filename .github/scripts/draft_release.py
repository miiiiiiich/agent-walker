#!/usr/bin/env python3
"""Render the draft GitHub Release title/body from CHANGELOG.md.

Usage: draft_release.py <tag>            -> body on stdout
       draft_release.py <tag> --title    -> title on stdout

Exits non-zero if CHANGELOG.md has no section for the tag's version — by
design: a release without release notes should fail loudly before dist
builds anything.
"""

import re
import sys

REPO = "miiiiiiich/agent-walker"


def changelog_section(version: str, changelog: str) -> tuple[str, str]:
    """Return (date, section body) for the version."""
    pattern = re.compile(
        r"^## \[" + re.escape(version) + r"\] - (\S+)\s*\n(.*?)(?=^## \[|^\[|\Z)",
        re.M | re.S,
    )
    m = pattern.search(changelog)
    if not m:
        sys.exit(f"CHANGELOG.md has no section for [{version}] — write it before tagging")
    return m.group(1), m.group(2).strip()


def main() -> None:
    tag = sys.argv[1]
    version = tag.lstrip("v")
    with open("CHANGELOG.md") as f:
        date, section = changelog_section(version, f.read())

    if "--title" in sys.argv[2:]:
        print(f"{version} - {date}")
        return

    body = f"""## Release Notes

{section}

## Usage

No install — `npx` / `bunx` fetch the prebuilt binary for your platform:

```sh
npx agent-walker
# or
bunx agent-walker
```

Prebuilt binaries are attached below. Every artifact carries a [GitHub Artifact Attestation](https://github.com/{REPO}/attestations) — verify with `gh attestation verify <file> --repo {REPO}`.
"""
    print(body)


if __name__ == "__main__":
    main()
