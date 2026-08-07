from tweet_release import MAX_WEIGHT, bullets_from, compose, effective_weight, weight

BODY = """## Release Notes

### Added

- `SKILLS` (Claude tab): **token volume** per skill, read from the
  [attribution field](https://example.com/docs) recent versions write (#30). More prose
  in a second sentence.
- Short one.

## Install agent-walker 1.2.0

- this bullet must not appear
"""


def test_weight_counts_cjk_as_two():
    assert weight("abc") == 3
    assert weight("あ") == 2


def test_bullets_strip_markup_and_scope():
    bullets = bullets_from(BODY)
    assert len(bullets) == 2
    first = bullets[0]
    assert "**" not in first and "`" not in first
    assert "https://" not in first
    assert "(#30)" not in first
    assert "second sentence" not in first  # first clause only
    assert "must not appear" not in " ".join(bullets)


def test_bullets_cap_length():
    long_bullet = "- " + "word " * 40
    bullets = bullets_from("## Release Notes\n\n" + long_bullet)
    assert len(bullets[0]) <= 90
    assert bullets[0].endswith("…")


def test_effective_weight_counts_urls_as_23():
    assert effective_weight("https://example.com/a/very/long/path") == 23
    assert effective_weight("ab https://example.com/x") == 3 + 23


def test_compose_fits_and_contains_anchor():
    tweet = compose("v1.2.0", BODY)
    assert "agent-walker v1.2.0" in tweet
    assert "releases/tag/v1.2.0" in tweet
    assert effective_weight(tweet) <= MAX_WEIGHT


def test_compose_many_bullets_still_fits():
    body = "## Release Notes\n\n" + "\n".join(
        f"- feature number {i} with some words" for i in range(20)
    )
    tweet = compose("v1.2.0", body)
    assert effective_weight(tweet) <= MAX_WEIGHT


def test_compose_without_bullets():
    tweet = compose("v1.2.0", "## Release Notes\n\nno bullets here")
    assert effective_weight(tweet) <= MAX_WEIGHT
    assert "releases/tag/v1.2.0" in tweet
