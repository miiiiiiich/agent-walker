import re
import sys
from pathlib import Path

REPO = "miiiiiiich/agent-walker"
CHANGELOG = Path(__file__).resolve().parents[1] / "CHANGELOG.md"


def changelog_section(version: str, changelog: str) -> tuple[str, str]:
    pattern = re.compile(
        r"^## \[" + re.escape(version) + r"\] - (\S+)\s*\n(.*?)(?=^## \[|^\[|\Z)",
        re.MULTILINE | re.DOTALL,
    )
    m = pattern.search(changelog)
    if not m:
        sys.exit(f"CHANGELOG.md has no section for [{version}] — write it before tagging")
    return m.group(1), m.group(2).strip()


def version_from(tag: str) -> str:
    # dist also fires on package-qualified tags like "agent-walker/0.14.0"
    return tag.rsplit("/", 1)[-1].lstrip("v")


def main() -> None:
    tag = sys.argv[1]
    version = version_from(tag)
    date, section = changelog_section(version, CHANGELOG.read_text())

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
