import pytest

from draft_release import changelog_section

CHANGELOG = """# Changelog

## [Unreleased]

### Added

- something in flight

## [1.2.0] - 2026-08-01

### Added

- feature one, wrapped across
  two lines (#42)

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
