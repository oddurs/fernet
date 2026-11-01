//! The fernet resolver: path grammar + store + format drivers → nodes.
//!
//! Both front doors (the FUSE mount and the CLI) resolve paths through this
//! crate, so every path resolves identically through either (invariant 7).
//! The primitive is [`Fernet::lookup`], one component at a time, which is how
//! the kernel resolves paths; [`Fernet::resolve`] is a fold over it.
//!
//! Time lenses need a store (`.fernet/`); without one they resolve to
//! [`Error::NoHistory`].
#![forbid(unsafe_code)]

mod cache;
mod error;
mod history;
pub mod time;
mod write;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use fernet_fs_formats::{Registry, Tree};
use fernet_fs_path::{Component, FernetPath, TimeSpec, decode_token, encode_token};
use fernet_fs_store::{Mode, Store, Version};
use jiff::Timestamp;
use jiff::tz::TimeZone;

pub use cache::TreeCache;
pub use error::{Error, Result};
pub use fernet_fs_formats::{NewValue, NodeKind, ScalarType};
pub use history::{DEFAULT_IGNORES, DEFAULT_MAX_FILE, Ignore, Stratum, TreeCapture};
pub use time::Target;

/// The name of the store directory, hidden from listings at the root.
pub const STORE_DIR: &str = ".fernet";

/// Whether `name` is the store directory's name. On macOS the default
/// filesystem ignores case, so `.FERNET` is the store too.
#[must_use]
pub fn is_store_name(name: &OsStr) -> bool {
    if cfg!(target_os = "macos") {
        name.to_str()
            .is_some_and(|n| n.eq_ignore_ascii_case(STORE_DIR))
    } else {
        name == STORE_DIR
    }
}

/// Longest name a listing will show, in bytes (`NAME_MAX` on Linux and macOS).
pub const NAME_MAX: usize = 255;

/// A clock: the current instant.
pub type Clock = Box<dyn Fn() -> Timestamp + Send + Sync>;

/// A fernet root: a backing directory, its drivers and its store.
pub struct Fernet {
    root: PathBuf,
    registry: Registry,
    cache: TreeCache,
    store: Option<Mutex<Store>>,
    clock: Clock,
    tz: TimeZone,
    ignore: Ignore,
    locks: write::FileLocks,
}

/// How to open a root.
pub struct Options {
    /// How to open the store, if there is one.
    pub mode: Mode,
    /// The clock; default the system clock.
    pub clock: Clock,
    /// The user's time zone; default the system zone (`TZ` is honoured).
    pub tz: TimeZone,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::Read,
            clock: Box::new(Timestamp::now),
            tz: TimeZone::system(),
        }
    }
}

/// What a path resolves to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Resolved {
    /// A regular file (or anything that isn't a directory) in the backing
    /// directory. Symlinks are followed.
    RealFile {
        /// Absolute path in the backing directory.
        path: PathBuf,
    },
    /// A directory in the backing directory.
    RealDir {
        /// Absolute path in the backing directory.
        path: PathBuf,
    },
    /// A file as it was: one recorded version.
    PastFile {
        /// Absolute path the file had in the backing directory.
        path: PathBuf,
        /// The version.
        version: Version,
    },
    /// A directory as it was at an instant.
    PastDir {
        /// Absolute path in the backing directory.
        path: PathBuf,
        /// UTC nanoseconds.
        at: i64,
    },
    /// The directory of a file's kept versions (`notes.md@`).
    VersionListing {
        /// Absolute path of the file in the backing directory.
        path: PathBuf,
    },
    /// A node inside a structured file (`config.json#/server/port`).
    Inside {
        /// Where the file's bytes come from.
        source: Source,
        /// Decoded keys from the file's root.
        pointer: Vec<String>,
        /// What the node is.
        node: NodeKind,
    },
    /// Nothing by that name.
    Missing,
}

/// Where the bytes of an opened file come from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// A file in the backing directory, at the present.
    Present(PathBuf),
    /// A recorded version of a file.
    Past {
        /// The file's path, for format detection and messages.
        path: PathBuf,
        /// The version.
        version: Version,
    },
}

impl Source {
    /// The file's path in the backing directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Present(p) | Self::Past { path: p, .. } => p,
        }
    }
}

impl Resolved {
    /// True for things that list like directories.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        matches!(
            self,
            Self::RealDir { .. }
                | Self::PastDir { .. }
                | Self::VersionListing { .. }
                | Self::Inside {
                    node: NodeKind::Object | NodeKind::Array,
                    ..
                }
        )
    }

    /// True for anything in the past, which is read-only.
    #[must_use]
    pub fn is_past(&self) -> bool {
        matches!(
            self,
            Self::PastFile { .. }
                | Self::PastDir { .. }
                | Self::VersionListing { .. }
                | Self::Inside {
                    source: Source::Past { .. },
                    ..
                }
        )
    }
}

/// One entry of a listing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entry {
    /// The name as it would be looked up: real names verbatim, keys as
    /// encoded pointer tokens.
    pub name: OsString,
    /// File, directory, symlink or other.
    pub kind: EntryKind,
    /// Size in bytes: the real size, or a scalar's rendered size.
    pub size: u64,
}

/// Entry kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link (not followed in listings).
    Symlink,
    /// Anything else (sockets, fifos, devices).
    Other,
}

impl EntryKind {
    fn of(ft: fs::FileType) -> Self {
        if ft.is_symlink() {
            Self::Symlink
        } else if ft.is_dir() {
            Self::Dir
        } else if ft.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }
}

impl Fernet {
    /// Opens a root with default options: the store (if any) read-only, the
    /// system clock and time zone. Without `.fernet/` there is no history, but
    /// real paths and `#` lenses work.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if `root` can't be canonicalised or isn't a directory;
    /// store errors.
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with(root, Options::default())
    }

    /// Opens a root with explicit options.
    ///
    /// # Errors
    ///
    /// As [`Fernet::open`].
    pub fn open_with(root: &Path, options: Options) -> Result<Self> {
        let root = fs::canonicalize(root).map_err(|e| Error::io(root, e))?;
        if !root.is_dir() {
            return Err(Error::NotADirectory {
                path: root.display().to_string(),
            });
        }
        let store = if Store::exists(&root) {
            Some(Mutex::new(Store::open(&root, options.mode)?))
        } else {
            None
        };
        let ignore = Ignore::load(&root);
        Ok(Self {
            root,
            registry: Registry::default(),
            cache: TreeCache::default(),
            store,
            clock: options.clock,
            tz: options.tz,
            ignore,
            locks: write::FileLocks::default(),
        })
    }

    /// Creates a store under `root` and opens the root for writing.
    ///
    /// # Errors
    ///
    /// Store errors ([`fernet_fs_store::Error::Exists`] if there is one).
    pub fn init(root: &Path, options: Options) -> Result<Self> {
        drop(Store::init(root)?);
        Self::open_with(
            root,
            Options {
                mode: Mode::Write,
                ..options
            },
        )
    }

    /// The canonical root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The root as a [`Resolved`].
    #[must_use]
    pub fn root_node(&self) -> Resolved {
        Resolved::RealDir {
            path: self.root.clone(),
        }
    }

    /// The store, if there is one.
    pub fn store(&self) -> Option<MutexGuard<'_, Store>> {
        self.store
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn need_store(&self, path: &Path) -> Result<MutexGuard<'_, Store>> {
        self.store().ok_or_else(|| Error::NoHistory {
            path: self.display(path),
        })
    }

    /// The current instant.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        (self.clock)()
    }

    /// The user's time zone.
    #[must_use]
    pub fn tz(&self) -> &TimeZone {
        &self.tz
    }

    /// A local date-time for messages: `2026-09-14 09:30`.
    fn local_date(&self, nanos: i64) -> String {
        time::timestamp(nanos)
            .to_zoned(self.tz.clone())
            .strftime("%Y-%m-%d %H:%M")
            .to_string()
    }

    /// The ignore rules.
    #[must_use]
    pub fn ignore(&self) -> &Ignore {
        &self.ignore
    }

    /// Resolves one component under `parent`. This is the step the mount
    /// calls for every kernel lookup.
    ///
    /// Real names win: the backing directory (or, under a time lens, the
    /// directory as it was) is checked for `name` verbatim before any lens is
    /// read from it.
    ///
    /// # Errors
    ///
    /// [`Error::NoHistory`] for time lenses without a store,
    /// [`Error::NotADirectory`] under a file, [`Error::OutsideRoot`] for `..`
    /// at the root, [`Error::NoMark`], and parse, store or I/O errors.
    pub fn lookup(&self, parent: &Resolved, name: &OsStr) -> Result<Resolved> {
        match parent {
            Resolved::Missing => Ok(Resolved::Missing),
            Resolved::RealFile { path } | Resolved::PastFile { path, .. } => {
                Err(Error::NotADirectory {
                    path: self.display(path),
                })
            }
            Resolved::RealDir { path } => self.lookup_real(path, name),
            Resolved::PastDir { path, at } => self.lookup_past(path, *at, name),
            Resolved::VersionListing { path } => self.lookup_listing(path, name),
            Resolved::Inside {
                source,
                pointer,
                node,
            } => match node {
                NodeKind::Scalar(_) => Err(Error::NotADirectory {
                    path: self.display_inside(source, pointer),
                }),
                NodeKind::Object | NodeKind::Array => self.lookup_inside(source, pointer, name),
            },
        }
    }

    fn lookup_real(&self, dir: &Path, name: &OsStr) -> Result<Resolved> {
        if name == "." || name.is_empty() {
            return Ok(Resolved::RealDir {
                path: dir.to_owned(),
            });
        }
        if name == ".." {
            return match dir.parent() {
                Some(up) if dir != self.root => Ok(Resolved::RealDir {
                    path: up.to_owned(),
                }),
                _ => Err(Error::OutsideRoot { path: "..".into() }),
            };
        }
        if dir == self.root && is_store_name(name) {
            return Ok(Resolved::Missing);
        }
        let real = dir.join(name);
        if fs::symlink_metadata(&real).is_ok() {
            return Ok(real_node(real));
        }
        // No real entry: read lenses. Lenses are UTF-8.
        let Some(text) = name.to_str() else {
            return Ok(Resolved::Missing);
        };
        let c = Component::parse(text);
        let target = dir.join(&c.name);
        if let Some(spec) = &c.when {
            let found = self.travel(&target, spec)?;
            return if c.open {
                self.open_lens(found)
            } else {
                Ok(found)
            };
        }
        if !c.open {
            return Ok(Resolved::Missing);
        }
        match fs::metadata(&target) {
            Ok(m) if m.is_file() => self.open_source(Source::Present(target)),
            _ => Ok(Resolved::Missing),
        }
    }

    /// Opens a past file with the `#` lens.
    fn open_lens(&self, r: Resolved) -> Result<Resolved> {
        match r {
            Resolved::PastFile { path, version } => {
                self.open_source(Source::Past { path, version })
            }
            _ => Ok(Resolved::Missing),
        }
    }

    fn open_source(&self, source: Source) -> Result<Resolved> {
        let Some(tree) = self.tree(&source)? else {
            return Ok(Resolved::Missing);
        };
        let node = tree.node(tree.root).kind;
        Ok(Resolved::Inside {
            source,
            pointer: Vec::new(),
            node,
        })
    }

    /// The store path of a backing path: relative, `/`-separated, UTF-8.
    pub(crate) fn rel(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
        Some(parts?.join("/"))
    }

    /// Applies a time lens to `path` (a file or directory, present or not).
    fn travel(&self, path: &Path, spec: &TimeSpec) -> Result<Resolved> {
        let store = self.need_store(path)?;
        let Some(rel) = self.rel(path) else {
            return Ok(Resolved::Missing);
        };
        let target = time::resolve(spec, self.now(), &self.tz, &store.marks()?)?;
        let kept: Vec<Version> = store
            .versions(&rel)?
            .into_iter()
            .filter(|v| v.hash.is_some())
            .collect();
        let file = |v: Option<&Version>| match v {
            Some(v) => Resolved::PastFile {
                path: path.to_owned(),
                version: v.clone(),
            },
            None => Resolved::Missing,
        };
        Ok(match target {
            Target::At(t) => match history::entry_at(&store, path, &rel, t)? {
                // The file has history, but none that early: say so.
                Resolved::Missing if kept.first().is_some_and(|v| v.time > t) => {
                    return Err(Error::NoVersion {
                        path: self.display(path),
                        before: self.local_date(t),
                        first: kept
                            .first()
                            .map(|v| self.local_date(v.time))
                            .unwrap_or_default(),
                    });
                }
                found => found,
            },
            Target::Back(n) => file(kept.iter().rev().nth(n as usize)),
            Target::First if !kept.is_empty() => file(kept.first()),
            Target::First => match history::first_under(&store, &rel)? {
                Some(t) => Resolved::PastDir {
                    path: path.to_owned(),
                    at: t,
                },
                None => Resolved::Missing,
            },
            Target::Listing if kept.is_empty() => Resolved::Missing,
            Target::Listing => Resolved::VersionListing {
                path: path.to_owned(),
            },
        })
    }

    /// Looks up `name` in a directory as it was at `at`. Names that existed
    /// then win; then `#` opens a past file. A second time lens is literal.
    fn lookup_past(&self, dir: &Path, at: i64, name: &OsStr) -> Result<Resolved> {
        if name == "." || name.is_empty() {
            return Ok(Resolved::PastDir {
                path: dir.to_owned(),
                at,
            });
        }
        if name == ".." {
            return match dir.parent() {
                Some(up) if dir != self.root => Ok(Resolved::PastDir {
                    path: up.to_owned(),
                    at,
                }),
                _ => Err(Error::OutsideRoot { path: "..".into() }),
            };
        }
        let Some(text) = name.to_str() else {
            return Ok(Resolved::Missing);
        };
        let store = self.need_store(dir)?;
        let child = dir.join(text);
        let Some(rel) = self.rel(&child) else {
            return Ok(Resolved::Missing);
        };
        let found = history::entry_at(&store, &child, &rel, at)?;
        if found != Resolved::Missing {
            return Ok(found);
        }
        let c = Component::parse(text);
        if !c.open || c.when.is_some() {
            return Ok(Resolved::Missing);
        }
        let file = dir.join(&c.name);
        let Some(rel) = self.rel(&file) else {
            return Ok(Resolved::Missing);
        };
        let found = history::entry_at(&store, &file, &rel, at)?;
        drop(store);
        self.open_lens(found)
    }

    fn lookup_listing(&self, path: &Path, name: &OsStr) -> Result<Resolved> {
        let Some(text) = name.to_str() else {
            return Ok(Resolved::Missing);
        };
        let (text, open) = match text.strip_suffix('#') {
            Some(t) => (t, true),
            None => (text, false),
        };
        let store = self.need_store(path)?;
        let Some(rel) = self.rel(path) else {
            return Ok(Resolved::Missing);
        };
        let entries = history::listing(&store, &rel)?;
        drop(store);
        let Some((_, version)) = entries.into_iter().find(|(n, _)| n == text) else {
            return Ok(Resolved::Missing);
        };
        let found = Resolved::PastFile {
            path: path.to_owned(),
            version,
        };
        if open {
            self.open_lens(found)
        } else {
            Ok(found)
        }
    }

    fn lookup_inside(&self, source: &Source, pointer: &[String], name: &OsStr) -> Result<Resolved> {
        let Some(key) = name.to_str().and_then(decode_token) else {
            return Ok(Resolved::Missing);
        };
        let Some(tree) = self.tree(source)? else {
            return Ok(Resolved::Missing);
        };
        let mut pointer = pointer.to_vec();
        pointer.push(key);
        Ok(match tree.get(&pointer) {
            Some(id) => Resolved::Inside {
                source: source.clone(),
                node: tree.node(id).kind,
                pointer,
            },
            None => Resolved::Missing,
        })
    }

    /// Resolves a parsed path from the root: a fold of [`Fernet::lookup`] over
    /// its components, printed back to their exact text.
    ///
    /// # Errors
    ///
    /// As [`Fernet::lookup`], plus [`Error::NotFound`] naming the first
    /// component that doesn't exist.
    pub fn resolve(&self, path: &FernetPath) -> Result<Resolved> {
        let names: Vec<OsString> = path
            .components
            .iter()
            .map(|c| c.to_string().into())
            .collect();
        self.resolve_names(names.iter().map(OsString::as_os_str))
    }

    /// Resolves a sequence of raw names from the root. Unlike
    /// [`Fernet::resolve`], names needn't be UTF-8 (real names can be anything
    /// but `/` and NUL).
    ///
    /// # Errors
    ///
    /// As [`Fernet::resolve`].
    pub fn resolve_names<'a>(
        &self,
        names: impl IntoIterator<Item = &'a OsStr>,
    ) -> Result<Resolved> {
        let mut at = self.root_node();
        let mut walked = PathBuf::new();
        for name in names {
            let next = self.lookup(&at, name)?;
            if next == Resolved::Missing {
                return Err(Error::NotFound {
                    parent: walked.display().to_string(),
                    name: name.to_string_lossy().into_owned(),
                    inside: matches!(at, Resolved::Inside { .. }),
                });
            }
            walked.push(name);
            at = next;
        }
        Ok(at)
    }

    /// Splits a path typed on the command line (relative to `cwd`, or
    /// absolute) into names below the root. `.` and `..` apply lexically.
    ///
    /// # Errors
    ///
    /// [`Error::OutsideRoot`].
    pub fn user_names(&self, cwd: &Path, arg: &OsStr) -> Result<Vec<OsString>> {
        let joined = cwd.join(arg);
        let mut parts: Vec<&OsStr> = Vec::new();
        for c in joined.components() {
            match c {
                PathComponent::Prefix(_) | PathComponent::RootDir | PathComponent::CurDir => {}
                PathComponent::ParentDir => {
                    parts.pop();
                }
                PathComponent::Normal(n) => parts.push(n),
            }
        }
        let root_parts: Vec<&OsStr> = self
            .root
            .components()
            .filter_map(|c| match c {
                PathComponent::Normal(n) => Some(n),
                _ => None,
            })
            .collect();
        if parts.len() < root_parts.len() || parts[..root_parts.len()] != root_parts[..] {
            return Err(Error::OutsideRoot {
                path: arg.to_string_lossy().into_owned(),
            });
        }
        Ok(parts[root_parts.len()..]
            .iter()
            .map(|p| (*p).to_owned())
            .collect())
    }

    /// Resolves a path given the way a user types it on the command line.
    ///
    /// # Errors
    ///
    /// [`Error::OutsideRoot`], or as [`Fernet::resolve_names`].
    pub fn resolve_user_path(&self, cwd: &Path, arg: &OsStr) -> Result<Resolved> {
        let names = self.user_names(cwd, arg)?;
        self.resolve_names(names.iter().map(OsString::as_os_str))
    }

    /// The bytes of a file, past file or value.
    ///
    /// # Errors
    ///
    /// [`Error::IsADirectory`] for directories, [`Error::NotFound`] for
    /// [`Resolved::Missing`], store, I/O and parse errors.
    pub fn read(&self, r: &Resolved) -> Result<Vec<u8>> {
        match r {
            Resolved::RealFile { path } => fs::read(path).map_err(|e| Error::io(path, e)),
            Resolved::RealDir { path }
            | Resolved::PastDir { path, .. }
            | Resolved::VersionListing { path } => Err(Error::IsADirectory {
                path: self.display(path),
            }),
            Resolved::PastFile { path, version } => Ok(self.need_store(path)?.read(version)?),
            Resolved::Inside {
                source, pointer, ..
            } => {
                let tree = self
                    .tree(source)?
                    .ok_or_else(|| self.gone(source, pointer))?;
                let id = tree
                    .get(pointer)
                    .ok_or_else(|| self.gone(source, pointer))?;
                tree.render(id).ok_or_else(|| Error::IsADirectory {
                    path: self.display_inside(source, pointer),
                })
            }
            Resolved::Missing => Err(missing()),
        }
    }

    /// The entries of a directory. Real directories list exactly their real
    /// entries (minus `.fernet/` at the root), with real sizes and types,
    /// sorted by name (invariant 3). Past directories list what existed then.
    /// Version listings list versions oldest first, then marks. Opened files
    /// list keys in source order, as encoded pointer tokens; keys no path can
    /// name are left out.
    ///
    /// # Errors
    ///
    /// [`Error::NotADirectory`] for files and scalars; store, I/O and parse
    /// errors.
    pub fn list(&self, r: &Resolved) -> Result<Vec<Entry>> {
        match r {
            Resolved::RealDir { path } => {
                let mut out = Vec::new();
                for entry in fs::read_dir(path).map_err(|e| Error::io(path, e))? {
                    let entry = entry.map_err(|e| Error::io(path, e))?;
                    let name = entry.file_name();
                    if path == &self.root && is_store_name(&name) {
                        continue;
                    }
                    let meta = fs::symlink_metadata(entry.path())
                        .map_err(|e| Error::io(&entry.path(), e))?;
                    out.push(Entry {
                        name,
                        kind: EntryKind::of(meta.file_type()),
                        size: meta.len(),
                    });
                }
                out.sort();
                Ok(out)
            }
            Resolved::PastDir { path, at } => {
                let store = self.need_store(path)?;
                let Some(rel) = self.rel(path) else {
                    return Ok(Vec::new());
                };
                history::children_at(&store, &rel, *at)
            }
            Resolved::VersionListing { path } => {
                let store = self.need_store(path)?;
                let Some(rel) = self.rel(path) else {
                    return Ok(Vec::new());
                };
                Ok(history::listing(&store, &rel)?
                    .into_iter()
                    .map(|(name, v)| Entry {
                        name: name.into(),
                        kind: EntryKind::File,
                        size: v.size,
                    })
                    .collect())
            }
            Resolved::Inside {
                source,
                pointer,
                node: NodeKind::Object | NodeKind::Array,
            } => {
                let tree = self
                    .tree(source)?
                    .ok_or_else(|| self.gone(source, pointer))?;
                let id = tree
                    .get(pointer)
                    .ok_or_else(|| self.gone(source, pointer))?;
                Ok(tree
                    .node(id)
                    .children
                    .iter()
                    .filter_map(|(key, child)| {
                        let token = encode_token(key).filter(|t| t.len() <= NAME_MAX)?;
                        let (kind, size) = match tree.render(*child) {
                            Some(bytes) => (EntryKind::File, bytes.len() as u64),
                            None => (EntryKind::Dir, 0),
                        };
                        Some(Entry {
                            name: token.into(),
                            kind,
                            size,
                        })
                    })
                    .collect())
            }
            Resolved::RealFile { path } | Resolved::PastFile { path, .. } => {
                Err(Error::NotADirectory {
                    path: self.display(path),
                })
            }
            Resolved::Inside {
                source, pointer, ..
            } => Err(Error::NotADirectory {
                path: self.display_inside(source, pointer),
            }),
            Resolved::Missing => Err(missing()),
        }
    }

    /// The parsed tree for `source`, or `None` if no driver handles it.
    fn tree(&self, source: &Source) -> Result<Option<Arc<Tree>>> {
        let key = match source {
            Source::Present(path) => {
                let meta = fs::metadata(path).map_err(|e| Error::io(path, e))?;
                cache::Key::present(path, &meta)
            }
            Source::Past { path, version } => cache::Key::Blob {
                path: path.clone(),
                hash: version.hash.unwrap_or_default().0,
            },
        };
        if let Some(hit) = self.cache.get(&key) {
            return Ok(Some(hit));
        }
        let path = source.path();
        let bytes = match source {
            Source::Present(path) => fs::read(path).map_err(|e| Error::io(path, e))?,
            Source::Past { path, version } => self.need_store(path)?.read(version)?,
        };
        Ok(self.parse_bytes(path, &bytes)?.inspect(|tree| {
            self.cache.insert(key, Arc::clone(tree), bytes.len());
        }))
    }

    /// Parses `bytes` with the driver for `path`; `None` if there is none.
    pub(crate) fn parse_bytes(&self, path: &Path, bytes: &[u8]) -> Result<Option<Arc<Tree>>> {
        let head = &bytes[..bytes.len().min(fernet_fs_formats::SNIFF_LEN)];
        let Some(driver) = self.registry.detect(path, head) else {
            tracing::debug!(path = %path.display(), "no driver");
            return Ok(None);
        };
        let tree = driver.parse(bytes).map_err(|source| Error::Parse {
            path: self.display(path),
            driver: driver.name(),
            source,
        })?;
        Ok(Some(Arc::new(tree)))
    }

    fn gone(&self, source: &Source, pointer: &[String]) -> Error {
        Error::NotFound {
            parent: String::new(),
            name: self.display_inside(source, pointer),
            inside: true,
        }
    }

    /// `path` relative to the root, for messages.
    pub(crate) fn display(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        if rel.as_os_str().is_empty() {
            ".".into()
        } else {
            rel.display().to_string()
        }
    }

    fn display_inside(&self, source: &Source, pointer: &[String]) -> String {
        let mut s = match source {
            Source::Present(path) => format!("{}#", self.display(path)),
            Source::Past { path, version } => {
                format!("{}@{}#", self.display(path), time::basic_name(version.time))
            }
        };
        for key in pointer {
            s.push('/');
            s.push_str(&encode_token(key).unwrap_or_default());
        }
        s
    }
}

fn missing() -> Error {
    Error::NotFound {
        parent: String::new(),
        name: String::new(),
        inside: false,
    }
}

fn real_node(path: PathBuf) -> Resolved {
    // Follow symlinks: a link to a directory is walked like one.
    if path.is_dir() {
        Resolved::RealDir { path }
    } else {
        Resolved::RealFile { path }
    }
}
