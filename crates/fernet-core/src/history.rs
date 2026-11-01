//! History: capturing snapshots and reading the past.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use fernet_fs_path::{FernetPath, MarkName};
use fernet_fs_store::{Config, Hash, Kind, Record, Store, Version, VersionId};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::time::{basic_name, nanos};
use crate::{Entry, EntryKind, Error, Fernet, Resolved, Result, STORE_DIR, Source, is_store_name};

/// Default ignore patterns (brief 4.4), plus editors' temporary files
/// (brief 4.6): vim swap and backup files and its `4913` probe, Emacs lock
/// files, `*.tmp` (fernet's own temp files too) and `JetBrains` safe-write files.
pub const DEFAULT_IGNORES: [&str; 12] = [
    ".git/",
    "node_modules/",
    "target/",
    ".DS_Store",
    "*.swp",
    "*.swx",
    "*~",
    ".#*",
    "*.tmp",
    "4913",
    "*___jb_tmp___",
    "*___jb_old___",
];

/// Default size limit for captured files: 50 MiB.
pub const DEFAULT_MAX_FILE: u64 = 50 << 20;

/// What isn't captured: `.fernetignore` (gitignore syntax), the defaults, and
/// files over the size limit. `.fernet/` itself is never captured.
pub struct Ignore {
    root: PathBuf,
    rules: Gitignore,
    max_file: u64,
}

impl Ignore {
    /// Loads the rules for `root`. Malformed patterns are skipped (and
    /// logged), never fatal.
    #[must_use]
    pub fn load(root: &Path) -> Self {
        let mut b = GitignoreBuilder::new(root);
        for p in DEFAULT_IGNORES {
            let _ = b.add_line(None, p);
        }
        if let Some(e) = b.add(root.join(".fernetignore")) {
            tracing::debug!(error = %e, ".fernetignore");
        }
        // Problems in config.toml are reported by the store when it opens.
        let config = Config::load(&root.join(STORE_DIR));
        for p in config.patterns() {
            let _ = b.add_line(None, &p);
        }
        let rules = b.build().unwrap_or_else(|_| Gitignore::empty());
        let max_file = config.u64("max_file_size").unwrap_or(DEFAULT_MAX_FILE);
        Self {
            root: root.to_owned(),
            rules,
            max_file,
        }
    }

    /// Whether `path` (absolute, under the root) is ignored.
    ///
    /// Gitignore semantics, the same as the tree walk: a path under an
    /// ignored directory is ignored even if a later `!pattern` names it,
    /// because the walk never enters that directory.
    #[must_use]
    pub fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return true;
        };
        if rel
            .components()
            .next()
            .is_some_and(|c| is_store_name(c.as_os_str()))
        {
            return true;
        }
        // Ancestors from the top down, as the walk meets them.
        let mut ancestors: Vec<&Path> = rel
            .ancestors()
            .skip(1)
            .filter(|a| !a.as_os_str().is_empty())
            .collect();
        ancestors.reverse();
        ancestors
            .into_iter()
            .any(|a| self.rules.matched(a, true).is_ignore())
            || self.rules.matched(rel, is_dir).is_ignore()
    }

    /// Files larger than this aren't captured.
    #[must_use]
    pub fn max_file(&self) -> u64 {
        self.max_file
    }
}

/// One layer of a core sample: a value and when it appeared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stratum {
    /// When this value first appeared (UTC nanoseconds): the oldest version
    /// in the run of equal values.
    pub since: i64,
    /// The newest version in the run.
    pub version: VersionId,
    /// The value's bytes, or `None` where the file or key didn't exist.
    pub value: Option<Vec<u8>>,
}

fn tombstone(rel: &str, time: i64) -> Record {
    Record {
        kind: Kind::Delete,
        time,
        path: rel.to_owned(),
        hash: Hash::ZERO,
        size: 0,
        extra: String::new(),
    }
}

/// What's on disk at a path, for deciding whether its history ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OnDisk {
    /// A regular file, reached without passing through a symlink.
    File,
    /// Nothing, something other than a regular file, or a file reached only
    /// through a symlinked directory (captures never follow links).
    Gone,
    /// Can't tell (permission denied and the like): leave history alone.
    Unknown,
}

/// Whether `err` means the path isn't there (a missing component, or a
/// component that's now a file).
fn is_gone(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Whether `path`'s parent directory is reached through a symlink: its
/// canonical location differs from its spelling (the root is canonical).
/// `None` when that can't be told.
fn behind_link(path: &Path) -> Option<bool> {
    let parent = path.parent()?;
    match fs::canonicalize(parent) {
        Ok(c) => Some(c != parent),
        Err(e) if is_gone(&e) => Some(false),
        Err(_) => None,
    }
}

fn on_disk(path: &Path) -> OnDisk {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => match behind_link(path) {
            Some(false) => OnDisk::File,
            Some(true) => OnDisk::Gone,
            None => OnDisk::Unknown,
        },
        Ok(_) => OnDisk::Gone,
        Err(e) if is_gone(&e) => OnDisk::Gone,
        Err(_) => OnDisk::Unknown,
    }
}

/// What a tree capture recorded, and what it couldn't read.
#[derive(Debug, Default)]
pub struct TreeCapture {
    /// Versions and tombstones recorded.
    pub captured: usize,
    /// Paths skipped because they couldn't be read, with the reason. Their
    /// history is left as it was.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Tombstones every live path under `rel/` whose file is gone from disk.
/// Returns the first record written.
fn tombstone_under(
    store: &mut Store,
    root: &Path,
    rel: &str,
    time: i64,
) -> Result<Option<VersionId>> {
    let mut first = None;
    for p in store.paths(&format!("{rel}/"))? {
        let Some(v) = store.version_at(&p, i64::MAX)?.filter(|v| v.hash.is_some()) else {
            continue;
        };
        if on_disk(&root.join(&p)) != OnDisk::Gone {
            continue;
        }
        let id = store.append(&tombstone(&p, time.max(v.time)))?;
        first.get_or_insert(id);
    }
    Ok(first)
}

/// What exists at `rel` at instant `t`: a file, a directory (some path below
/// it existed), or nothing.
pub(crate) fn entry_at(store: &Store, path: &Path, rel: &str, t: i64) -> Result<Resolved> {
    if let Some(v) = store.version_at(rel, t)?
        && v.hash.is_some()
    {
        return Ok(Resolved::PastFile {
            path: path.to_owned(),
            version: v,
        });
    }
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    for p in store.paths(&prefix)? {
        if store.version_at(&p, t)?.is_some_and(|v| v.hash.is_some()) {
            return Ok(Resolved::PastDir {
                path: path.to_owned(),
                at: t,
            });
        }
    }
    Ok(Resolved::Missing)
}

/// The earliest time anything under `rel/` was recorded.
pub(crate) fn first_under(store: &Store, rel: &str) -> Result<Option<i64>> {
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    let mut first: Option<i64> = None;
    for p in store.paths(&prefix)? {
        if let Some(v) = store.versions(&p)?.into_iter().find(|v| v.hash.is_some()) {
            first = Some(first.map_or(v.time, |f| f.min(v.time)));
        }
    }
    Ok(first)
}

/// The entries of directory `rel` at `t`, sorted by name.
pub(crate) fn children_at(store: &Store, rel: &str, t: i64) -> Result<Vec<Entry>> {
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    let mut out: BTreeMap<String, Entry> = BTreeMap::new();
    for p in store.paths(&prefix)? {
        let Some(v) = store.version_at(&p, t)?.filter(|v| v.hash.is_some()) else {
            continue;
        };
        let rest = &p[prefix.len()..];
        let (name, entry) = match rest.split_once('/') {
            Some((dir, _)) => (
                dir,
                Entry {
                    name: dir.into(),
                    kind: EntryKind::Dir,
                    size: 0,
                },
            ),
            None => (
                rest,
                Entry {
                    name: rest.into(),
                    kind: EntryKind::File,
                    size: v.size,
                },
            ),
        };
        out.entry(name.to_owned()).or_insert(entry);
    }
    Ok(out.into_values().collect())
}

/// The version listing of `rel`: kept versions oldest first, named in ISO 8601
/// basic UTC (a `.N` suffix separates versions within the same second), then
/// one entry per mark covering the file.
pub(crate) fn listing(store: &Store, rel: &str) -> Result<Vec<(String, Version)>> {
    let kept: Vec<Version> = store
        .versions(rel)?
        .into_iter()
        .filter(|v| v.hash.is_some())
        .collect();
    let mut out: Vec<(String, Version)> = Vec::with_capacity(kept.len());
    let mut last = String::new();
    let mut n = 0;
    for v in &kept {
        let base = basic_name(v.time);
        if base == last {
            n += 1;
            out.push((format!("{base}.{n}"), v.clone()));
        } else {
            n = 0;
            out.push((base.clone(), v.clone()));
            last = base;
        }
    }
    for (mark, t) in store.marks()? {
        if let Some(v) = store.version_at(rel, t)?.filter(|v| v.hash.is_some()) {
            out.push((mark, v));
        }
    }
    Ok(out)
}

impl Fernet {
    fn writable_store(&self, path: &Path) -> Result<std::sync::MutexGuard<'_, Store>> {
        let store = self.store().ok_or_else(|| Error::NoHistory {
            path: self.display(path),
        })?;
        if store.writable() {
            Ok(store)
        } else {
            Err(Error::ReadOnly {
                path: STORE_DIR.into(),
            })
        }
    }

    /// Captures the current state of one real file. Returns the new version,
    /// or `None` when nothing changed (identical bytes are never stored twice),
    /// the path is ignored, too large, or not UTF-8. A missing file records a
    /// tombstone if it had history.
    ///
    /// # Errors
    ///
    /// [`Error::NoHistory`], [`Error::ReadOnly`], store and I/O errors.
    pub fn snapshot(&self, real_path: &Path) -> Result<Option<VersionId>> {
        self.snapshot_at(real_path, nanos(self.now()))
    }

    /// [`Fernet::snapshot`] with an explicit record time.
    fn snapshot_at(&self, real_path: &Path, time: i64) -> Result<Option<VersionId>> {
        let path = if real_path.is_absolute() {
            real_path.to_owned()
        } else {
            self.root().join(real_path)
        };
        let mut store = self.writable_store(&path)?;
        let Some(rel) = self.rel(&path) else {
            return Ok(None);
        };
        if rel.is_empty() || self.ignore().ignored(&path, false) {
            return Ok(None);
        }
        let latest = store.version_at(&rel, i64::MAX)?;
        let live = latest.as_ref().is_some_and(|v| v.hash.is_some());
        // Never record behind the path's latest record: a tree capture stamps
        // a time taken before its walk, and a concurrent write may be newer.
        let time = latest.as_ref().map_or(time, |v| time.max(v.time));
        // Gone, or not a regular file (a directory, symlink or other), or
        // reached through a symlinked directory, which captures never follow
        // (it may point outside the root, or into the store): it ended, and so
        // did anything that lived under it.
        let ended = |store: &mut Store| -> Result<Option<VersionId>> {
            let first = if live {
                Some(store.append(&tombstone(&rel, time))?)
            } else {
                None
            };
            let under = tombstone_under(store, self.root(), &rel, time)?;
            Ok(first.or(under))
        };
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if is_gone(&e) => return ended(&mut store),
            Err(e) => return Err(Error::io(&path, e)),
        };
        match behind_link(&path) {
            Some(false) => {}
            Some(true) => return ended(&mut store),
            None => {
                return Err(Error::io(
                    &path,
                    std::io::Error::other("can't resolve the parent directory"),
                ));
            }
        }
        if !meta.is_file() {
            return ended(&mut store);
        }
        // A directory replaced by this file: whatever lived under it ended.
        let under = tombstone_under(&mut store, self.root(), &rel, time)?;
        if meta.len() > self.ignore().max_file() {
            return Ok(under);
        }
        let bytes = fs::read(&path).map_err(|e| Error::io(&path, e))?;
        let hash = Hash::of(&bytes);
        if latest.as_ref().and_then(|v| v.hash) == Some(hash) {
            return Ok(under);
        }
        store.put_blob(&bytes)?;
        let r = Record {
            kind: Kind::Write,
            time,
            path: rel,
            hash,
            size: bytes.len() as u64,
            extra: String::new(),
        };
        Ok(Some(store.append(&r)?))
    }

    /// Captures every file under `dir` (and records tombstones for files with
    /// history that are gone) as one moment: every record gets the same time,
    /// so the tree's past state is never half-captured. Returns how many
    /// versions were recorded. Unreadable paths are skipped (and logged); see
    /// [`Fernet::capture_tree`] to get them.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn snapshot_tree(&self, dir: &Path) -> Result<usize> {
        let report = self.capture_tree(dir)?;
        for (path, why) in &report.skipped {
            tracing::warn!(path = %path.display(), error = %why, "skipped");
        }
        Ok(report.captured)
    }

    /// [`Fernet::snapshot_tree`], reporting what couldn't be read. Symlinks
    /// are never followed: a `dir` that is a symlink is captured as a
    /// non-file (its history, if any, ends).
    ///
    /// # Errors
    ///
    /// Store errors. I/O errors on single paths are skipped and reported.
    pub fn capture_tree(&self, dir: &Path) -> Result<TreeCapture> {
        let time = nanos(self.now());
        let mut report = TreeCapture::default();
        let real_dir = fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir());
        if !real_dir && dir != self.root() {
            let r = self.snapshot_at(dir, time);
            tally(&mut report, r, dir)?;
            self.flush()?;
            return Ok(report);
        }
        let mut files = Vec::new();
        walk(self, dir, &mut files, &mut report.skipped);
        for f in &files {
            let r = self.snapshot_at(f, time);
            tally(&mut report, r, f)?;
        }
        // Tombstones for vanished files.
        let gone: Vec<PathBuf> = {
            let store = self.writable_store(dir)?;
            let prefix = match self.rel(dir) {
                Some(r) if r.is_empty() => String::new(),
                Some(r) => format!("{r}/"),
                None => return Ok(report),
            };
            let mut gone = Vec::new();
            for p in store.paths(&prefix)? {
                let alive = store.versions(&p)?.last().is_some_and(|v| v.hash.is_some());
                let path = self.root().join(&p);
                // Gone, or no longer a regular file.
                if alive && on_disk(&path) == OnDisk::Gone {
                    gone.push(path);
                }
            }
            gone
        };
        for g in gone {
            let r = self.snapshot_at(&g, time);
            tally(&mut report, r, &g)?;
        }
        self.flush()?;
        Ok(report)
    }

    /// Makes recorded history durable now.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn flush(&self) -> Result<()> {
        if let Some(mut store) = self.store()
            && store.writable()
        {
            store.flush()?;
        }
        Ok(())
    }

    /// Adds a tephra mark at the current instant.
    ///
    /// # Errors
    ///
    /// [`Error::Store`] with a mark error for invalid or existing names.
    pub fn mark(&self, name: &str) -> Result<()> {
        let mark = MarkName::new(name).map_err(|e| fernet_fs_store::Error::Mark(e.to_string()))?;
        let mut store = self.writable_store(self.root())?;
        if store.marks()?.contains_key(mark.as_str()) {
            return Err(fernet_fs_store::Error::Mark(format!(
                "a mark called {mark} already exists"
            ))
            .into());
        }
        let r = Record {
            kind: Kind::Mark,
            time: nanos(self.now()),
            path: String::new(),
            hash: Hash::ZERO,
            size: 0,
            extra: mark.to_string(),
        };
        store.append(&r)?;
        store.flush()?;
        Ok(())
    }

    /// Removes a mark.
    ///
    /// # Errors
    ///
    /// [`Error::NoMark`]; store errors.
    pub fn unmark(&self, name: &str) -> Result<()> {
        let mut store = self.writable_store(self.root())?;
        if !store.marks()?.contains_key(name) {
            return Err(Error::NoMark {
                name: name.to_owned(),
            });
        }
        let r = Record {
            kind: Kind::Unmark,
            time: nanos(self.now()),
            path: String::new(),
            hash: Hash::ZERO,
            size: 0,
            extra: name.to_owned(),
        };
        store.append(&r)?;
        store.flush()?;
        Ok(())
    }

    /// The history of one file or value, newest first, with consecutive equal
    /// values collapsed into one stratum.
    ///
    /// # Errors
    ///
    /// [`Error::NoHistory`]; resolution, store and parse errors.
    pub fn core_sample(&self, path: &FernetPath) -> Result<Vec<Stratum>> {
        let (file, pointer) = match self.resolve(path)? {
            Resolved::RealFile { path } | Resolved::PastFile { path, .. } => (path, Vec::new()),
            Resolved::Inside {
                source, pointer, ..
            } => (source.path().to_owned(), pointer),
            Resolved::RealDir { path }
            | Resolved::PastDir { path, .. }
            | Resolved::VersionListing { path } => {
                return Err(Error::IsADirectory {
                    path: self.display(&path),
                });
            }
            Resolved::Missing => return Err(crate::missing()),
        };
        let store = self.store().ok_or_else(|| Error::NoHistory {
            path: self.display(&file),
        })?;
        let Some(rel) = self.rel(&file) else {
            return Ok(Vec::new());
        };
        let versions = store.versions(&rel)?;
        let mut values = Vec::with_capacity(versions.len());
        for v in versions.iter().rev() {
            let value = match v.hash {
                None => None,
                Some(_) if pointer.is_empty() => Some(store.read(v)?),
                Some(_) => {
                    let bytes = store.read(v)?;
                    // A version that doesn't parse, or lacks the key, has no value.
                    self.parse_bytes(&file, &bytes)
                        .ok()
                        .flatten()
                        .and_then(|tree| {
                            let id = tree.get(&pointer)?;
                            tree.render(id)
                        })
                }
            };
            values.push((v, value));
        }
        let mut strata: Vec<Stratum> = Vec::new();
        for (v, value) in values {
            match strata.last_mut() {
                Some(s) if s.value == value => s.since = v.time,
                _ => strata.push(Stratum {
                    since: v.time,
                    version: v.id,
                    value,
                }),
            }
        }
        Ok(strata)
    }

    /// The source file and pointer a path names, for tools.
    #[must_use]
    pub fn source_of(r: &Resolved) -> Option<(&Path, &[String])> {
        match r {
            Resolved::Inside {
                source: Source::Present(p) | Source::Past { path: p, .. },
                pointer,
                ..
            } => Some((p, pointer)),
            _ => None,
        }
    }
}

/// Counts one capture in `report`; an I/O error on the path is a skip, any
/// other error is returned.
fn tally(report: &mut TreeCapture, r: Result<Option<VersionId>>, path: &Path) -> Result<()> {
    match r {
        Ok(v) => {
            report.captured += usize::from(v.is_some());
            Ok(())
        }
        Err(Error::Io { source, .. }) => {
            report
                .skipped
                .push((path.to_owned(), source.to_string().to_lowercase()));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Every non-ignored regular file under `dir`, never following symlinks.
/// Directories that can't be read go in `skipped`.
fn walk(f: &Fernet, dir: &Path, out: &mut Vec<PathBuf>, skipped: &mut Vec<(PathBuf, String)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if is_gone(&e) => return,
        Err(e) => {
            skipped.push((dir.to_owned(), e.to_string().to_lowercase()));
            return;
        }
    };
    let mut names: Vec<(OsString, PathBuf)> = Vec::new();
    for e in entries {
        match e {
            Ok(e) => names.push((e.file_name(), e.path())),
            Err(e) => skipped.push((dir.to_owned(), e.to_string().to_lowercase())),
        }
    }
    names.sort();
    for (_, path) in names {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if f.ignore().ignored(&path, meta.is_dir()) {
            continue;
        }
        if meta.is_dir() {
            walk(f, &path, out, skipped);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}
