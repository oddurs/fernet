//! The fernet resolver: path grammar + store + format drivers → nodes.
//!
//! Both front doors (the FUSE mount and the CLI) resolve paths through this
//! crate, so every path resolves identically through either (invariant 7).
#![forbid(unsafe_code)]
