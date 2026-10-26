//! Timestamps from agent logs and stores are untrusted. `time` holds dates up
//! to year ±9999, but shifting one near that edge into another offset panics
//! (`to_offset`): a single row dated `9999-12-31T23:00:00-05:00` aborted every
//! run. Parsing keeps only plausible instants, so such a row reads like an
//! unparseable one, and conversions to the local offset are checked.

use std::ops::Range;

use time::format_description::well_known::Rfc3339;
use time::macros::datetime;
use time::{Date, OffsetDateTime, UtcOffset};

/// Unix seconds a row may carry. The floor is the epoch, so a zeroed field
/// still dates as it always has; the ceiling leaves every offset shift and
/// day step of an accepted instant far inside the representable range.
const PLAUSIBLE_UNIX_SECONDS: Range<i64> = datetime!(1970-01-01 0:00 UTC).unix_timestamp()
    ..datetime!(3000-01-01 0:00 UTC).unix_timestamp();

pub(crate) fn plausible(timestamp: OffsetDateTime) -> Option<OffsetDateTime> {
    PLAUSIBLE_UNIX_SECONDS
        .contains(&timestamp.unix_timestamp())
        .then_some(timestamp)
}

pub(crate) fn parse_rfc3339(raw: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw, &Rfc3339)
        .ok()
        .and_then(plausible)
}

pub(crate) fn from_unix_seconds(seconds: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .and_then(plausible)
}

pub(crate) fn from_unix_millis(millis: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .ok()
        .and_then(plausible)
}

/// `None` only for a timestamp that never went through `plausible` (a parse
/// cache written by another build): skipped like an undated row, not a panic.
pub(crate) fn to_local(timestamp: OffsetDateTime, offset: UtcOffset) -> Option<OffsetDateTime> {
    timestamp.checked_to_offset(offset)
}

/// The current instant in `offset`; the clock is always far inside the range.
pub(crate) fn now_local(offset: UtcOffset) -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    to_local(now, offset).unwrap_or(now)
}

pub(crate) fn local_date(timestamp: OffsetDateTime, offset: UtcOffset) -> Option<Date> {
    to_local(timestamp, offset).map(OffsetDateTime::date)
}

#[cfg(test)]
mod tests {
    use time::macros::offset;

    use super::*;

    #[test]
    fn rejects_instants_that_would_overflow_in_another_offset() {
        for raw in [
            "9999-12-31T23:00:00Z",
            "9999-12-31T23:00:00-05:00",
            "3000-01-01T00:00:00Z",
            "1969-12-31T23:59:59Z",
            "-0001-01-01T00:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(raw), None, "{raw}");
        }
        assert_eq!(from_unix_seconds(253_402_290_000), None);
        assert_eq!(from_unix_millis(253_402_290_000_000), None);
        assert_eq!(from_unix_millis(-1), None);
    }

    #[test]
    fn keeps_ordinary_instants_including_the_epoch() {
        let at = parse_rfc3339("2026-10-25T00:00:00-05:00").expect("ordinary date parses");
        assert_eq!(at.offset(), offset!(-5));
        assert_eq!(from_unix_seconds(0), Some(OffsetDateTime::UNIX_EPOCH));
        assert_eq!(
            from_unix_millis(1_761_350_400_123).map(OffsetDateTime::millisecond),
            Some(123)
        );
        assert!(parse_rfc3339("2999-12-31T23:59:59Z").is_some());
    }

    #[test]
    fn local_conversion_of_an_edge_instant_is_none_not_a_panic() {
        let edge = OffsetDateTime::parse("9999-12-31T23:00:00Z", &Rfc3339).expect("parses");

        assert_eq!(local_date(edge, offset!(+9)), None);
        assert_eq!(
            local_date(edge, UtcOffset::UTC),
            Some(datetime!(9999-12-31 0:00 UTC).date())
        );
    }
}
