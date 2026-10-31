//! Time specs: the text after `@` in a time lens.
//!
//! Only canonical spellings are recognised. `-2h` is a spec; `-02h`, `-2H` and
//! `+2h` are not. That makes printing an exact inverse of parsing, which the
//! resolver relies on to apply "real names win" (see `docs/GRAMMAR.md`).

use std::fmt;
use std::time::Duration;

/// A parsed, unresolved time spec.
///
/// Resolution to an instant needs a clock, a time zone and (for counts, marks
/// and `first`) the store, so it lives in `fernet-fs-core`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TimeSpec {
    /// `-90s`, `-2h`, `-3d`, `-2w`: now minus a duration. The written unit is
    /// kept so the spec prints back exactly as written.
    Relative {
        /// The number before the unit.
        amount: u64,
        /// The unit letter.
        unit: Unit,
    },
    /// `yesterday`, `monday` … `sunday`: local midnight at the start of that day.
    NamedDay(DayName),
    /// `2026-10-01`, `2026-10-01T14:30`, `2026-10-01T14:30:05Z`, with offsets.
    Absolute(AbsoluteTime),
    /// `~N`: N versions before the current one (N ≥ 1).
    Back(u32),
    /// `first`: the earliest kept version.
    First,
    /// `now`: the present (the latest version).
    Now,
    /// `20261030T164512Z`: a UTC second in ISO 8601 basic format, as version
    /// listings name versions. Covers the whole second.
    Stamp(Stamp),
    /// A named mark (a tephra layer).
    Mark(MarkName),
    /// The empty spec: a directory listing every kept version.
    Listing,
}

/// Units for [`TimeSpec::Relative`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unit {
    /// `s`
    Seconds,
    /// `m`
    Minutes,
    /// `h`
    Hours,
    /// `d` (24 hours; calendar-aware days are not a relative form)
    Days,
    /// `w` (7 × 24 hours)
    Weeks,
}

impl Unit {
    /// Seconds in one of this unit.
    #[must_use]
    pub const fn seconds(self) -> u64 {
        match self {
            Self::Seconds => 1,
            Self::Minutes => 60,
            Self::Hours => 3_600,
            Self::Days => 86_400,
            Self::Weeks => 604_800,
        }
    }

    const fn letter(self) -> char {
        match self {
            Self::Seconds => 's',
            Self::Minutes => 'm',
            Self::Hours => 'h',
            Self::Days => 'd',
            Self::Weeks => 'w',
        }
    }

    const fn from_letter(c: u8) -> Option<Self> {
        Some(match c {
            b's' => Self::Seconds,
            b'm' => Self::Minutes,
            b'h' => Self::Hours,
            b'd' => Self::Days,
            b'w' => Self::Weeks,
            _ => return None,
        })
    }
}

/// Named days for [`TimeSpec::NamedDay`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DayName {
    /// `yesterday`
    Yesterday,
    /// `monday`
    Monday,
    /// `tuesday`
    Tuesday,
    /// `wednesday`
    Wednesday,
    /// `thursday`
    Thursday,
    /// `friday`
    Friday,
    /// `saturday`
    Saturday,
    /// `sunday`
    Sunday,
}

impl DayName {
    /// Every named day, in display order.
    pub const ALL: [Self; 8] = [
        Self::Yesterday,
        Self::Monday,
        Self::Tuesday,
        Self::Wednesday,
        Self::Thursday,
        Self::Friday,
        Self::Saturday,
        Self::Sunday,
    ];

    /// The keyword as written in a path.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Yesterday => "yesterday",
            Self::Monday => "monday",
            Self::Tuesday => "tuesday",
            Self::Wednesday => "wednesday",
            Self::Thursday => "thursday",
            Self::Friday => "friday",
            Self::Saturday => "saturday",
            Self::Sunday => "sunday",
        }
    }
}

/// Words that can never be mark names.
pub const RESERVED_WORDS: [&str; 10] = [
    "yesterday",
    "first",
    "now",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// A validated mark name: `[a-z][a-z0-9-]{0,63}`, not a reserved word.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MarkName(String);

impl MarkName {
    /// Validates `s` as a mark name.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidMarkName`] if `s` doesn't match the pattern or is reserved.
    pub fn new(s: &str) -> Result<Self, InvalidMarkName> {
        let b = s.as_bytes();
        let shape_ok = !b.is_empty()
            && b.len() <= 64
            && b[0].is_ascii_lowercase()
            && b.iter()
                .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
        if shape_ok && !RESERVED_WORDS.contains(&s) {
            Ok(Self(s.to_owned()))
        } else {
            Err(InvalidMarkName)
        }
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MarkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Error for [`MarkName::new`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidMarkName;

impl fmt::Display for InvalidMarkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "mark names are 1-64 characters of a-z, 0-9 and '-', start with a letter, \
             and aren't a reserved word",
        )
    }
}

impl std::error::Error for InvalidMarkName {}

/// An absolute date and optional time, as written. Unresolved: without an
/// offset it is local time in the user's zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AbsoluteTime {
    /// The calendar date.
    pub date: Date,
    /// The time of day, if given.
    pub time: Option<TimeOfDay>,
    /// The UTC offset, if given. Only allowed together with a time.
    pub offset: Option<Offset>,
}

/// A proleptic Gregorian date with a four-digit year.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Date {
    /// 0000–9999.
    pub year: u16,
    /// 1–12.
    pub month: u8,
    /// 1–31, valid for the month and year.
    pub day: u8,
}

impl Date {
    /// Builds a date, checking the calendar (including leap years).
    #[must_use]
    pub fn new(year: u16, month: u8, day: u8) -> Option<Self> {
        (year <= 9999 && (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month))
            .then_some(Self { year, month, day })
    }
}

const fn is_leap(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// A time of day with minute or second precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimeOfDay {
    /// 0–23.
    pub hour: u8,
    /// 0–59.
    pub minute: u8,
    /// 0–59, if written. Leap seconds are not accepted.
    pub second: Option<u8>,
}

/// A UTC offset as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Offset {
    /// `Z`
    Utc,
    /// `+hh:mm` or `-hh:mm`. `-00:00` and `+00:00` are kept distinct so the
    /// spec prints back as written; both mean UTC.
    Fixed {
        /// True for `-`.
        negative: bool,
        /// 0–23.
        hours: u8,
        /// 0–59.
        minutes: u8,
    },
}

impl Offset {
    /// The offset in seconds east of UTC.
    #[must_use]
    pub fn seconds(self) -> i32 {
        match self {
            Self::Utc => 0,
            Self::Fixed {
                negative,
                hours,
                minutes,
            } => {
                let s = i32::from(hours) * 3600 + i32::from(minutes) * 60;
                if negative { -s } else { s }
            }
        }
    }
}

impl TimeSpec {
    /// Parses the text after `@`. Returns `None` for anything that isn't a
    /// canonical spec; the caller then treats the whole component as a name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        if s.is_empty() {
            return Some(Self::Listing);
        }
        if let Some(rest) = s.strip_prefix('-') {
            return parse_relative(rest);
        }
        if let Some(rest) = s.strip_prefix('~') {
            let n: u32 = parse_canonical_uint(rest)?.try_into().ok()?;
            return (n >= 1).then_some(Self::Back(n));
        }
        if s == "first" {
            return Some(Self::First);
        }
        if s == "now" {
            return Some(Self::Now);
        }
        if let Some(day) = DayName::ALL.into_iter().find(|d| d.as_str() == s) {
            return Some(Self::NamedDay(day));
        }
        if s.as_bytes()[0].is_ascii_digit() {
            return parse_absolute(s)
                .map(Self::Absolute)
                .or_else(|| parse_stamp(s).map(Self::Stamp));
        }
        MarkName::new(s).ok().map(Self::Mark)
    }

    /// For [`TimeSpec::Relative`], the duration; `None` for other forms or on
    /// overflow.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        match *self {
            Self::Relative { amount, unit } => {
                amount.checked_mul(unit.seconds()).map(Duration::from_secs)
            }
            _ => None,
        }
    }
}

/// Digits with no sign and no leading zeros (`0` itself is fine).
fn parse_canonical_uint(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) || (b.len() > 1 && b[0] == b'0') {
        return None;
    }
    s.parse().ok()
}

fn parse_relative(s: &str) -> Option<TimeSpec> {
    let (&last, digits) = s.as_bytes().split_last()?;
    let unit = Unit::from_letter(last)?;
    // `digits` is all ASCII, so this slice is on a char boundary.
    let amount = parse_canonical_uint(s.get(..digits.len())?)?;
    // Reject amounts whose duration overflows, so every Relative is resolvable.
    amount.checked_mul(unit.seconds())?;
    Some(TimeSpec::Relative { amount, unit })
}

/// Fixed-width decimal field: exactly `n` ASCII digits.
fn fixed(b: &[u8]) -> Option<u16> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        b.iter()
            .fold(0u16, |acc, &d| acc * 10 + u16::from(d - b'0')),
    )
}

fn two(b: &[u8]) -> Option<u8> {
    if b.len() != 2 {
        return None;
    }
    fixed(b).and_then(|v| u8::try_from(v).ok())
}

/// A UTC second in ISO 8601 basic format: `YYYYMMDDTHHMMSSZ`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stamp {
    /// The date.
    pub date: Date,
    /// The time, with seconds.
    pub time: TimeOfDay,
}

fn parse_stamp(s: &str) -> Option<Stamp> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
        return None;
    }
    let date = Date::new(fixed(&b[..4])?, two(&b[4..6])?, two(&b[6..8])?)?;
    let (hour, minute, second) = (two(&b[9..11])?, two(&b[11..13])?, two(&b[13..15])?);
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(Stamp {
        date,
        time: TimeOfDay {
            hour,
            minute,
            second: Some(second),
        },
    })
}

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Date { year, month, day } = self.date;
        let TimeOfDay {
            hour,
            minute,
            second,
        } = self.time;
        write!(
            f,
            "{year:04}{month:02}{day:02}T{hour:02}{minute:02}{:02}Z",
            second.unwrap_or(0)
        )
    }
}

fn parse_absolute(s: &str) -> Option<AbsoluteTime> {
    let b = s.as_bytes();
    // YYYY-MM-DD
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = fixed(&b[..4])?;
    let date = Date::new(year, two(&b[5..7])?, two(&b[8..10])?)?;
    let rest = &b[10..];
    if rest.is_empty() {
        return Some(AbsoluteTime {
            date,
            time: None,
            offset: None,
        });
    }
    // THH:MM[:SS]
    let rest = rest.strip_prefix(b"T")?;
    if rest.len() < 5 || rest[2] != b':' {
        return None;
    }
    let hour = two(&rest[..2])?;
    let minute = two(&rest[3..5])?;
    let mut rest = &rest[5..];
    let mut second = None;
    if let Some(r) = rest.strip_prefix(b":") {
        second = Some(two(r.get(..2)?)?);
        rest = &r[2..];
    }
    if hour > 23 || minute > 59 || second.is_some_and(|s| s > 59) {
        return None;
    }
    let time = Some(TimeOfDay {
        hour,
        minute,
        second,
    });
    let offset = match rest {
        [] => None,
        [b'Z'] => Some(Offset::Utc),
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let hours = two(&[*h1, *h2])?;
            let minutes = two(&[*m1, *m2])?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            Some(Offset::Fixed {
                negative: *sign == b'-',
                hours,
                minutes,
            })
        }
        _ => return None,
    };
    Some(AbsoluteTime { date, time, offset })
}

impl fmt::Display for TimeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Relative { amount, unit } => write!(f, "-{amount}{}", unit.letter()),
            Self::NamedDay(d) => f.write_str(d.as_str()),
            Self::Absolute(a) => a.fmt(f),
            Self::Back(n) => write!(f, "~{n}"),
            Self::First => f.write_str("first"),
            Self::Now => f.write_str("now"),
            Self::Stamp(st) => st.fmt(f),
            Self::Mark(m) => m.fmt(f),
            Self::Listing => Ok(()),
        }
    }
}

impl fmt::Display for AbsoluteTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Date { year, month, day } = self.date;
        write!(f, "{year:04}-{month:02}-{day:02}")?;
        if let Some(TimeOfDay {
            hour,
            minute,
            second,
        }) = self.time
        {
            write!(f, "T{hour:02}:{minute:02}")?;
            if let Some(s) = second {
                write!(f, ":{s:02}")?;
            }
            match self.offset {
                None => {}
                Some(Offset::Utc) => f.write_str("Z")?,
                Some(Offset::Fixed {
                    negative,
                    hours,
                    minutes,
                }) => {
                    let sign = if negative { '-' } else { '+' };
                    write!(f, "{sign}{hours:02}:{minutes:02}")?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<TimeSpec> {
        TimeSpec::parse(s)
    }

    fn roundtrips(s: &str) {
        let spec = p(s).unwrap_or_else(|| panic!("{s:?} should parse"));
        assert_eq!(spec.to_string(), s, "display of {s:?}");
    }

    #[test]
    fn relative() {
        for s in ["-90s", "-2h", "-3d", "-2w", "-15m", "-0s", "-1w"] {
            roundtrips(s);
        }
        assert_eq!(
            p("-90s"),
            Some(TimeSpec::Relative {
                amount: 90,
                unit: Unit::Seconds
            })
        );
        assert_eq!(
            p("-2h").and_then(|t| t.duration()),
            Some(Duration::from_secs(7200))
        );
        for s in [
            "-02h", "-2H", "-2", "-h", "-", "-2y", "- 2h", "-2hh", "-1.5h", "--2h",
        ] {
            assert_eq!(p(s), None, "{s:?}");
        }
        // Overflowing durations aren't specs.
        assert_eq!(p(&format!("-{}w", u64::MAX)), None);
        assert!(p(&format!("-{}s", u64::MAX)).is_some());
    }

    #[test]
    fn named_days() {
        for d in DayName::ALL {
            assert_eq!(p(d.as_str()), Some(TimeSpec::NamedDay(d)));
        }
        assert_eq!(p("Monday"), None);
        assert_eq!(
            p("mon"),
            Some(TimeSpec::Mark(MarkName::new("mon").unwrap()))
        );
    }

    #[test]
    fn count_and_first() {
        roundtrips("~1");
        roundtrips("~42");
        assert_eq!(p("~3"), Some(TimeSpec::Back(3)));
        for s in [
            "~0",
            "~01",
            "~",
            "~-1",
            "~x",
            &format!("~{}", u64::from(u32::MAX) + 1),
        ] {
            assert_eq!(p(s), None, "{s:?}");
        }
        assert_eq!(p("first"), Some(TimeSpec::First));
    }

    #[test]
    fn now_and_stamps() {
        assert_eq!(p("now"), Some(TimeSpec::Now));
        roundtrips("now");
        roundtrips("20261030T164512Z");
        roundtrips("00000101T000000Z");
        for s in [
            "20261030T164512",
            "20261030T246000Z",
            "20260230T000000Z",
            "2026103T164512Z",
            "20261030t164512Z",
            "20261030T164512Z1",
        ] {
            assert_eq!(p(s), None, "{s:?}");
        }
    }

    #[test]
    fn listing() {
        assert_eq!(p(""), Some(TimeSpec::Listing));
        assert_eq!(TimeSpec::Listing.to_string(), "");
    }

    #[test]
    fn absolute() {
        for s in [
            "2026-10-01",
            "2026-10-01T14:30",
            "2026-10-01T14:30:05",
            "2026-10-01T14:30:05Z",
            "2026-10-01T14:30Z",
            "2026-10-01T14:30+02:00",
            "2026-10-01T14:30:05-05:30",
            "2026-10-01T00:00-00:00",
            "2024-02-29",
            "2000-02-29",
            "0000-01-01",
            "9999-12-31T23:59:59Z",
        ] {
            roundtrips(s);
        }
        for s in [
            "2026-13-01",
            "2026-00-10",
            "2026-02-29",
            "1900-02-29",
            "2026-04-31",
            "2026-10-1",
            "26-10-01",
            "2026-10-01T24:00",
            "2026-10-01T14:60",
            "2026-10-01T14:30:60",
            "2026-10-01T14",
            "2026-10-01T14:3",
            "2026-10-01t14:30",
            "2026-10-01 14:30",
            "2026-10-01Z",
            "2026-10-01T14:30z",
            "2026-10-01T14:30+2:00",
            "2026-10-01T14:30+0200",
            "2026-10-01T14:30+24:00",
            "2026-10-01T14:30:05.5Z",
            "2026-10-01T14:30Z ",
        ] {
            assert_eq!(p(s), None, "{s:?}");
        }
        let Some(TimeSpec::Absolute(a)) = p("2026-10-01T14:30:05-05:30") else {
            panic!()
        };
        assert_eq!(a.offset.map(Offset::seconds), Some(-(5 * 3600 + 30 * 60)));
    }

    #[test]
    fn marks() {
        roundtrips("before-migration");
        roundtrips("a");
        roundtrips("v2");
        let max = format!("a{}", "b".repeat(63));
        roundtrips(&max);
        assert_eq!(p(&format!("{max}c")), None);
        for s in ["Release", "2fast", "-x", "a_b", "a.b", "a b", "é"] {
            assert!(!matches!(p(s), Some(TimeSpec::Mark(_))), "{s:?}");
        }
        for w in RESERVED_WORDS {
            assert!(MarkName::new(w).is_err(), "{w}");
        }
        assert!(MarkName::new("a-").is_ok());
    }
}
