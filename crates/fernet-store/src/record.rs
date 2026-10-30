//! Log records: the on-disk unit of history (`docs/FORMAT.md`, "Log record").

use crate::{Error, Hash, Result};

/// Record magic.
pub const MAGIC: [u8; 4] = *b"FREC";

/// Bytes before the `length` field's payload: magic + length.
pub const PREFIX_LEN: usize = 8;

/// Smallest possible payload: kind, flags, time, `path_len`, hash, size,
/// `extra_len`, crc (with empty path and extra).
pub const MIN_PAYLOAD: usize = 1 + 1 + 8 + 2 + 32 + 8 + 2 + 4;

/// Largest payload a record may declare. Paths and extras are `u16`-sized, so
/// anything bigger is corruption.
pub const MAX_PAYLOAD: usize = MIN_PAYLOAD + 2 * u16::MAX as usize;

/// What a record says happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// A new version of `path` with content `hash`.
    Write = 1,
    /// `path` stopped existing (a tombstone).
    Delete = 2,
    /// `extra` (the old path) moved to `path`, with content `hash`.
    Rename = 3,
    /// A named mark (`extra`) at `time`.
    Mark = 4,
    /// The mark `extra` was removed.
    Unmark = 5,
}

impl Kind {
    fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Write,
            2 => Self::Delete,
            3 => Self::Rename,
            4 => Self::Mark,
            5 => Self::Unmark,
            _ => return None,
        })
    }
}

/// One log record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// What happened.
    pub kind: Kind,
    /// Unix nanoseconds, UTC.
    pub time: i64,
    /// UTF-8, relative to the root, `/`-separated. Empty for marks.
    pub path: String,
    /// BLAKE3 of the uncompressed content; zero for delete, mark and unmark.
    pub hash: Hash,
    /// Uncompressed size; 0 for delete, mark and unmark.
    pub size: u64,
    /// Rename: the old path. Mark and unmark: the mark name. Else empty.
    pub extra: String,
}

impl Record {
    /// Serialises the record, magic to crc.
    ///
    /// # Errors
    ///
    /// [`Error::TooLong`] if `path` or `extra` exceeds `u16::MAX` bytes.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let path_len = u16::try_from(self.path.len()).map_err(|_| Error::TooLong)?;
        let extra_len = u16::try_from(self.extra.len()).map_err(|_| Error::TooLong)?;
        let mut body = Vec::with_capacity(MIN_PAYLOAD + self.path.len() + self.extra.len());
        body.push(self.kind as u8);
        body.push(0); // flags
        body.extend_from_slice(&self.time.to_le_bytes());
        body.extend_from_slice(&path_len.to_le_bytes());
        body.extend_from_slice(self.path.as_bytes());
        body.extend_from_slice(&self.hash.0);
        body.extend_from_slice(&self.size.to_le_bytes());
        body.extend_from_slice(&extra_len.to_le_bytes());
        body.extend_from_slice(self.extra.as_bytes());
        let crc = crc32c::crc32c(&body);
        let length = u32::try_from(body.len() + 4).map_err(|_| Error::TooLong)?;
        let mut out = Vec::with_capacity(PREFIX_LEN + body.len() + 4);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc.to_le_bytes());
        Ok(out)
    }
}

/// Why bytes at some offset aren't a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// The bytes end before the record does: a torn write if at the tail.
    Truncated,
    /// The magic isn't `FREC`.
    Magic,
    /// The declared length is impossible.
    Length,
    /// The checksum doesn't match.
    Crc,
    /// The checksum matches but a field is invalid (unknown kind, non-zero
    /// flags, inconsistent lengths, non-UTF-8 text).
    Field,
}

/// Decodes the record at the start of `buf`. Returns it and its total length.
///
/// # Errors
///
/// [`Bad`] describing the first problem found.
pub fn decode(buf: &[u8]) -> std::result::Result<(Record, usize), Bad> {
    if buf.len() < PREFIX_LEN {
        let n = buf.len().min(4);
        return Err(if buf[..n] == MAGIC[..n] {
            Bad::Truncated
        } else {
            Bad::Magic
        });
    }
    if buf[..4] != MAGIC {
        return Err(Bad::Magic);
    }
    let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if !(MIN_PAYLOAD..=MAX_PAYLOAD).contains(&length) {
        return Err(Bad::Length);
    }
    let total = PREFIX_LEN + length;
    let Some(payload) = buf.get(PREFIX_LEN..total) else {
        return Err(Bad::Truncated);
    };
    let (body, crc) = payload.split_at(length - 4);
    if crc32c::crc32c(body) != u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) {
        return Err(Bad::Crc);
    }
    let record = parse_body(body).ok_or(Bad::Field)?;
    Ok((record, total))
}

fn parse_body(b: &[u8]) -> Option<Record> {
    let mut r = Reader(b);
    let kind = Kind::from_u8(r.u8()?)?;
    if r.u8()? != 0 {
        return None;
    }
    let time = i64::from_le_bytes(r.array()?);
    let path_len = u16::from_le_bytes(r.array()?) as usize;
    let path = String::from_utf8(r.take(path_len)?.to_vec()).ok()?;
    let hash = Hash(r.array()?);
    let size = u64::from_le_bytes(r.array()?);
    let extra_len = u16::from_le_bytes(r.array()?) as usize;
    let extra = String::from_utf8(r.take(extra_len)?.to_vec()).ok()?;
    r.0.is_empty().then_some(Record {
        kind,
        time,
        path,
        hash,
        size,
        extra,
    })
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        Record {
            kind: Kind::Rename,
            time: -5,
            path: "a/ü.txt".into(),
            hash: Hash([7; 32]),
            size: 42,
            extra: "old/name".into(),
        }
    }

    #[test]
    fn roundtrip() {
        let bytes = sample().encode().unwrap();
        assert_eq!(&bytes[..4], b"FREC");
        let (r, n) = decode(&bytes).unwrap();
        assert_eq!((r, n), (sample(), bytes.len()));
        let mut two = bytes.clone();
        two.extend_from_slice(&bytes);
        assert_eq!(decode(&two).unwrap().1, bytes.len());
    }

    #[test]
    fn every_truncation_is_truncated() {
        let bytes = sample().encode().unwrap();
        for k in 0..bytes.len() {
            assert_eq!(decode(&bytes[..k]).unwrap_err(), Bad::Truncated, "at {k}");
        }
    }

    #[test]
    fn every_bit_flip_is_detected() {
        let bytes = sample().encode().unwrap();
        for i in 0..bytes.len() {
            for bit in 0..8 {
                let mut b = bytes.clone();
                b[i] ^= 1 << bit;
                assert!(decode(&b).is_err(), "flip at {i}.{bit} decoded");
            }
        }
    }

    #[test]
    fn too_long() {
        let mut r = sample();
        r.path = "x".repeat(70_000);
        assert!(matches!(r.encode(), Err(Error::TooLong)));
    }
}
