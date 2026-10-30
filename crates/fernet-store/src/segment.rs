//! Segments: append-only files of records behind a fixed header.

use crate::record::{self, Bad, Record};

/// Segment magic.
pub const MAGIC: [u8; 4] = *b"FSEG";

/// Segment format version.
pub const VERSION: u16 = 1;

/// Header length: magic, version `u16`, segment id `u64`.
pub const HEADER_LEN: usize = 4 + 2 + 8;

/// Segments roll over once they reach this size.
pub const ROLL_AT: u64 = 64 << 20;

/// The file name of segment `id`: six or more digits, `.seg`.
#[must_use]
pub fn file_name(id: u64) -> String {
    format!("{id:06}.seg")
}

/// Serialises a segment header.
#[must_use]
pub fn header(id: u64) -> [u8; HEADER_LEN] {
    let mut h = [0; HEADER_LEN];
    h[..4].copy_from_slice(&MAGIC);
    h[4..6].copy_from_slice(&VERSION.to_le_bytes());
    h[6..].copy_from_slice(&id.to_le_bytes());
    h
}

/// The result of reading a segment's bytes.
#[derive(Debug, Default)]
pub struct Scan {
    /// Every valid record, with its byte offset in the segment.
    pub records: Vec<(u64, Record)>,
    /// Offset just past the last valid record.
    pub valid_end: u64,
    /// The first problem, if the bytes don't end cleanly after the last record.
    pub problem: Option<Problem>,
}

/// What stopped a scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The header is wrong (magic, version, or id doesn't match the file name).
    Header,
    /// A record at `offset` is bad.
    Record {
        /// Byte offset of the bad record.
        offset: u64,
        /// Why.
        bad: Bad,
        /// True if the bad record runs to (or past) the end of the file: on
        /// the active segment that's a torn write, repaired by truncation.
        at_tail: bool,
    },
}

/// Reads every record in `bytes`, stopping at the first problem.
#[must_use]
pub fn scan(bytes: &[u8], id: u64) -> Scan {
    if bytes.len() < HEADER_LEN || bytes[..HEADER_LEN] != header(id) {
        return Scan {
            problem: Some(Problem::Header),
            ..Scan::default()
        };
    }
    let mut at = HEADER_LEN;
    let mut records = Vec::new();
    while at < bytes.len() {
        match record::decode(&bytes[at..]) {
            Ok((r, n)) => {
                records.push((at as u64, r));
                at += n;
            }
            Err(bad) => {
                let at_tail = match bad {
                    Bad::Truncated => true,
                    // A checksum failure on a record that ends exactly at EOF
                    // can be a torn write whose length field landed.
                    Bad::Crc | Bad::Field => declared_end(&bytes[at..]) == Some(bytes.len() - at),
                    Bad::Magic | Bad::Length => false,
                };
                return Scan {
                    records,
                    valid_end: at as u64,
                    problem: Some(Problem::Record {
                        offset: at as u64,
                        bad,
                        at_tail,
                    }),
                };
            }
        }
    }
    Scan {
        records,
        valid_end: at as u64,
        problem: None,
    }
}

fn declared_end(buf: &[u8]) -> Option<usize> {
    let len = u32::from_le_bytes(buf.get(4..8)?.try_into().ok()?) as usize;
    Some(record::PREFIX_LEN + len)
}
