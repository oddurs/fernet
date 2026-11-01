//! Resolving time specs to instants or version selectors.
//!
//! Every instant is UTC nanoseconds (`i64`). Relative and named forms use the
//! user's time zone; absolute forms without an offset are local time in that
//! zone. Ambiguous local times use jiff's "compatible" rule: a time in a DST
//! gap moves forward by the gap; a time in a fold picks the earlier instant.

use std::collections::BTreeMap;

use fernet_fs_path::{AbsoluteTime, DayName, Offset, TimeSpec};
use jiff::Timestamp;
use jiff::civil::{Date, DateTime, Weekday};
use jiff::tz::{self, TimeZone};

use crate::{Error, Result};

/// What a time spec selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The latest version at or before this instant (UTC nanoseconds).
    At(i64),
    /// N versions before the current one.
    Back(u32),
    /// The earliest kept version.
    First,
    /// The list of versions.
    Listing,
    /// A listing name: the first version within this UTC second (its start,
    /// in nanoseconds), or else the latest version before the second ends.
    Second(i64),
}

/// Nanoseconds since the Unix epoch, saturating at the `i64` range.
#[must_use]
pub fn nanos(ts: Timestamp) -> i64 {
    i64::try_from(ts.as_nanosecond()).unwrap_or(if ts.as_nanosecond() < 0 {
        i64::MIN
    } else {
        i64::MAX
    })
}

/// The timestamp for `nanos`, clamped to jiff's range.
#[must_use]
pub fn timestamp(nanos: i64) -> Timestamp {
    Timestamp::from_nanosecond(i128::from(nanos)).unwrap_or(if nanos < 0 {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    })
}

/// Resolves `spec` against `now` in `tz`. Marks come from the store.
///
/// # Errors
///
/// [`Error::NoMark`] for an unknown mark.
pub fn resolve(
    spec: &TimeSpec,
    now: Timestamp,
    tz: &TimeZone,
    marks: &BTreeMap<String, i64>,
) -> Result<Target> {
    Ok(match spec {
        TimeSpec::Listing => Target::Listing,
        TimeSpec::Back(n) => Target::Back(*n),
        TimeSpec::First => Target::First,
        TimeSpec::Now => Target::At(nanos(now)),
        // The version a listing shows under this name.
        TimeSpec::Stamp(st) => {
            let a = AbsoluteTime {
                date: st.date,
                time: Some(st.time),
                offset: Some(Offset::Utc),
            };
            Target::Second(absolute(&a, tz))
        }
        TimeSpec::Mark(m) => Target::At(*marks.get(m.as_str()).ok_or_else(|| Error::NoMark {
            name: m.to_string(),
        })?),
        TimeSpec::Relative { .. } => {
            let secs = spec.duration().map_or(u64::MAX, |d| d.as_secs());
            let back = i128::from(secs) * 1_000_000_000;
            Target::At(i64::try_from(now.as_nanosecond() - back).unwrap_or(i64::MIN))
        }
        TimeSpec::NamedDay(day) => {
            let today = now.to_zoned(tz.clone()).date();
            let date = if *day == DayName::Yesterday {
                today.yesterday().unwrap_or(today)
            } else {
                let target = weekday(*day);
                let offset =
                    today.weekday().to_monday_zero_offset() - target.to_monday_zero_offset();
                // The most recent past one: today's weekday means a week ago.
                let back = match i64::from(offset).rem_euclid(7) {
                    0 => 7,
                    n => n,
                };
                today
                    .checked_sub(jiff::Span::new().days(back))
                    .unwrap_or(today)
            };
            Target::At(start_of_day(date, tz))
        }
        // With seconds, the whole second: `fernet log` shows times truncated
        // to the second, and a time copied from it must select that version.
        TimeSpec::Absolute(a) if a.time.is_some_and(|t| t.second.is_some()) => {
            Target::At(absolute(a, tz).saturating_add(999_999_999))
        }
        TimeSpec::Absolute(a) => Target::At(absolute(a, tz)),
    })
}

fn weekday(d: DayName) -> Weekday {
    match d {
        DayName::Monday | DayName::Yesterday => Weekday::Monday,
        DayName::Tuesday => Weekday::Tuesday,
        DayName::Wednesday => Weekday::Wednesday,
        DayName::Thursday => Weekday::Thursday,
        DayName::Friday => Weekday::Friday,
        DayName::Saturday => Weekday::Saturday,
        DayName::Sunday => Weekday::Sunday,
    }
}

/// The first instant of `date` in `tz`.
fn start_of_day(date: Date, tz: &TimeZone) -> i64 {
    date.to_zoned(tz.clone())
        .map_or(beyond(date.year()), |z| nanos(z.timestamp()))
}

/// A valid date outside jiff's range (the last hours of 9999 in some zones,
/// or before year 1): saturate toward the side it lies on.
fn beyond(year: i16) -> i64 {
    if year > 1970 { i64::MAX } else { i64::MIN }
}

fn absolute(a: &AbsoluteTime, tz: &TimeZone) -> i64 {
    let Ok(date) = Date::new(
        i16::try_from(a.date.year).unwrap_or(i16::MAX),
        a.date.month.cast_signed(),
        a.date.day.cast_signed(),
    ) else {
        return i64::MIN;
    };
    let Some(t) = a.time else {
        return start_of_day(date, tz);
    };
    let second = t.second.unwrap_or(0);
    let Ok(dt) = DateTime::new(
        date.year(),
        date.month(),
        date.day(),
        t.hour.cast_signed(),
        t.minute.cast_signed(),
        second.cast_signed(),
        0,
    ) else {
        return i64::MIN;
    };
    let zone = match a.offset {
        None => tz.clone(),
        Some(Offset::Utc) => TimeZone::UTC,
        Some(o @ Offset::Fixed { .. }) => match tz::Offset::from_seconds(o.seconds()) {
            Ok(off) => TimeZone::fixed(off),
            Err(_) => return i64::MIN,
        },
    };
    dt.to_zoned(zone)
        .map_or(beyond(dt.year()), |z| nanos(z.timestamp()))
}

/// The ISO 8601 basic UTC name of an instant: `20261030T164512Z`.
#[must_use]
pub fn basic_name(nanos: i64) -> String {
    timestamp(nanos).strftime("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ny() -> TimeZone {
        TimeZone::get("America/New_York").unwrap()
    }

    fn at(spec: &str, now: &str) -> i64 {
        let spec = TimeSpec::parse(spec).unwrap();
        let now: Timestamp = now.parse().unwrap();
        let marks = BTreeMap::from([("release".to_owned(), 42)]);
        match resolve(&spec, now, &ny(), &marks).unwrap() {
            Target::At(t) => t,
            other => panic!("{other:?}"),
        }
    }

    fn ts(s: &str) -> i64 {
        nanos(s.parse().unwrap())
    }

    #[test]
    fn relative() {
        let now = "2026-03-08T12:00:00Z";
        assert_eq!(at("-90s", now), ts("2026-03-08T11:58:30Z"));
        assert_eq!(at("-2h", now), ts("2026-03-08T10:00:00Z"));
        // `d` is 24 hours, also across the spring-forward boundary.
        assert_eq!(at("-1d", now), ts("2026-03-07T12:00:00Z"));
        assert_eq!(at("-2w", now), ts("2026-02-22T12:00:00Z"));
        assert_eq!(at("-0s", now), ts(now));
    }

    #[test]
    fn named_days_across_dst() {
        // Monday 2026-03-09, 10:00 EDT. DST began Sunday 2026-03-08 at 02:00.
        let now = "2026-03-09T14:00:00Z";
        // Yesterday's midnight was still EST (-05:00).
        assert_eq!(at("yesterday", now), ts("2026-03-08T05:00:00Z"));
        assert_eq!(at("sunday", now), ts("2026-03-08T05:00:00Z"));
        // `@monday` on a Monday is a week ago.
        assert_eq!(at("monday", now), ts("2026-03-02T05:00:00Z"));
        assert_eq!(at("saturday", now), ts("2026-03-07T05:00:00Z"));
        assert_eq!(at("tuesday", now), ts("2026-03-03T05:00:00Z"));
        // Local midnight decides "today": 2026-03-10T03:00Z is still the 9th in New York.
        assert_eq!(
            at("yesterday", "2026-03-10T03:00:00Z"),
            ts("2026-03-08T05:00:00Z")
        );
    }

    #[test]
    fn absolute_local_and_offsets() {
        let now = "2026-10-30T16:00:00Z";
        assert_eq!(at("2026-10-01", now), ts("2026-10-01T04:00:00Z"));
        assert_eq!(at("2026-10-01T14:30", now), ts("2026-10-01T18:30:00Z"));
        // With seconds: the whole second.
        assert_eq!(
            at("2026-10-01T14:30:05Z", now),
            ts("2026-10-01T14:30:05.999999999Z")
        );
        assert_eq!(
            at("2026-10-01T14:30+02:00", now),
            ts("2026-10-01T12:30:00Z")
        );
        assert_eq!(
            at("2026-10-01T14:30-00:00", now),
            ts("2026-10-01T14:30:00Z")
        );
        // Spring-forward gap: 02:30 doesn't exist; compatible moves it to 03:30 EDT.
        assert_eq!(at("2026-03-08T02:30", now), ts("2026-03-08T07:30:00Z"));
        // Fall-back fold: 01:30 happens twice; compatible picks the earlier (EDT).
        assert_eq!(at("2026-11-01T01:30", now), ts("2026-11-01T05:30:00Z"));
        assert_eq!(at("2026-11-01", now), ts("2026-11-01T04:00:00Z"));
        assert_eq!(at("2026-11-02", now), ts("2026-11-02T05:00:00Z"));
    }

    #[test]
    fn selectors_and_marks() {
        let now: Timestamp = "2026-10-30T16:00:00Z".parse().unwrap();
        let marks = BTreeMap::from([("release".to_owned(), 42)]);
        let r = |s: &str| resolve(&TimeSpec::parse(s).unwrap(), now, &ny(), &marks);
        assert_eq!(r("~3").unwrap(), Target::Back(3));
        assert_eq!(r("first").unwrap(), Target::First);
        assert_eq!(r("now").unwrap(), Target::At(nanos(now)));
        assert_eq!(
            r("20261030T120000Z").unwrap(),
            Target::Second(ts("2026-10-30T12:00:00Z"))
        );
        // The end of jiff's range is still in the future, not the past.
        for late in ["9999-12-31", "9999-12-31T23:59:59Z", "99991231T235959Z"] {
            let t = r(late).unwrap();
            assert!(
                matches!(t, Target::At(n) | Target::Second(n) if n > nanos(now)),
                "{late}: {t:?}"
            );
        }
        assert_eq!(r("").unwrap(), Target::Listing);
        assert_eq!(r("release").unwrap(), Target::At(42));
        assert!(matches!(r("nope"), Err(Error::NoMark { .. })));
    }

    #[test]
    fn basic_names() {
        assert_eq!(basic_name(ts("2026-10-30T16:45:12.5Z")), "20261030T164512Z");
    }
}
