//! Span-tracking format drivers for fernet.
//!
//! A driver parses a structured file (JSON, TOML, YAML, …) into a tree whose
//! nodes record exact byte spans in the source, so values can be read and later
//! edited as byte splices.
#![forbid(unsafe_code)]
