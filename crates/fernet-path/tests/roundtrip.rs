//! Property tests for the grammar's round-trip guarantees.

use fernet_fs_path::{
    AbsoluteTime, Component, Date, DayName, FernetPath, MarkName, Offset, TimeOfDay, TimeSpec,
    Unit, decode_token, encode_token,
};
use proptest::prelude::*;

fn unit() -> impl Strategy<Value = Unit> {
    prop_oneof![
        Just(Unit::Seconds),
        Just(Unit::Minutes),
        Just(Unit::Hours),
        Just(Unit::Days),
        Just(Unit::Weeks)
    ]
}

fn absolute() -> impl Strategy<Value = AbsoluteTime> {
    let date = (0u16..=9999, 1u8..=12, 1u8..=31)
        .prop_filter_map("valid date", |(y, m, d)| Date::new(y, m, d));
    let time = proptest::option::of((0u8..24, 0u8..60, proptest::option::of(0u8..60)).prop_map(
        |(hour, minute, second)| TimeOfDay {
            hour,
            minute,
            second,
        },
    ));
    let offset = proptest::option::of(prop_oneof![
        Just(Offset::Utc),
        (any::<bool>(), 0u8..24, 0u8..60).prop_map(|(negative, hours, minutes)| Offset::Fixed {
            negative,
            hours,
            minutes
        }),
    ]);
    (date, time, offset).prop_map(|(date, time, offset)| AbsoluteTime {
        date,
        offset: time.and(offset),
        time,
    })
}

fn timespec() -> impl Strategy<Value = TimeSpec> {
    prop_oneof![
        (0u64..10_000_000, unit()).prop_map(|(amount, unit)| TimeSpec::Relative { amount, unit }),
        proptest::sample::select(DayName::ALL.to_vec()).prop_map(TimeSpec::NamedDay),
        absolute().prop_map(TimeSpec::Absolute),
        (1u32..).prop_map(TimeSpec::Back),
        Just(TimeSpec::First),
        "[a-z][a-z0-9-]{0,63}"
            .prop_filter_map("mark", |s| MarkName::new(&s).ok().map(TimeSpec::Mark)),
        Just(TimeSpec::Listing),
    ]
}

fn component() -> impl Strategy<Value = Component> {
    let name = prop_oneof!["[a-zA-Z0-9._ ~-]{1,12}", "[a-z.@#~é-]{1,8}", "\\PC{1,6}"]
        .prop_filter("no separators", |s: &String| {
            !s.contains('/') && !s.contains('\0')
        });
    (name, proptest::option::of(timespec()), any::<bool>())
        .prop_map(|(name, when, open)| Component { name, when, open })
        .prop_filter("canonical", Component::is_canonical)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    /// The brief's property: parse(display(p)) == p for generated paths.
    #[test]
    fn parse_display(components in proptest::collection::vec(component(), 1..6)) {
        let p = FernetPath { components };
        prop_assert_eq!(FernetPath::parse(&p.to_string()), Ok(p));
    }

    /// The stronger property: display(parse(s)) == s for any string without
    /// empty segments.
    #[test]
    fn display_parse(segments in proptest::collection::vec("[^/\0]{1,10}|[a-z]{1,4}[@#~][a-z0-9:~-]{0,10}#?", 1..6)) {
        let s = segments.join("/");
        let p = FernetPath::parse(&s);
        prop_assert!(p.is_ok());
        prop_assert_eq!(p.unwrap().to_string(), s);
    }

    #[test]
    fn spec_display_parse(spec in timespec()) {
        prop_assert_eq!(TimeSpec::parse(&spec.to_string()), Some(spec));
    }

    #[test]
    fn token_roundtrip(key in "[^\0]{0,12}") {
        let token = encode_token(&key).unwrap();
        prop_assert!(!token.contains('/'));
        prop_assert_eq!(decode_token(&token), Some(key));
    }
}
