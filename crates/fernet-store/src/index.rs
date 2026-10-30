//! The index: disposable lookup tables rebuilt from the log (invariant 5).
//!
//! Two backends hold the same tables. [`DiskIndex`] (redb, in
//! `.fernet/index/`) persists between runs for the process holding the store
//! lock. [`MemIndex`] is built by replaying the log and serves read-only opens
//! while another process holds the lock. Queries are written once, over the
//! [`Tables`] trait, so both answer identically.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::record::{Kind, Record};
use crate::{Error, Hash, Result};

/// Index format version; a mismatch triggers a rebuild.
pub(crate) const FORMAT: u64 = 1;

/// Where a record lives in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VersionId {
    /// Segment id.
    pub segment: u64,
    /// Byte offset of the record in the segment.
    pub offset: u64,
}

/// One version of one path, as the index sees it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    /// The path.
    pub path: String,
    /// Unix nanoseconds, UTC.
    pub time: i64,
    /// Log order, to break ties between records with the same time.
    pub ord: u64,
    /// The record.
    pub id: VersionId,
    /// Content hash, or `None` for a tombstone.
    pub hash: Option<Hash>,
    /// Uncompressed size (0 for tombstones).
    pub size: u64,
}

/// How far the index has read the log: the next record's position and
/// ordinal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    /// Segment id of the next record.
    pub segment: u64,
    /// Offset of the next record in that segment.
    pub offset: u64,
    /// Ordinal of the next record.
    pub ord: u64,
}

/// One row of the versions table, keyed `(path, time, ord)`.
type Row = (String, i64, u64, VersionId, Option<Hash>, u64);

/// Storage-level operations both backends provide.
pub(crate) trait Tables {
    /// Versions of `path`, oldest first.
    fn versions(&self, path: &str) -> Result<Vec<Version>>;
    /// Distinct paths starting with `prefix`, sorted.
    fn paths(&self, prefix: &str) -> Result<Vec<String>>;
    /// Every mark and its time.
    fn marks(&self) -> Result<BTreeMap<String, i64>>;
    /// Where the next record goes.
    fn position(&self) -> Position;
    /// Applies records (with their ids) and the new position, atomically.
    /// Not necessarily durable until [`Tables::persist`].
    fn apply(&mut self, records: &[(VersionId, Record)], next: Position) -> Result<()>;
    /// Makes applied changes durable.
    fn persist(&mut self) -> Result<()>;
}

/// Rows and mark changes a record implies.
fn rows(id: VersionId, ord: u64, r: &Record) -> (Vec<Row>, Option<(String, Option<i64>)>) {
    match r.kind {
        Kind::Write => (
            vec![(r.path.clone(), r.time, ord, id, Some(r.hash), r.size)],
            None,
        ),
        Kind::Delete => (vec![(r.path.clone(), r.time, ord, id, None, 0)], None),
        Kind::Rename => (
            vec![
                (r.path.clone(), r.time, ord, id, Some(r.hash), r.size),
                (r.extra.clone(), r.time, ord, id, None, 0),
            ],
            None,
        ),
        Kind::Mark => (Vec::new(), Some((r.extra.clone(), Some(r.time)))),
        Kind::Unmark => (Vec::new(), Some((r.extra.clone(), None))),
    }
}

fn version(row: Row) -> Version {
    let (path, time, ord, id, hash, size) = row;
    Version {
        path,
        time,
        ord,
        id,
        hash,
        size,
    }
}

/// The in-memory backend.
/// `(path, time, ord) → (id, hash, size)`.
type MemRows = BTreeMap<(String, i64, u64), (VersionId, Option<Hash>, u64)>;

#[derive(Debug, Default)]
pub(crate) struct MemIndex {
    rows: MemRows,
    marks: BTreeMap<String, i64>,
    position: Position,
}

impl Tables for MemIndex {
    fn versions(&self, path: &str) -> Result<Vec<Version>> {
        let lo = (path.to_owned(), i64::MIN, 0);
        let hi = (path.to_owned(), i64::MAX, u64::MAX);
        Ok(self
            .rows
            .range(lo..=hi)
            .map(|((p, t, o), (id, h, s))| version((p.clone(), *t, *o, *id, *h, *s)))
            .collect())
    }

    fn paths(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        let lo = (prefix.to_owned(), i64::MIN, 0);
        for ((p, _, _), _) in self.rows.range((Bound::Included(lo), Bound::Unbounded)) {
            if !p.starts_with(prefix) {
                break;
            }
            if out.last() != Some(p) {
                out.push(p.clone());
            }
        }
        Ok(out)
    }

    fn marks(&self) -> Result<BTreeMap<String, i64>> {
        Ok(self.marks.clone())
    }

    fn position(&self) -> Position {
        self.position
    }

    fn apply(&mut self, records: &[(VersionId, Record)], next: Position) -> Result<()> {
        for (ord, (id, r)) in (self.position.ord..).zip(records) {
            let (rows, mark) = rows(*id, ord, r);
            for (p, t, o, id, h, s) in rows {
                self.rows.insert((p, t, o), (id, h, s));
            }
            match mark {
                Some((name, Some(t))) => {
                    self.marks.insert(name, t);
                }
                Some((name, None)) => {
                    self.marks.remove(&name);
                }
                None => {}
            }
        }
        self.position = next;
        Ok(())
    }

    fn persist(&mut self) -> Result<()> {
        Ok(())
    }
}

const VERSIONS: TableDefinition<(&str, i64, u64), [u8; 57]> = TableDefinition::new("versions");
const COUNTS: TableDefinition<&str, u64> = TableDefinition::new("counts");
const MARKS: TableDefinition<&str, i64> = TableDefinition::new("marks");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

fn pack(id: VersionId, hash: Option<Hash>, size: u64) -> [u8; 57] {
    let mut v = [0; 57];
    v[..8].copy_from_slice(&id.segment.to_le_bytes());
    v[8..16].copy_from_slice(&id.offset.to_le_bytes());
    v[16..48].copy_from_slice(&hash.map_or([0; 32], |h| h.0));
    v[48..56].copy_from_slice(&size.to_le_bytes());
    v[56] = u8::from(hash.is_some());
    v
}

fn unpack(v: &[u8; 57]) -> (VersionId, Option<Hash>, u64) {
    let u = |r: std::ops::Range<usize>| {
        let mut b = [0; 8];
        b.copy_from_slice(&v[r]);
        u64::from_le_bytes(b)
    };
    let mut h = [0; 32];
    h.copy_from_slice(&v[16..48]);
    let id = VersionId {
        segment: u(0..8),
        offset: u(8..16),
    };
    (id, (v[56] == 1).then_some(Hash(h)), u(48..56))
}

fn db_err(e: impl std::fmt::Display) -> Error {
    Error::Index(e.to_string().to_lowercase())
}

/// The redb backend.
pub(crate) struct DiskIndex {
    db: Database,
    position: Position,
}

impl DiskIndex {
    /// Opens (or creates) the index at `path`. Returns `None` if it exists but
    /// can't be used (wrong format, unreadable), so the caller rebuilds.
    pub(crate) fn open(path: &Path) -> Result<Option<Self>> {
        let db = match Database::create(path) {
            Ok(db) => db,
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                return Err(Error::Index("index is open in another process".into()));
            }
            Err(_) => return Ok(None),
        };
        let txn = db.begin_read().map_err(db_err)?;
        let meta = match txn.open_table(META) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(_) => return Ok(None),
        };
        let get = |k: &str| -> Option<u64> { meta.as_ref()?.get(k).ok()??.value().into() };
        let position = match get("format") {
            None => {
                drop(meta);
                drop(txn);
                let me = Self {
                    db,
                    position: Position::default(),
                };
                me.init()?;
                return Ok(Some(me));
            }
            Some(FORMAT) => Position {
                segment: get("segment").unwrap_or(0),
                offset: get("offset").unwrap_or(0),
                ord: get("ord").unwrap_or(0),
            },
            Some(_) => return Ok(None),
        };
        drop(meta);
        drop(txn);
        Ok(Some(Self { db, position }))
    }

    fn init(&self) -> Result<()> {
        let txn = self.db.begin_write().map_err(db_err)?;
        {
            let mut meta = txn.open_table(META).map_err(db_err)?;
            meta.insert("format", FORMAT).map_err(db_err)?;
            txn.open_table(VERSIONS).map_err(db_err)?;
            txn.open_table(COUNTS).map_err(db_err)?;
            txn.open_table(MARKS).map_err(db_err)?;
        }
        txn.commit().map_err(db_err)
    }
}

impl Tables for DiskIndex {
    fn versions(&self, path: &str) -> Result<Vec<Version>> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(VERSIONS).map_err(db_err)?;
        let mut out = Vec::new();
        for item in t
            .range((path, i64::MIN, 0)..=(path, i64::MAX, u64::MAX))
            .map_err(db_err)?
        {
            let (k, v) = item.map_err(db_err)?;
            let (p, time, ord) = k.value();
            let (id, hash, size) = unpack(&v.value());
            out.push(version((p.to_owned(), time, ord, id, hash, size)));
        }
        Ok(out)
    }

    fn paths(&self, prefix: &str) -> Result<Vec<String>> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(COUNTS).map_err(db_err)?;
        let mut out = Vec::new();
        for item in t.range(prefix..).map_err(db_err)? {
            let (k, _) = item.map_err(db_err)?;
            let p = k.value();
            if !p.starts_with(prefix) {
                break;
            }
            out.push(p.to_owned());
        }
        Ok(out)
    }

    fn marks(&self) -> Result<BTreeMap<String, i64>> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(MARKS).map_err(db_err)?;
        let mut out = BTreeMap::new();
        for item in t.iter().map_err(db_err)? {
            let (k, v) = item.map_err(db_err)?;
            out.insert(k.value().to_owned(), v.value());
        }
        Ok(out)
    }

    fn position(&self) -> Position {
        self.position
    }

    fn apply(&mut self, records: &[(VersionId, Record)], next: Position) -> Result<()> {
        let mut txn = self.db.begin_write().map_err(db_err)?;
        // The log is truth: a lost index commit is replayed on the next open.
        txn.set_durability(redb::Durability::None).map_err(db_err)?;
        {
            let mut versions = txn.open_table(VERSIONS).map_err(db_err)?;
            let mut counts = txn.open_table(COUNTS).map_err(db_err)?;
            let mut marks = txn.open_table(MARKS).map_err(db_err)?;
            let mut meta = txn.open_table(META).map_err(db_err)?;
            for (ord, (id, r)) in (self.position.ord..).zip(records) {
                let (rows, mark) = rows(*id, ord, r);
                for (p, t, o, id, h, s) in rows {
                    versions
                        .insert((p.as_str(), t, o), pack(id, h, s))
                        .map_err(db_err)?;
                    let n = counts
                        .get(p.as_str())
                        .map_err(db_err)?
                        .map_or(0, |v| v.value());
                    counts.insert(p.as_str(), n + 1).map_err(db_err)?;
                }
                match mark {
                    Some((name, Some(t))) => {
                        marks.insert(name.as_str(), t).map_err(db_err)?;
                    }
                    Some((name, None)) => {
                        marks.remove(name.as_str()).map_err(db_err)?;
                    }
                    None => {}
                }
            }
            meta.insert("segment", next.segment).map_err(db_err)?;
            meta.insert("offset", next.offset).map_err(db_err)?;
            meta.insert("ord", next.ord).map_err(db_err)?;
        }
        txn.commit().map_err(db_err)?;
        self.position = next;
        Ok(())
    }

    fn persist(&mut self) -> Result<()> {
        let txn = self.db.begin_write().map_err(db_err)?;
        txn.commit().map_err(db_err)
    }
}
