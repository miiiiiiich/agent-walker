import pytest

from draft_release import changelog_section, usage, version_from

CHANGELOG = """# Changelog

## [Unreleased]

### Added

- something in flight

## [1.2.0] - 2026-08-01

### Added

- feature one, wrapped across
  two lines (#42)

[Migration guide](https://example.com/migrate) for the breaking parts.

### Fixed

- a bug

## [1.1.0] - 2026-07-01

### Changed

- older entry

[1.2.0]: https://example.com/compare/v1.1.0...v1.2.0
[1.1.0]: https://example.com/compare/v1.0.0...v1.1.0
"""


def test_extracts_date_and_section():
    date, section = changelog_section("1.2.0", CHANGELOG)
    assert date == "2026-08-01"
    assert "feature one" in section
    assert "a bug" in section


def test_stops_at_next_version():
    _, section = changelog_section("1.2.0", CHANGELOG)
    assert "older entry" not in section
    assert "something in flight" not in section


def test_last_section_stops_at_link_definitions():
    _, section = changelog_section("1.1.0", CHANGELOG)
    assert "older entry" in section
    assert "example.com" not in section


def test_missing_version_exits():
    with pytest.raises(SystemExit) as e:
        changelog_section("9.9.9", CHANGELOG)
    assert "9.9.9" in str(e.value)


def test_version_from_tag_formats():
    assert version_from("v0.14.0") == "0.14.0"
    assert version_from("0.14.0") == "0.14.0"
    assert version_from("agent-walker/0.14.0") == "0.14.0"
    assert version_from("agent-walker/v0.14.0") == "0.14.0"


def test_version_from_arbitrary_prefixes():
    # dist accepts any prefix (verified against `dist plan`)
    assert version_from("releases/v0.14.0") == "0.14.0"
    assert version_from("other/0.14.0") == "0.14.0"


def test_inline_links_do_not_truncate_section():
    _, section = changelog_section("1.2.0", CHANGELOG)
    assert "Migration guide" in section
    assert "a bug" in section


def test_usage_stable_vs_prerelease():
    assert "npx agent-walker" in usage("0.14.0")
    assert "npx" not in usage("0.14.0-rc.1")
    assert "prerelease" in usage("0.14.0-rc.1")
    assert "npx agent-walker" in usage("0.14.0+build5")
