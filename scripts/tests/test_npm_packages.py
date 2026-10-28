import io
import json
import shutil
import subprocess
import tarfile
import zipfile
from pathlib import Path

import pytest

import npm_packages
from npm_packages import PLATFORMS, build

VERSION = "1.2.0"
CARGO_TOML = f"""[package]
name = "agent-walker"
version = "{VERSION}"
description = "Local dashboard"
license = "MIT OR Apache-2.0"
homepage = "https://github.com/miiiiiiich/agent-walker"
repository = "https://github.com/miiiiiiich/agent-walker"
keywords = ["cli", "tui"]
categories = ["command-line-utilities"]
"""
DOCS = ("README.md", "LICENSE-MIT", "LICENSE-APACHE", "THIRD_PARTY_NOTICES")


def tar_xz(path: Path, top: str, files: dict[str, bytes]) -> None:
    with tarfile.open(path, "w:xz") as t:
        for name, content in files.items():
            info = tarfile.TarInfo(f"{top}/{name}" if top else name)
            info.size = len(content)
            t.addfile(info, io.BytesIO(content))


def write_archives(directory: Path, skip: str | None = None) -> None:
    directory.mkdir(exist_ok=True)
    for platform in PLATFORMS:
        files = {name: f"{name} for {platform.triple}".encode() for name in DOCS}
        if platform.executable != skip:
            files[platform.executable] = f"binary {platform.triple}".encode()
        archive = directory / platform.archive
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive, "w") as z:
                for name, content in files.items():
                    z.writestr(name, content)
        else:
            tar_xz(archive, archive.name.removesuffix(".tar.xz"), files)


@pytest.fixture
def cargo(tmp_path, monkeypatch):
    path = tmp_path / "Cargo.toml"
    path.write_text(CARGO_TOML)
    monkeypatch.setattr(npm_packages, "CARGO_TOML", path)


@pytest.fixture
def archives(tmp_path):
    directory = tmp_path / "archives"
    write_archives(directory)
    return directory


def manifest(directory: Path) -> dict:
    return json.loads((directory / "package.json").read_text())


def test_platform_packages_come_first_and_main_pins_them(cargo, archives, tmp_path):
    directories = build(VERSION, archives, tmp_path / "out")

    assert [d.name for d in directories] == [
        "darwin-arm64",
        "linux-x64",
        "linux-arm64",
        "win32-x64",
        "agent-walker",
    ]
    main = manifest(directories[-1])
    assert main["name"] == "agent-walker"
    assert main["version"] == VERSION
    assert main["optionalDependencies"] == {
        "@agent-walker/darwin-arm64": VERSION,
        "@agent-walker/linux-x64": VERSION,
        "@agent-walker/linux-arm64": VERSION,
        "@agent-walker/win32-x64": VERSION,
    }
    assert main["bin"] == {"agent-walker": "bin/agent-walker.js", "agw": "bin/agw.js"}
    assert main["keywords"] == ["cli", "tui", "command-line-utilities"]
    assert "scripts" not in main, "nothing may run at install time"
    for command in ("agent-walker", "agw"):
        launcher = directories[-1] / "bin" / f"{command}.js"
        assert launcher.read_bytes() == npm_packages.LAUNCHER.read_bytes()
        assert launcher.stat().st_mode & 0o111
    # docs come from the first (non-Windows) archive
    readme = (directories[-1] / "README.md").read_bytes()
    assert readme == b"README.md for aarch64-apple-darwin"


def test_platform_packages_carry_one_binary_and_their_npm_platform(cargo, archives, tmp_path):
    directories = build(VERSION, archives, tmp_path / "out")

    by_name = {d.name: d for d in directories}
    linux = by_name["linux-x64"]
    assert manifest(linux) | {"description": ""} == {
        "name": "@agent-walker/linux-x64",
        "description": "",
        "version": VERSION,
        "license": "MIT OR Apache-2.0",
        "homepage": "https://github.com/miiiiiiich/agent-walker",
        "repository": "https://github.com/miiiiiiich/agent-walker",
        "os": ["linux"],
        "cpu": ["x64"],
        "libc": ["glibc"],
        "preferUnplugged": True,
    }
    binary = linux / "bin" / "agent-walker"
    assert binary.read_bytes() == b"binary x86_64-unknown-linux-gnu"
    assert binary.stat().st_mode & 0o777 == 0o755
    assert (linux / "THIRD_PARTY_NOTICES").exists()

    darwin = manifest(by_name["darwin-arm64"])
    assert (darwin["os"], darwin["cpu"]) == (["darwin"], ["arm64"])
    assert "libc" not in darwin
    windows = by_name["win32-x64"]
    assert (windows / "bin" / "agent-walker.exe").read_bytes() == b"binary x86_64-pc-windows-msvc"
    assert sorted(p.name for p in (windows / "bin").iterdir()) == ["agent-walker.exe"]


def test_printed_directories_are_absolute(cargo, archives, tmp_path, monkeypatch, capsys):
    monkeypatch.chdir(tmp_path)
    npm_packages.main([VERSION, "archives", "npm"])

    lines = capsys.readouterr().out.splitlines()
    assert lines[0] == str(tmp_path.resolve() / "npm" / "darwin-arm64")
    assert all(Path(line).is_absolute() for line in lines)
    assert lines[-1].endswith("/agent-walker")


def test_version_must_match_cargo_toml(cargo, archives, tmp_path):
    with pytest.raises(SystemExit, match="does not match"):
        build("9.9.9", archives, tmp_path / "out")


def test_an_archive_without_the_binary_stops_the_build(cargo, tmp_path):
    write_archives(tmp_path / "archives", skip="agent-walker")
    with pytest.raises(SystemExit, match="missing agent-walker$"):
        build(VERSION, tmp_path / "archives", tmp_path / "out")


def test_a_duplicated_member_stops_the_build(cargo, archives, tmp_path):
    archive = archives / PLATFORMS[0].archive
    tar_xz(archive, "", {"a/agent-walker": b"one", "b/agent-walker": b"two", "README.md": b""})
    with pytest.raises(SystemExit, match="more than one agent-walker"):
        build(VERSION, archives, tmp_path / "out")


def test_member_paths_never_choose_where_files_land(cargo, archives, tmp_path):
    archive = archives / PLATFORMS[0].archive
    files = {f"../../escape/{name}": b"x" for name in (*DOCS, "agent-walker")}
    tar_xz(archive, "", files)

    build(VERSION, archives, tmp_path / "out")

    assert not (tmp_path.parent / "escape").exists()
    assert (tmp_path / "out" / "darwin-arm64" / "bin" / "agent-walker").read_bytes() == b"x"


def test_a_used_output_directory_is_refused(cargo, archives, tmp_path):
    out = tmp_path / "out"
    out.mkdir()
    (out / "left-over").touch()
    with pytest.raises(SystemExit, match="not empty"):
        build(VERSION, archives, out)


NODE = shutil.which("node") or ""


def install_layout(cargo_archives: Path, tmp_path: Path, with_binary: bool) -> Path:
    """A node_modules tree as npm lays it out: agent-walker plus the
    platform package for this machine, holding a fake binary."""
    directories = build(VERSION, cargo_archives, tmp_path / "out")
    modules = tmp_path / "project" / "node_modules"
    shutil.copytree(directories[-1], modules / "agent-walker")
    target = subprocess.run(  # noqa: S603 - fixed argv, trusted node
        [NODE, "-p", "`${process.platform}-${process.arch}`"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    if with_binary:
        binary = modules / "@agent-walker" / target / "bin" / "agent-walker"
        binary.parent.mkdir(parents=True)
        binary.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\nexit 3\n')
        binary.chmod(0o755)
    return modules / "agent-walker" / "bin"


@pytest.mark.skipif(not NODE or not Path("/bin/sh").exists(), reason="needs node and sh")
def test_launcher_runs_the_platform_binary_with_args_and_exit_code(cargo, archives, tmp_path):
    bin_dir = install_layout(archives, tmp_path, with_binary=True)

    result = subprocess.run(  # noqa: S603 - fixed argv, trusted node
        [NODE, str(bin_dir / "agw.js"), "--json", "a b"],
        capture_output=True,
        text=True,
        check=False,
    )

    assert result.returncode == 3
    assert result.stdout == "--json\na b\n"


@pytest.mark.skipif(not NODE, reason="needs node")
def test_launcher_without_the_platform_package_explains_why(cargo, archives, tmp_path):
    bin_dir = install_layout(archives, tmp_path, with_binary=False)

    result = subprocess.run(  # noqa: S603 - fixed argv, trusted node
        [NODE, str(bin_dir / "agent-walker.js")],
        capture_output=True,
        text=True,
        check=False,
    )

    assert result.returncode == 1
    assert "agent-walker: no binary for" in result.stderr
    assert "darwin-arm64, linux-x64, linux-arm64, win32-x64" in result.stderr
