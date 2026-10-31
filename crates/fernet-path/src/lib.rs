//! The fernet path grammar.
//!
//! A fernet path is an ordinary relative path in which components may carry a
//! time lens (`name@spec`) and an open lens (`name#`):
//!
//! ```text
//! <real components> / <name>[@<timespec>][#] [/ <pointer token>]*
//! ```
//!
//! This crate parses that grammar. It is pure (no I/O) and has no dependencies
//! beyond `std`. Parsing is syntactic: whether a component is a real name or a
//! lens is decided by the resolver using the "real names win" rule, which is
//! why parsing is total and printing is its exact inverse. See
//! `docs/GRAMMAR.md` for the normative text.
//!
//! ```
//! use fernet_fs_path::FernetPath;
//!
//! let p = FernetPath::parse("infra.yaml@yesterday#/services/api/image").unwrap();
//! assert_eq!(p.to_string(), "infra.yaml@yesterday#/services/api/image");
//! let view = p.lens_view();
//! assert_eq!(view.time, Some(0));
//! assert_eq!(view.pointer().unwrap(), ["services", "api", "image"]);
//! ```
#![forbid(unsafe_code)]

mod component;
mod pointer;
mod timespec;

use std::fmt;

pub use component::Component;
pub use pointer::{decode_token, encode_token};
pub use timespec::{
    AbsoluteTime, Date, DayName, InvalidMarkName, MarkName, Offset, RESERVED_WORDS, Stamp,
    TimeOfDay, TimeSpec, Unit,
};

/// A parsed fernet path: a non-empty list of components.
///
/// Components after an open component are pointer tokens. They're kept as
/// [`Component`]s so the path prints back exactly; read them with
/// [`Component::pointer_token`] or [`FernetPath::lens_view`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FernetPath {
    /// The components, in order. Never empty.
    pub components: Vec<Component>,
}

/// Errors from [`FernetPath::parse`]. Individual components never fail to
/// parse; only the path's overall shape can be wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The path has no components.
    Empty,
    /// The path contains a NUL byte.
    Nul,
    /// The path starts with `/`; fernet paths are relative to the root.
    Absolute,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty path",
            Self::Nul => "path contains a NUL byte",
            Self::Absolute => "path is absolute; fernet paths are relative to the root",
        })
    }
}

impl std::error::Error for ParseError {}

impl FernetPath {
    /// Parses a relative path. Empty segments (`a//b`, a trailing `/`) are
    /// dropped, as the kernel does, so
    /// `parse(s).to_string()` equals `s` with empty segments removed.
    ///
    /// # Errors
    ///
    /// See [`ParseError`].
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        if s.contains('\0') {
            return Err(ParseError::Nul);
        }
        if s.starts_with('/') {
            return Err(ParseError::Absolute);
        }
        let components: Vec<_> = s
            .split('/')
            .filter(|seg| !seg.is_empty())
            .map(Component::parse)
            .collect();
        if components.is_empty() {
            return Err(ParseError::Empty);
        }
        Ok(Self { components })
    }

    /// The syntactic reading of this path, assuming no real names get in the
    /// way. The resolver may still serve a lens-shaped component as a real
    /// name; this view is for tooling (completions, diagnostics).
    #[must_use]
    pub fn lens_view(&self) -> LensView<'_> {
        let open = self.components.iter().position(|c| c.open);
        let limit = open.map_or(self.components.len(), |i| i + 1);
        let time = self.components[..limit]
            .iter()
            .position(|c| c.when.is_some());
        LensView {
            path: self,
            time,
            open,
        }
    }
}

/// The syntactic reading of a [`FernetPath`]; see [`FernetPath::lens_view`].
///
/// At most one time lens and one open lens apply: the first of each, with the
/// time lens at or before the open lens. Later lens-shaped components are
/// literal names (before the open lens) or pointer tokens (after it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LensView<'a> {
    path: &'a FernetPath,
    /// Index of the component carrying the time lens.
    pub time: Option<usize>,
    /// Index of the component carrying the open lens.
    pub open: Option<usize>,
}

impl LensView<'_> {
    /// The decoded pointer after the open lens; empty if there is no open lens.
    /// `None` if a token has an invalid `~` escape.
    #[must_use]
    pub fn pointer(&self) -> Option<Vec<String>> {
        let Some(open) = self.open else {
            return Some(Vec::new());
        };
        self.path.components[open + 1..]
            .iter()
            .map(Component::pointer_token)
            .collect()
    }
}

impl fmt::Display for FernetPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, c) in self.components.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            c.fmt(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_errors() {
        assert_eq!(FernetPath::parse(""), Err(ParseError::Empty));
        assert_eq!(FernetPath::parse("//"), Err(ParseError::Absolute));
        assert_eq!(FernetPath::parse("/etc"), Err(ParseError::Absolute));
        assert_eq!(FernetPath::parse("a\0b"), Err(ParseError::Nul));
    }

    #[test]
    fn collapses_empty_segments() {
        let p = FernetPath::parse("a//b/").unwrap();
        assert_eq!(p.to_string(), "a/b");
    }

    #[test]
    fn lens_view() {
        let p = FernetPath::parse("src@2026-10-01/config.json#/a~1b/0").unwrap();
        let v = p.lens_view();
        assert_eq!((v.time, v.open), (Some(0), Some(1)));
        assert_eq!(v.pointer().unwrap(), ["a/b", "0"]);

        // Lens-shaped tokens after `#` are just tokens.
        let p = FernetPath::parse("a.json#/x@-1h/y#").unwrap();
        let v = p.lens_view();
        assert_eq!((v.time, v.open), (None, Some(0)));
        assert_eq!(v.pointer().unwrap(), ["x@-1h", "y#"]);

        // Only the first time lens counts.
        let v = FernetPath::parse("a@-1d/b@-2h").unwrap();
        assert_eq!(v.lens_view().time, Some(0));

        let p = FernetPath::parse("plain/dir").unwrap();
        let v = p.lens_view();
        assert_eq!((v.time, v.open), (None, None));
        assert_eq!(v.pointer().unwrap(), Vec::<String>::new());

        let p = FernetPath::parse("a.json#/~x").unwrap();
        assert_eq!(p.lens_view().pointer(), None);
    }
}
