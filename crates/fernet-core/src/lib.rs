//! The fernet resolver: path grammar + store + format drivers → nodes.
//!
//! Both front doors (the FUSE mount and the CLI) resolve paths through this
//! crate, so every path resolves identically through either (invariant 7).
//! The primitive is [`Fernet::lookup`], one component at a time, which is how
//! the kernel resolves paths; [`Fernet::resolve`] is a fold over it.
//!
//! Milestone 1 resolves real paths and `#` lenses. Time lenses need the store
//! (milestone 2) and resolve to [`Error::NoHistory`] until then.
#![forbid(unsafe_code)]

mod cache;
mod error;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::sync::Arc;

use fernet_fs_formats::{NodeKind, Registry, Tree};
use fernet_fs_path::{Component, FernetPath, decode_token, encode_token};

pub use cache::TreeCache;
pub use error::{Error, Result};

/// The name of the store directory, hidden from listings at the root.
pub const STORE_DIR: &str = ".fernet";

/// Longest name a listing will show, in bytes (`NAME_MAX` on Linux and macOS).
pub const NAME_MAX: usize = 255;

/// A fernet root: a backing directory, its drivers and (later) its store.
pub struct Fernet {
    root: PathBuf,
    registry: Registry,
    cache: TreeCache,
}

/// What a path resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
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
}

impl Resolved {
    /// True for things that list like directories.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        matches!(
            self,
            Self::RealDir { .. }
                | Self::Inside {
                    node: NodeKind::Object | NodeKind::Array,
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
    /// Opens a root. A missing `.fernet/` is fine: without a store there is no
    /// history, but real paths and `#` lenses work.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if `root` can't be canonicalised or isn't a directory.
    pub fn open(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root).map_err(|e| Error::io(root, e))?;
        if !root.is_dir() {
            return Err(Error::NotADirectory {
                path: root.display().to_string(),
            });
        }
        Ok(Self {
            root,
            registry: Registry::default(),
            cache: TreeCache::default(),
        })
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

    /// Resolves one component under `parent`. This is the step the mount
    /// calls for every kernel lookup.
    ///
    /// Real names win: the backing directory is checked for `name` verbatim
    /// before any lens is read from it.
    ///
    /// # Errors
    ///
    /// [`Error::NoHistory`] for time lenses (until milestone 2),
    /// [`Error::NotADirectory`] under a file, [`Error::OutsideRoot`] for `..`
    /// at the root, and parse or I/O errors from opening a file.
    pub fn lookup(&self, parent: &Resolved, name: &OsStr) -> Result<Resolved> {
        match parent {
            Resolved::Missing => Ok(Resolved::Missing),
            Resolved::RealFile { path } => Err(Error::NotADirectory {
                path: self.display(path),
            }),
            Resolved::RealDir { path } => self.lookup_real(path, name),
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
        if dir == self.root && name == STORE_DIR {
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
        if c.when.is_some() {
            return Err(Error::NoHistory {
                path: self.display(&dir.join(text)),
            });
        }
        if !c.open {
            return Ok(Resolved::Missing);
        }
        let file = dir.join(&c.name);
        match fs::metadata(&file) {
            Ok(m) if m.is_file() => {}
            _ => return Ok(Resolved::Missing),
        }
        let source = Source::Present(file);
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

    /// Resolves a path given the way a user types it on the command line:
    /// relative to `cwd`, or absolute. `.` and `..` are applied lexically, and
    /// the result must lie inside the root.
    ///
    /// # Errors
    ///
    /// [`Error::OutsideRoot`], or as [`Fernet::resolve_names`].
    pub fn resolve_user_path(&self, cwd: &Path, arg: &OsStr) -> Result<Resolved> {
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
        self.resolve_names(parts[root_parts.len()..].iter().copied())
    }

    /// The bytes of a file or value.
    ///
    /// # Errors
    ///
    /// [`Error::IsADirectory`] for directories, [`Error::NotFound`] for
    /// [`Resolved::Missing`], I/O and parse errors.
    pub fn read(&self, r: &Resolved) -> Result<Vec<u8>> {
        match r {
            Resolved::RealFile { path } => fs::read(path).map_err(|e| Error::io(path, e)),
            Resolved::RealDir { path } => Err(Error::IsADirectory {
                path: self.display(path),
            }),
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
            Resolved::Missing => Err(Error::NotFound {
                parent: String::new(),
                name: String::new(),
                inside: false,
            }),
        }
    }

    /// The entries of a directory, sorted by name. Real directories list
    /// exactly their real entries (minus `.fernet/` at the root), with real
    /// sizes and types (invariant 3). Opened files list keys in source order,
    /// as encoded pointer tokens; keys no path can name are left out.
    ///
    /// # Errors
    ///
    /// [`Error::NotADirectory`] for files and scalars, I/O and parse errors.
    pub fn list(&self, r: &Resolved) -> Result<Vec<Entry>> {
        match r {
            Resolved::RealDir { path } => {
                let mut out = Vec::new();
                for entry in fs::read_dir(path).map_err(|e| Error::io(path, e))? {
                    let entry = entry.map_err(|e| Error::io(path, e))?;
                    let name = entry.file_name();
                    if path == &self.root && name == STORE_DIR {
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
            Resolved::RealFile { path } => Err(Error::NotADirectory {
                path: self.display(path),
            }),
            Resolved::Inside {
                source, pointer, ..
            } => Err(Error::NotADirectory {
                path: self.display_inside(source, pointer),
            }),
            Resolved::Missing => Err(Error::NotFound {
                parent: String::new(),
                name: String::new(),
                inside: false,
            }),
        }
    }

    /// The parsed tree for `source`, or `None` if no driver handles it.
    fn tree(&self, source: &Source) -> Result<Option<Arc<Tree>>> {
        let Source::Present(path) = source;
        let meta = fs::metadata(path).map_err(|e| Error::io(path, e))?;
        let key = cache::Key::present(path, &meta);
        if let Some(hit) = self.cache.get(&key) {
            return Ok(Some(hit));
        }
        let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
        let head = &bytes[..bytes.len().min(fernet_fs_formats::SNIFF_LEN)];
        let Some(driver) = self.registry.detect(path, head) else {
            tracing::debug!(path = %path.display(), "no driver");
            return Ok(None);
        };
        let tree = driver.parse(&bytes).map_err(|source| Error::Parse {
            path: self.display(path),
            driver: driver.name(),
            source,
        })?;
        let tree = Arc::new(tree);
        self.cache.insert(key, Arc::clone(&tree), bytes.len());
        Ok(Some(tree))
    }

    fn gone(&self, source: &Source, pointer: &[String]) -> Error {
        Error::NotFound {
            parent: String::new(),
            name: self.display_inside(source, pointer),
            inside: true,
        }
    }

    /// `path` relative to the root, for messages.
    fn display(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        if rel.as_os_str().is_empty() {
            ".".into()
        } else {
            rel.display().to_string()
        }
    }

    fn display_inside(&self, source: &Source, pointer: &[String]) -> String {
        let Source::Present(path) = source;
        let mut s = format!("{}#", self.display(path));
        for key in pointer {
            s.push('/');
            s.push_str(&encode_token(key).unwrap_or_default());
        }
        s
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
