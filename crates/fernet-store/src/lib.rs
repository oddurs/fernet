//! The fernet history store: segments, records, objects, manifest and index.
//!
//! Layout, inside the backing root (`docs/FORMAT.md` is normative):
//!
//! ```text
//! .fernet/
//! ├── FORMAT          "fernet-store 1\n"
//! ├── MANIFEST        live segment ids; replaced atomically
//! ├── config.toml     settings
//! ├── log/            000001.seg …
//! ├── objects/        ab/cdef…  (BLAKE3 hex; zstd)
//! ├── index/          disposable (redb)
//! └── lock            advisory lock; one writer per store
//! ```
//!
//! History is append-only (invariant 2): records are only ever appended, and
//! blobs are written before the records that name them. The log is truth and
//! the index is disposable (invariant 5).
#![forbid(unsafe_code)]

mod index;
mod objects;
pub mod record;
pub mod segment;
mod verify;

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use index::{DiskIndex, MemIndex, Tables};
pub use index::{Position, Version, VersionId};
pub(crate) use objects::Objects;
pub use record::{Kind, Record};
pub use verify::{Report, verify};

/// The store directory's name inside the backing root.
pub const DIR: &str = ".fernet";

/// Contents of `.fernet/FORMAT`.
pub const FORMAT_LINE: &str = "fernet-store 1\n";

/// First line of `MANIFEST`.
pub const MANIFEST_HEADER: &str = "fernet-manifest 1";

/// Default group-commit interval.
pub const GROUP_COMMIT: Duration = Duration::from_millis(50);

/// A BLAKE3 hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// The all-zero hash, used by records without content.
    pub const ZERO: Self = Self([0; 32]);

    /// Hashes `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Lowercase hex.
    #[must_use]
    pub fn to_hex(&self) -> String {
        use fmt::Write as _;
        self.0.iter().fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    /// Parses 64 lowercase hex digits.
    #[must_use]
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        let mut out = [0; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", &self.to_hex()[..12])
    }
}

/// Store errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No `.fernet/` here.
    #[error("no store at {0}")]
    NotAStore(PathBuf),
    /// `.fernet/` exists already (on init).
    #[error("a store already exists at {0}")]
    Exists(PathBuf),
    /// `FORMAT` names a version this build can't read.
    #[error("unsupported store format {0:?}")]
    Format(String),
    /// Another process holds the writer lock.
    #[error("the store is busy: another fernet process is writing to it")]
    Busy,
    /// The store was opened read-only.
    #[error("the store is open read-only")]
    ReadOnly,
    /// The log is damaged somewhere other than a torn final record.
    #[error("the log is damaged in segment {segment} at byte {offset}")]
    Corrupt {
        /// Segment id.
        segment: u64,
        /// Byte offset of the first problem.
        offset: u64,
    },
    /// A blob is missing or doesn't match its hash.
    #[error("blob {} is missing or damaged", &hash.to_hex()[..12])]
    BadBlob {
        /// The blob's hash.
        hash: Hash,
    },
    /// A path or extra field is longer than a record allows.
    #[error("path too long for a record (max 65535 bytes)")]
    TooLong,
    /// A mark already exists or doesn't exist.
    #[error("{0}")]
    Mark(String),
    /// The store's files aren't laid out as expected.
    #[error("store layout: {0}")]
    Layout(&'static str),
    /// The index failed.
    #[error("index: {0}")]
    Index(String),
    /// An I/O error.
    #[error("{path}: {source}")]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The error.
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

/// Store result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Fsyncs a directory so renames and creations in it are durable.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| Error::io(dir, e))
}

/// Writes `bytes` to `path` atomically: temp file, fsync, rename, fsync dir.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or(Error::Layout("path has no parent"))?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        f.write_all(bytes).map_err(|e| Error::io(&tmp, e))?;
        f.sync_all().map_err(|e| Error::io(&tmp, e))?;
    }
    fs::rename(&tmp, path).map_err(|e| Error::io(path, e))?;
    sync_dir(dir)
}

/// How to open a store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Take the writer lock; fail with [`Error::Busy`] if another process has it.
    Write,
    /// Take the lock if it's free (to use and update the persistent index);
    /// otherwise read the log into memory without touching anything.
    Read,
}

struct Active {
    file: File,
    id: u64,
    len: u64,
}

/// An open store.
pub struct Store {
    dir: PathBuf,
    objects: Objects,
    segments: Vec<u64>,
    active: Option<Active>,
    index: Box<dyn Tables + Send>,
    lock: Option<File>,
    group_commit: Duration,
    roll_at: u64,
    last_sync: Instant,
    dirty: bool,
}

/// Store usage, for `fernet log --size`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Bytes in log segments.
    pub log: u64,
    /// Bytes in blobs (compressed).
    pub objects: u64,
    /// Number of blobs.
    pub blobs: u64,
    /// Bytes in the index.
    pub index: u64,
}

impl Store {
    /// Whether `root` has a store.
    #[must_use]
    pub fn exists(root: &Path) -> bool {
        root.join(DIR).join("FORMAT").is_file()
    }

    /// Creates `.fernet/` under `root` and opens it for writing.
    ///
    /// # Errors
    ///
    /// [`Error::Exists`] if there is one already; I/O errors.
    pub fn init(root: &Path) -> Result<Self> {
        let dir = root.join(DIR);
        if dir.exists() {
            return Err(Error::Exists(dir));
        }
        for sub in ["", "log", "objects", "index"] {
            let d = dir.join(sub);
            fs::create_dir_all(&d).map_err(|e| Error::io(&d, e))?;
        }
        let first = dir.join("log").join(segment::file_name(1));
        write_atomic(&first, &segment::header(1))?;
        write_atomic(&dir.join("config.toml"), DEFAULT_CONFIG.as_bytes())?;
        write_manifest(&dir, &[1])?;
        // FORMAT last: a store without it is incomplete and isn't opened.
        write_atomic(&dir.join("FORMAT"), FORMAT_LINE.as_bytes())?;
        sync_dir(root)?;
        Self::open(root, Mode::Write)
    }

    /// Opens the store under `root`.
    ///
    /// A torn final record in the active segment is truncated when the lock
    /// is held (and ignored otherwise). Damage anywhere else is an error;
    /// `verify` describes it.
    ///
    /// # Errors
    ///
    /// [`Error::NotAStore`], [`Error::Format`], [`Error::Busy`] (write mode),
    /// [`Error::Corrupt`], I/O and index errors.
    pub fn open(root: &Path, mode: Mode) -> Result<Self> {
        let dir = root.join(DIR);
        let format_path = dir.join("FORMAT");
        let format = match fs::read_to_string(&format_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotAStore(root.to_owned()));
            }
            Err(e) => return Err(Error::io(&format_path, e)),
        };
        if format != FORMAT_LINE {
            return Err(Error::Format(format.trim().to_owned()));
        }
        let lock = take_lock(&dir, mode)?;
        let segments = read_manifest(&dir)?;
        let last = *segments
            .last()
            .ok_or(Error::Layout("manifest lists no segments"))?;

        // Check the active segment's tail.
        let last_path = dir.join("log").join(segment::file_name(last));
        let bytes = fs::read(&last_path).map_err(|e| Error::io(&last_path, e))?;
        let scan = segment::scan(&bytes, last);
        match scan.problem {
            Some(segment::Problem::Record { at_tail: true, .. }) if lock.is_some() => {
                tracing::debug!(
                    segment = last,
                    at = scan.valid_end,
                    "truncating torn record"
                );
                let f = OpenOptions::new()
                    .write(true)
                    .open(&last_path)
                    .map_err(|e| Error::io(&last_path, e))?;
                f.set_len(scan.valid_end)
                    .map_err(|e| Error::io(&last_path, e))?;
                f.sync_all().map_err(|e| Error::io(&last_path, e))?;
            }
            // Without the lock a torn tail may be a write in progress: ignore it.
            None | Some(segment::Problem::Record { at_tail: true, .. }) => {}
            Some(segment::Problem::Record { offset, .. }) => {
                return Err(Error::Corrupt {
                    segment: last,
                    offset,
                });
            }
            Some(segment::Problem::Header) => {
                return Err(Error::Corrupt {
                    segment: last,
                    offset: 0,
                });
            }
        }
        let end = Position {
            segment: last,
            offset: scan.valid_end,
            ord: 0,
        };

        let active = if mode == Mode::Write {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&last_path)
                .map_err(|e| Error::io(&last_path, e))?;
            let len = file
                .seek(SeekFrom::End(0))
                .map_err(|e| Error::io(&last_path, e))?;
            Some(Active {
                file,
                id: last,
                len,
            })
        } else {
            None
        };

        let mut store = Self {
            objects: Objects::new(dir.join("objects")),
            segments,
            active,
            index: Box::new(MemIndex::default()),
            lock,
            group_commit: read_group_commit(&dir),
            roll_at: config_u64(&dir, "segment_bytes").unwrap_or(segment::ROLL_AT),
            last_sync: Instant::now(),
            dirty: false,
            dir,
        };
        store.load_index(end)?;
        Ok(store)
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("index").join("fernet.redb")
    }

    /// Brings the index up to date with the log, rebuilding it if it's
    /// missing, from another format, or ahead of the log.
    fn load_index(&mut self, end: Position) -> Result<()> {
        if self.lock.is_none() {
            let mut mem = MemIndex::default();
            self.replay(&mut mem, Position::default(), end)?;
            self.index = Box::new(mem);
            return Ok(());
        }
        let path = self.index_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let mut disk = if let Some(d) = DiskIndex::open(&path)? {
            d
        } else {
            remove_index(&path)?;
            DiskIndex::open(&path)?.ok_or(Error::Index("can't create the index".into()))?
        };
        let pos = disk.position();
        let usable = pos == Position::default()
            || (self.segments.contains(&pos.segment)
                && (pos.segment < end.segment
                    || (pos.segment == end.segment && pos.offset <= end.offset)));
        if !usable || self.replay(&mut disk, pos, end).is_err() {
            drop(disk);
            remove_index(&path)?;
            disk = DiskIndex::open(&path)?.ok_or(Error::Index("can't create the index".into()))?;
            self.replay(&mut disk, Position::default(), end)?;
        }
        disk.persist()?;
        self.index = Box::new(disk);
        Ok(())
    }

    /// Applies every record from `from` up to `end` to `index`.
    fn replay(&self, index: &mut dyn Tables, from: Position, end: Position) -> Result<()> {
        let mut ord = from.ord;
        for &id in self.segments.iter().filter(|&&s| s >= from.segment) {
            let path = self.dir.join("log").join(segment::file_name(id));
            let bytes = fs::read(&path).map_err(|e| Error::io(&path, e))?;
            let limit = if id == end.segment {
                usize::try_from(end.offset).unwrap_or(usize::MAX)
            } else {
                bytes.len()
            };
            let start = if id == from.segment {
                usize::try_from(from.offset)
                    .unwrap_or(usize::MAX)
                    .max(segment::HEADER_LEN)
            } else {
                segment::HEADER_LEN
            };
            if bytes.get(..segment::HEADER_LEN) != Some(&segment::header(id)[..]) {
                return Err(Error::Corrupt {
                    segment: id,
                    offset: 0,
                });
            }
            let mut at = start;
            let mut batch = Vec::new();
            while at < limit.min(bytes.len()) {
                let (r, n) = record::decode(&bytes[at..limit]).map_err(|_| Error::Corrupt {
                    segment: id,
                    offset: at as u64,
                })?;
                batch.push((
                    VersionId {
                        segment: id,
                        offset: at as u64,
                    },
                    r,
                ));
                at += n;
            }
            ord += batch.len() as u64;
            index.apply(
                &batch,
                Position {
                    segment: id,
                    offset: at as u64,
                    ord,
                },
            )?;
            if id == end.segment {
                break;
            }
        }
        Ok(())
    }

    /// Drops the index and rebuilds it from the log. Returns the number of
    /// records indexed.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] without the lock; log and index errors.
    pub fn reindex(&mut self) -> Result<u64> {
        if self.lock.is_none() {
            return Err(Error::ReadOnly);
        }
        let end = self.log_end()?;
        self.index = Box::new(MemIndex::default());
        let path = self.index_path();
        remove_index(&path)?;
        self.load_index(end)?;
        Ok(self.index.position().ord)
    }

    fn log_end(&self) -> Result<Position> {
        let last = *self
            .segments
            .last()
            .ok_or(Error::Layout("manifest lists no segments"))?;
        let path = self.dir.join("log").join(segment::file_name(last));
        let bytes = fs::read(&path).map_err(|e| Error::io(&path, e))?;
        Ok(Position {
            segment: last,
            offset: segment::scan(&bytes, last).valid_end,
            ord: 0,
        })
    }

    /// Stores a blob (deduplicated) and returns its hash. Durable on return.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`]; I/O errors.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<Hash> {
        if self.active.is_none() {
            return Err(Error::ReadOnly);
        }
        self.objects.put(bytes)
    }

    /// The content of a version.
    ///
    /// # Errors
    ///
    /// [`Error::BadBlob`] for tombstones and missing or damaged blobs.
    pub fn read(&self, v: &Version) -> Result<Vec<u8>> {
        let hash = v.hash.ok_or(Error::BadBlob { hash: Hash::ZERO })?;
        self.objects.get(&hash, v.size)
    }

    /// Appends a record. The blob it names must already be stored. Records
    /// are fsynced in groups: at most every 50 ms by default, and always on
    /// [`Store::flush`] and drop.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`], [`Error::TooLong`], I/O and index errors.
    pub fn append(&mut self, record: &Record) -> Result<VersionId> {
        let bytes = record.encode()?;
        if self.active.as_ref().is_some_and(|a| {
            a.len > segment::HEADER_LEN as u64 && a.len + bytes.len() as u64 > self.roll_at
        }) {
            self.roll()?;
        }
        let path_of = |id| self.dir.join("log").join(segment::file_name(id));
        let active = self.active.as_mut().ok_or(Error::ReadOnly)?;
        let path = path_of(active.id);
        active
            .file
            .write_all(&bytes)
            .map_err(|e| Error::io(&path, e))?;
        let id = VersionId {
            segment: active.id,
            offset: active.len,
        };
        active.len += bytes.len() as u64;
        let next = Position {
            segment: active.id,
            offset: active.len,
            ord: self.index.position().ord + 1,
        };
        self.dirty = true;
        if self.last_sync.elapsed() >= self.group_commit {
            self.flush()?;
        }
        self.index.apply(&[(id, record.clone())], next)?;
        Ok(id)
    }

    /// Starts a new segment: create it with its header, fsync, then swap the
    /// manifest. A crash in between leaves an unlisted segment, which is
    /// ignored.
    fn roll(&mut self) -> Result<()> {
        self.flush()?;
        let id = self.segments.last().copied().unwrap_or(0) + 1;
        let path = self.dir.join("log").join(segment::file_name(id));
        write_atomic(&path, &segment::header(id))?;
        let mut segments = self.segments.clone();
        segments.push(id);
        write_manifest(&self.dir, &segments)?;
        self.segments = segments;
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| Error::io(&path, e))?;
        self.active = Some(Active {
            file,
            id,
            len: segment::HEADER_LEN as u64,
        });
        Ok(())
    }

    /// Fsyncs pending records and persists the index.
    ///
    /// # Errors
    ///
    /// I/O and index errors.
    pub fn flush(&mut self) -> Result<()> {
        if self.dirty {
            if let Some(a) = &self.active {
                a.file.sync_data().map_err(|e| Error::io(&self.dir, e))?;
            }
            self.dirty = false;
        }
        self.last_sync = Instant::now();
        self.index.persist()
    }

    /// Whether this handle can write.
    #[must_use]
    pub fn writable(&self) -> bool {
        self.active.is_some()
    }

    /// Every version of `path`, oldest first, tombstones included.
    ///
    /// # Errors
    ///
    /// Index errors.
    pub fn versions(&self, path: &str) -> Result<Vec<Version>> {
        self.index.versions(path)
    }

    /// The latest record for `path` at or before `time`, tombstones included.
    ///
    /// # Errors
    ///
    /// Index errors.
    pub fn version_at(&self, path: &str, time: i64) -> Result<Option<Version>> {
        Ok(self
            .index
            .versions(path)?
            .into_iter()
            .rev()
            .find(|v| v.time <= time))
    }

    /// Every path with history that starts with `prefix`, sorted.
    ///
    /// # Errors
    ///
    /// Index errors.
    pub fn paths(&self, prefix: &str) -> Result<Vec<String>> {
        self.index.paths(prefix)
    }

    /// Every mark and its time.
    ///
    /// # Errors
    ///
    /// Index errors.
    pub fn marks(&self) -> Result<BTreeMap<String, i64>> {
        self.index.marks()
    }

    /// Every record in the log, in order.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`], I/O errors.
    pub fn records(&self) -> Result<Vec<(VersionId, Record)>> {
        let mut mem = Recorder::default();
        let end = self.log_end()?;
        self.replay(&mut mem, Position::default(), end)?;
        Ok(mem.0)
    }

    /// Disk usage.
    ///
    /// # Errors
    ///
    /// I/O errors.
    pub fn usage(&self) -> Result<Usage> {
        let mut u = Usage::default();
        for id in &self.segments {
            let p = self.dir.join("log").join(segment::file_name(*id));
            u.log += fs::metadata(&p).map_err(|e| Error::io(&p, e))?.len();
        }
        for h in self.objects.all()? {
            u.blobs += 1;
            u.objects += fs::metadata(self.objects.path(&h)).map_or(0, |m| m.len());
        }
        u.index = fs::metadata(self.index_path()).map_or(0, |m| m.len());
        Ok(u)
    }

    /// The `.fernet` directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether a blob is stored.
    #[must_use]
    pub fn has_blob(&self, hash: &Hash) -> bool {
        self.objects.contains(hash)
    }

    /// Live segment ids, from the manifest.
    #[must_use]
    pub fn segments(&self) -> &[u64] {
        &self.segments
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Collects records during a replay.
#[derive(Default)]
struct Recorder(Vec<(VersionId, Record)>);

impl Tables for Recorder {
    fn versions(&self, _: &str) -> Result<Vec<Version>> {
        Ok(Vec::new())
    }
    fn paths(&self, _: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn marks(&self) -> Result<BTreeMap<String, i64>> {
        Ok(BTreeMap::new())
    }
    fn position(&self) -> Position {
        Position::default()
    }
    fn apply(&mut self, records: &[(VersionId, Record)], _: Position) -> Result<()> {
        self.0.extend_from_slice(records);
        Ok(())
    }
    fn persist(&mut self) -> Result<()> {
        Ok(())
    }
}

fn remove_index(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(path, e)),
    }
}

fn take_lock(dir: &Path, mode: Mode) -> Result<Option<File>> {
    let path = dir.join("lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| Error::io(&path, e))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) if mode == Mode::Read => Ok(None),
        Err(fs::TryLockError::WouldBlock) => Err(Error::Busy),
        Err(fs::TryLockError::Error(e)) => Err(Error::io(&path, e)),
    }
}

pub(crate) fn read_manifest(dir: &Path) -> Result<Vec<u64>> {
    let path = dir.join("MANIFEST");
    let text = fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
    let mut lines = text.lines();
    if lines.next() != Some(MANIFEST_HEADER) {
        return Err(Error::Layout("manifest header"));
    }
    let ids: Vec<u64> = lines
        .map(|l| l.parse().map_err(|_| Error::Layout("manifest entry")))
        .collect::<Result<_>>()?;
    if ids.is_empty() || ids.windows(2).any(|w| w[0] >= w[1]) {
        return Err(Error::Layout("manifest ids must be ascending"));
    }
    Ok(ids)
}

pub(crate) fn write_manifest(dir: &Path, ids: &[u64]) -> Result<()> {
    use fmt::Write as _;
    let mut text = format!("{MANIFEST_HEADER}\n");
    for id in ids {
        let _ = writeln!(text, "{id}"); // Writing to a String can't fail.
    }
    write_atomic(&dir.join("MANIFEST"), text.as_bytes())
}

/// The default `config.toml`.
pub const DEFAULT_CONFIG: &str = "\
# fernet store settings.

[log]
# Records are fsynced in groups, at most this often.
group_commit_ms = 50

# Segments roll over at this size.
segment_bytes = 67108864

[capture]
# A path is captured once it has been quiet this long (mount and watch).
debounce_ms = 2000

[limits]
# Files larger than this aren't captured.
max_file_size = 52428800

[ignore]
# Extra gitignore-style patterns, on top of .fernetignore and the defaults
# (.git/, node_modules/, target/, .DS_Store).
patterns = []
";

fn read_group_commit(dir: &Path) -> Duration {
    config_u64(dir, "group_commit_ms").map_or(GROUP_COMMIT, Duration::from_millis)
}

/// Reads one integer setting from `config.toml` with a line scan; the file is
/// ours and simple. Unknown or malformed settings fall back to defaults.
#[must_use]
pub fn config_u64(dir: &Path, key: &str) -> Option<u64> {
    let text = fs::read_to_string(dir.join("config.toml")).ok()?;
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().replace('_', "").parse().ok())?
    })
}
