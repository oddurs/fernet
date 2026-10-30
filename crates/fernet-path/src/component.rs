//! One `/`-separated component and its lenses.

use std::fmt;

use crate::pointer;
use crate::timespec::TimeSpec;

/// One component of a fernet path: `name[@spec][#]`.
///
/// Parsing is purely syntactic. [`Component::parse`] never fails: a segment
/// whose lens isn't in canonical form is a plain name. Printing is the exact
/// inverse of parsing, so `Component::parse(s).to_string() == s` for every
/// segment `s`, and the resolver can always recover the text the user typed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Component {
    /// The real-name part.
    pub name: String,
    /// The time lens, if the component carries `@spec`.
    pub when: Option<TimeSpec>,
    /// True if the component ends with the open lens `#`.
    pub open: bool,
}

impl Component {
    /// A plain name with no lenses.
    #[must_use]
    pub fn plain(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            when: None,
            open: false,
        }
    }

    /// Parses one segment. Total: every string is a component.
    ///
    /// Rules, in order:
    /// 1. A trailing `#` sets `open` (only one is stripped: `a##` is `a#`, open).
    /// 2. The rest is split at its **last** `@`. If the text after it is a
    ///    canonical [`TimeSpec`] and the text before it is a non-empty name,
    ///    that's the time lens. Otherwise the `@` is part of the name.
    /// 3. A time lens needs a name before the `@`: `@-2h` is a plain name and
    ///    `@-2h#` is the name `@-2h`, opened. A bare `#` is a plain name, and
    ///    `.` and `..` never carry lenses (`..#` is a plain name).
    #[must_use]
    pub fn parse(segment: &str) -> Self {
        let (rest, open) = match segment.strip_suffix('#') {
            Some(rest) => (rest, true),
            None => (segment, false),
        };
        let (name, when) = match rest.rsplit_once('@') {
            Some((name, spec)) => match TimeSpec::parse(spec) {
                Some(spec) if !name.is_empty() => (name, Some(spec)),
                _ => (rest, None),
            },
            None => (rest, None),
        };
        if name.is_empty() || name == "." || name == ".." {
            return Self::plain(segment);
        }
        Self {
            name: name.to_owned(),
            when,
            open,
        }
    }

    /// True if this component carries any lens.
    #[must_use]
    pub fn has_lens(&self) -> bool {
        self.when.is_some() || self.open
    }

    /// True if printing and re-parsing yields this same component. Values from
    /// [`Component::parse`] always are; hand-built ones may not be (for
    /// example a plain name `a@-2h` would re-parse as a lens).
    #[must_use]
    pub fn is_canonical(&self) -> bool {
        !self.name.is_empty() && Self::parse(&self.to_string()) == *self
    }

    /// The component's text read as an RFC 6901 pointer token (with fernet's
    /// extension; see [`crate::decode_token`]). Lenses are not interpreted:
    /// the whole printed text is the token. `None` for an invalid escape.
    #[must_use]
    pub fn pointer_token(&self) -> Option<String> {
        pointer::decode_token(&self.to_string())
    }
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if let Some(spec) = &self.when {
            write!(f, "@{spec}")?;
        }
        if self.open {
            f.write_str("#")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timespec::{DayName, MarkName, Unit};

    fn c(s: &str) -> Component {
        let comp = Component::parse(s);
        assert_eq!(comp.to_string(), s, "bijection for {s:?}");
        comp
    }

    #[test]
    fn plain_names() {
        for s in [
            "notes.md", "a b", "ünï", "#", "@", "@-2h", ".", "..", "..#", ".@-2h", "..@~1#",
        ] {
            let comp = c(s);
            assert!(!comp.has_lens(), "{s:?} -> {comp:?}");
        }
        assert_eq!(
            c("@-2h#"),
            Component {
                name: "@-2h".into(),
                when: None,
                open: true
            }
        );
        assert_eq!(c("user@example.com"), Component::plain("user@example.com"));
        assert_eq!(c("x@-02h"), Component::plain("x@-02h"));
        assert_eq!(c("x@Monday"), Component::plain("x@Monday"));
    }

    #[test]
    fn lenses() {
        assert_eq!(
            c("notes.md@-2h"),
            Component {
                name: "notes.md".into(),
                when: Some(TimeSpec::Relative {
                    amount: 2,
                    unit: Unit::Hours
                }),
                open: false
            }
        );
        assert_eq!(
            c("infra.yaml@yesterday#"),
            Component {
                name: "infra.yaml".into(),
                when: Some(TimeSpec::NamedDay(DayName::Yesterday)),
                open: true
            }
        );
        assert_eq!(
            c("config.json#"),
            Component {
                name: "config.json".into(),
                when: None,
                open: true
            }
        );
        assert_eq!(
            c("notes.md@"),
            Component {
                name: "notes.md".into(),
                when: Some(TimeSpec::Listing),
                open: false
            }
        );
        assert_eq!(
            c("x@"),
            Component {
                name: "x".into(),
                when: Some(TimeSpec::Listing),
                open: false
            }
        );
    }

    #[test]
    fn splits_at_last_at() {
        let comp = c("a@b@-2h");
        assert_eq!(comp.name, "a@b");
        assert!(comp.when.is_some());
        let comp = c("me@home");
        assert_eq!(comp.name, "me");
        assert_eq!(
            comp.when,
            Some(TimeSpec::Mark(MarkName::new("home").unwrap()))
        );
        // The last `@` isn't a spec, so no lens even though an earlier one could be.
        assert_eq!(c("a@-2h@X"), Component::plain("a@-2h@X"));
    }

    #[test]
    fn hash_rules() {
        assert_eq!(
            c("a##"),
            Component {
                name: "a#".into(),
                when: None,
                open: true
            }
        );
        // `#` not last: part of the name.
        assert_eq!(c("a#b"), Component::plain("a#b"));
        // Wrong order: `#@` makes `a#` the name.
        let comp = c("a#@-1h");
        assert_eq!(comp.name, "a#");
        assert!(!comp.open);
        // Invalid spec but trailing `#`: a real name, opened.
        assert_eq!(
            c("foo@B#"),
            Component {
                name: "foo@B".into(),
                when: None,
                open: true
            }
        );
    }

    #[test]
    fn canonical() {
        assert!(Component::parse("a@-2h#").is_canonical());
        assert!(!Component::plain("a@-2h").is_canonical());
        assert!(!Component::plain("a#").is_canonical());
        assert!(!Component::plain("").is_canonical());
    }

    #[test]
    fn tokens() {
        assert_eq!(c("a~1b").pointer_token().as_deref(), Some("a/b"));
        assert_eq!(c("me@home").pointer_token().as_deref(), Some("me@home"));
        assert_eq!(c("~e").pointer_token().as_deref(), Some(""));
        assert_eq!(c("~2").pointer_token(), None);
    }
}
