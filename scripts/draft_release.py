import re
import sys
from pathlib import Path

REPO = "miiiiiiich/agent-walker"
CHANGELOG = Path(__file__).resolve().parents[1] / "CHANGELOG.md"
CARGO_TOML = Path(__file__).resolve().parents[1] / "Cargo.toml"


def changelog_section(version: str, changelog: str) -> tuple[str, str]:
    # a section ends at the next version heading or at the footer's
    # link-reference definitions — not at any body line starting with "["
    pattern = re.compile(
        r"^## \[" + re.escape(version) + r"\] - (\S+)\s*\n(.*?)(?=^## \[|^\[[^\]]+\]:|\Z)",
        re.MULTILINE | re.DOTALL,
    )
    m = pattern.search(changelog)
    if not m:
        sys.exit(f"CHANGELOG.md has no section for [{version}] — write it before tagging")
    return m.group(1), m.group(2).strip()


def version_from(tag: str) -> str:
    # dist accepts arbitrary tag prefixes (agent-walker/0.14.0,
    # releases/v1.0.0 — verified against `dist plan`); only the version
    # component matters here
    return tag.rsplit("/", 1)[-1].lstrip("v")


def check_version_matches(version: str, cargo_toml: str) -> None:
    # mirror dist plan's tag-version validation so a tag plan would
    # reject can never leave a dangling draft behind
    m = re.search(r'^version\s*=\s*"([^"]+)"', cargo_toml, re.MULTILINE)
    if not m:
        sys.exit("Cargo.toml has no package version")
    if version != m.group(1):
        sys.exit(
            f"tag version {version} does not match Cargo.toml version "
            f"{m.group(1)} — did the release PR merge?"
        )


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
    check_version_matches(version, CARGO_TOML.read_text())
    _date, section = changelog_section(version, CHANGELOG.read_text())

    if "--title" in sys.argv[2:]:
        # version only — GitHub renders the release date itself
        print(f"v{version}")
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
