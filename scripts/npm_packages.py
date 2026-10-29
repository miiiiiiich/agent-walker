"""Build a release's npm packages from dist's platform archives.

Each platform's binary ships in its own package, and `agent-walker` lists
them all as optional dependencies; npm installs the one whose os, cpu and
libc match. Nothing is fetched at install time, so the binary is covered by
the registry's integrity check and the package provenance, which a script
downloading it from the GitHub Release never was.

Usage: npm_packages.py <version> <archive dir> <out dir>

Prints the package directories, absolute, in publish order: the platform
packages first, `agent-walker` last.
"""

import json
import shutil
import sys
import tarfile
import tomllib
import zipfile
from dataclasses import dataclass
from pathlib import Path

CARGO_TOML = Path(__file__).resolve().parents[1] / "Cargo.toml"
LAUNCHER = Path(__file__).resolve().parent / "npm" / "launcher.js"

NAME = "agent-walker"
SCOPE = "@agent-walker"
# both launch the one binary each platform package carries
COMMANDS = ("agent-walker", "agw")
LICENSES = ("LICENSE-MIT", "LICENSE-APACHE")
NOTICES = "THIRD_PARTY_NOTICES"


@dataclass(frozen=True)
class Platform:
    triple: str
    # npm's names, as Node reports them in process.platform / process.arch;
    # the launcher derives the package name from them
    os: str
    cpu: str
    libc: str | None = None

    @property
    def suffix(self) -> str:
        return f"{self.os}-{self.cpu}"

    @property
    def package(self) -> str:
        return f"{SCOPE}/{self.suffix}"

    @property
    def executable(self) -> str:
        return NAME + (".exe" if self.os == "win32" else "")

    @property
    def archive(self) -> str:
        return f"{NAME}-{self.triple}" + (".zip" if self.os == "win32" else ".tar.xz")


# dist-workspace.toml's targets
PLATFORMS = (
    Platform("aarch64-apple-darwin", "darwin", "arm64"),
    Platform("x86_64-unknown-linux-gnu", "linux", "x64", "glibc"),
    Platform("aarch64-unknown-linux-gnu", "linux", "arm64", "glibc"),
    Platform("x86_64-pc-windows-msvc", "win32", "x64"),
)


def tar_bytes(t: tarfile.TarFile, member: tarfile.TarInfo) -> bytes:
    content = t.extractfile(member)
    if content is None:
        sys.exit(f"{member.name}: not a regular file")
    return content.read()


def read_members(archive: Path, names: set[str]) -> dict[str, bytes]:
    """The named files, matched by base name: dist's tarballs keep
    everything under one top directory, its zips do not. Nothing else is
    extracted, so a member path never decides where a file is written."""
    found: dict[str, bytes] = {}

    def keep(path: str, read) -> None:
        base = path.rsplit("/", 1)[-1]
        if base not in names:
            return
        if base in found:
            sys.exit(f"{archive.name}: more than one {base}")
        found[base] = read()

    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as z:
            for info in z.infolist():
                if not info.is_dir():
                    keep(info.filename, lambda info=info: z.read(info))
    else:
        with tarfile.open(archive, "r:xz") as t:
            for member in t:
                if member.isfile():
                    keep(member.name, lambda member=member: tar_bytes(t, member))

    missing = sorted(names - found.keys())
    if missing:
        sys.exit(f"{archive.name}: missing {', '.join(missing)}")
    return found


def write_manifest(directory: Path, manifest: dict) -> None:
    (directory / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")


def common_fields(meta: dict, version: str) -> dict:
    return {
        "version": version,
        "license": meta["license"],
        "homepage": meta["homepage"],
        # npm refuses a provenance statement whose source repository differs
        # from this one. The object form is what npm normalizes a bare URL
        # to, with a warning on every publish.
        "repository": {"type": "git", "url": f"git+{meta['repository']}.git"},
    }


def build_platform(
    platform: Platform, files: dict[str, bytes], meta: dict, version: str, out: Path
) -> Path:
    directory = out / platform.suffix
    (directory / "bin").mkdir(parents=True)
    binary = directory / "bin" / platform.executable
    binary.write_bytes(files[platform.executable])
    binary.chmod(0o755)
    for name in (*LICENSES, NOTICES):
        (directory / name).write_bytes(files[name])
    (directory / "README.md").write_text(
        f"The {platform.suffix} binary for [{NAME}](https://www.npmjs.com/package/{NAME}).\n"
        f"Install `{NAME}`; it picks this package on {platform.suffix}.\n"
    )
    manifest = {
        "name": platform.package,
        "description": f"The {platform.suffix} binary for {NAME}",
        **common_fields(meta, version),
        "os": [platform.os],
        "cpu": [platform.cpu],
    }
    if platform.libc:
        manifest["libc"] = [platform.libc]
    # Yarn Plug'n'Play would otherwise run the binary from inside a zip archive
    manifest["preferUnplugged"] = True
    write_manifest(directory, manifest)
    return directory


def build_main(docs: dict[str, bytes], meta: dict, version: str, out: Path) -> Path:
    directory = out / NAME
    (directory / "bin").mkdir(parents=True)
    for command in COMMANDS:
        launcher = directory / "bin" / f"{command}.js"
        shutil.copyfile(LAUNCHER, launcher)
        launcher.chmod(0o755)
    for name, content in docs.items():
        (directory / name).write_bytes(content)
    write_manifest(
        directory,
        {
            "name": NAME,
            "description": meta["description"],
            **common_fields(meta, version),
            "keywords": meta["keywords"] + meta["categories"],
            "bin": {command: f"bin/{command}.js" for command in COMMANDS},
            # exact: a binary from another release would not match this
            # package's provenance
            "optionalDependencies": {p.package: version for p in PLATFORMS},
            "engines": {"node": ">=16"},
        },
    )
    return directory


def build(version: str, archives: Path, out: Path) -> list[Path]:
    meta = tomllib.loads(CARGO_TOML.read_text())["package"]
    if meta["version"] != version:
        sys.exit(f"version {version} does not match Cargo.toml's {meta['version']}")
    if out.exists() and any(out.iterdir()):
        sys.exit(f"{out} is not empty")

    directories = []
    docs: dict[str, bytes] = {}
    for platform in PLATFORMS:
        files = read_members(
            archives / platform.archive,
            {platform.executable, "README.md", *LICENSES, NOTICES},
        )
        directories.append(build_platform(platform, files, meta, version, out))
        # from the first archive: the Windows zip carries CRLF copies
        docs = docs or {name: files[name] for name in ("README.md", *LICENSES)}
    directories.append(build_main(docs, meta, version, out))
    return directories


def main(argv: list[str]) -> None:
    if len(argv) != 3:
        sys.exit(__doc__)
    version, archives, out = argv[0], Path(argv[1]), Path(argv[2])
    # absolute: `npm publish npm/darwin-arm64` reads a relative path without
    # a leading ./ as the GitHub repository npm/darwin-arm64
    for directory in build(version, archives, out):
        print(directory.resolve())


if __name__ == "__main__":
    main(sys.argv[1:])
