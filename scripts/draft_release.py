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
    package, _, version = tag.rpartition("/")
    if package and package != "agent-walker":
        sys.exit(f"tag {tag} names a package this workspace does not ship")
    return version.lstrip("v")


def usage(version: str) -> str:
    if "-" in version.split("+", 1)[0]:
        # prereleases are not published to npm (dist skips publish-npm),
        # so unversioned npx/bunx would silently run the latest stable
        return (
            "This is a prerelease — it is not published to npm. "
            "Download the platform binary from the assets below."
        )
    return """No install — `npx` / `bunx` fetch the prebuilt binary for your platform:

```sh
npx agent-walker
# or
bunx agent-walker
```"""


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

{usage(version)}

Prebuilt binaries are attached below, each carrying a [GitHub Artifact Attestation](https://github.com/{REPO}/attestations) — verify with `gh attestation verify <file> --repo {REPO}`.
"""
    print(body)


if __name__ == "__main__":
    main()
