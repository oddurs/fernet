//! The fernet path grammar.
//!
//! A fernet path is an ordinary relative path in which components may carry a
//! time lens (`name@spec`) and an open lens (`name#`). This crate parses that
//! grammar. It is pure: no I/O and no dependencies beyond `std`. Whether a
//! component is a real name or a lens is decided by the resolver, using the
//! "real names win" rule. See `docs/GRAMMAR.md` for the normative text.
#![forbid(unsafe_code)]
