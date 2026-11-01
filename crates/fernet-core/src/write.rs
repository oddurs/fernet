//! Writes through `#`: byte splices applied atomically to present files.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use fernet_fs_formats::{DriverError, Edit, NewValue};
use fernet_fs_store::Hash;

use crate::{Error, Fernet, Resolved, Result, Source};

/// Per-file locks, so two writers in this process never interleave on one
/// file.
#[derive(Default)]
pub(crate) struct FileLocks(Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>);

impl FileLocks {
    fn get(&self, path: &Path) -> Arc<Mutex<()>> {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(map.entry(path.to_owned()).or_default())
    }
}

/// What identifies a file's content for the changed-on-disk check.
fn fingerprint(path: &Path) -> Result<(Option<SystemTime>, u64, Hash, Vec<u8>)> {
    let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
    let meta = fs::metadata(path).map_err(|e| Error::io(path, e))?;
    Ok((meta.modified().ok(), meta.len(), Hash::of(&bytes), bytes))
}

impl Fernet {
    /// Sets the value at a `#` path. The text must fit the existing type
    /// (strings take anything); exactly one trailing newline is stripped.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] in the past, [`Error::InvalidValue`] for a value
    /// of the wrong type, [`Error::Unsupported`], [`Error::Changed`], I/O and
    /// store errors.
    pub fn write_value(&self, r: &Resolved, text: &str) -> Result<()> {
        let (path, pointer) = self.present_inside(r)?;
        self.edit(&path, &pointer, &Edit::SetScalar(text.to_owned()))
    }

    /// Adds `key` under the object or array at `r`.
    ///
    /// # Errors
    ///
    /// As [`Fernet::write_value`], plus [`Error::Exists`].
    pub fn insert(&self, r: &Resolved, key: &str, value: NewValue) -> Result<()> {
        let (path, pointer) = self.present_inside(r)?;
        self.edit(
            &path,
            &pointer,
            &Edit::Insert {
                key: key.to_owned(),
                value,
            },
        )
    }

    /// Removes the key at `r`.
    ///
    /// # Errors
    ///
    /// As [`Fernet::write_value`].
    pub fn remove(&self, r: &Resolved) -> Result<()> {
        let (path, pointer) = self.present_inside(r)?;
        if pointer.is_empty() {
            return Err(Error::Unsupported {
                path: self.display(&path),
                message: "can't remove a whole file through #".into(),
            });
        }
        self.edit(&path, &pointer, &Edit::Remove)
    }

    /// Replaces a real file's content atomically, then snapshots it.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] in the past; I/O and store errors.
    pub fn write_file(&self, r: &Resolved, bytes: &[u8]) -> Result<()> {
        let path = match r {
            Resolved::RealFile { path } => path.clone(),
            r if r.is_past() => return Err(self.read_only(r)),
            _ => {
                return Err(Error::IsADirectory {
                    path: self.display(self.root()),
                });
            }
        };
        let path = self.write_target(&path)?;
        let lock = self.locks.get(&path);
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        replace_file(&path, bytes)?;
        self.capture(&path)
    }

    /// Creates the real file `name` in the directory `dir` (which must not
    /// have it yet), then snapshots it. The directory is checked the way
    /// [`Fernet::write_file`] checks a file: symlinks resolved, inside the
    /// root, outside the store.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] outside the root or inside the store; I/O and
    /// store errors.
    pub fn create_file(&self, dir: &Path, name: &std::ffi::OsStr, bytes: &[u8]) -> Result<()> {
        let dir = self.write_target(dir)?;
        let path = dir.join(name);
        if self.in_store(&path) {
            return Err(self.store_refusal(&path));
        }
        let lock = self.locks.get(&path);
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        replace_file(&path, bytes)?;
        self.capture(&path)
    }

    /// Where a write to `path` lands: symlinks resolved (replacing the link
    /// itself would leave the real file unchanged), inside the root and
    /// outside the store. The canonical path is also what the per-file lock
    /// is keyed on.
    fn write_target(&self, path: &Path) -> Result<PathBuf> {
        let real = fs::canonicalize(path).map_err(|e| Error::io(path, e))?;
        if !real.starts_with(self.root()) {
            return Err(Error::Unsupported {
                path: self.display(path),
                message: "it links outside the root; edit the target directly".into(),
            });
        }
        if self.in_store(&real) {
            return Err(self.store_refusal(path));
        }
        Ok(real)
    }

    /// Whether the canonical path `real` is the store or inside it.
    fn in_store(&self, real: &Path) -> bool {
        real.strip_prefix(self.root())
            .ok()
            .and_then(|rel| rel.components().next())
            .is_some_and(|c| crate::is_store_name(c.as_os_str()))
    }

    fn store_refusal(&self, path: &Path) -> Error {
        Error::Unsupported {
            path: self.display(path),
            message: "it is inside the store, which fernet never writes through".into(),
        }
    }

    /// What the driver for the file behind `r` can do, if it has one.
    #[must_use]
    pub fn capabilities(&self, r: &Resolved) -> Option<fernet_fs_formats::Caps> {
        let Resolved::Inside { source, .. } = r else {
            return None;
        };
        let path = source.path();
        let head = fs::read(path).ok()?;
        let head = &head[..head.len().min(fernet_fs_formats::SNIFF_LEN)];
        self.registry
            .detect(path, head)
            .map(fernet_fs_formats::Driver::capabilities)
    }

    fn read_only(&self, r: &Resolved) -> Error {
        let path = match r {
            Resolved::PastFile { path, .. }
            | Resolved::PastDir { path, .. }
            | Resolved::VersionListing { path } => path.clone(),
            Resolved::Inside { source, .. } => source.path().to_owned(),
            _ => self.root().to_owned(),
        };
        Error::ReadOnly {
            path: self.display(&path),
        }
    }

    /// The present file and pointer behind an `Inside` node.
    fn present_inside(&self, r: &Resolved) -> Result<(PathBuf, Vec<String>)> {
        match r {
            Resolved::Inside {
                source: Source::Present(path),
                pointer,
                ..
            } => Ok((path.clone(), pointer.clone())),
            r if r.is_past() => Err(self.read_only(r)),
            Resolved::Missing => Err(crate::missing()),
            _ => Err(Error::Unsupported {
                path: self.display(self.root()),
                message: "not a path inside a file".into(),
            }),
        }
    }

    /// Splices `path` under its lock: read, splice, write a temp file beside
    /// it, fsync, check the file didn't change meanwhile, rename over it,
    /// then snapshot. A change during the edit is retried once.
    fn edit(&self, path: &Path, pointer: &[String], edit: &Edit) -> Result<()> {
        let path = &self.write_target(path)?;
        let lock = self.locks.get(path);
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let shown = self.display(path);
        for _attempt in 0..2 {
            let (mtime, size, hash, bytes) = fingerprint(path)?;
            let head = &bytes[..bytes.len().min(fernet_fs_formats::SNIFF_LEN)];
            let driver = self
                .registry
                .detect(path, head)
                .ok_or_else(|| Error::Unsupported {
                    path: shown.clone(),
                    message: "no format driver for this file".into(),
                })?;
            let out = driver
                .splice(&bytes, pointer, edit.clone())
                .map_err(|e| driver_error(e, &shown, pointer))?;
            let tmp = temp_beside(path);
            write_temp(&tmp, path, &out)?;
            let (mtime2, size2, hash2, _) = fingerprint(path)?;
            if (mtime2, size2, hash2) != (mtime, size, hash) {
                let _ = fs::remove_file(&tmp);
                continue;
            }
            fs::rename(&tmp, path).map_err(|e| Error::io(path, e))?;
            sync_parent(path)?;
            return self.capture(path);
        }
        Err(Error::Changed { path: shown })
    }

    /// Snapshots after a write, if there is a writable store. Writes succeed
    /// without one: history is a bonus, the file is the truth.
    pub(crate) fn capture(&self, path: &Path) -> Result<()> {
        let writable = self.store().is_some_and(|s| s.writable());
        if writable {
            self.snapshot(path)?;
            self.flush()?;
        }
        Ok(())
    }
}

fn driver_error(e: DriverError, path: &str, pointer: &[String]) -> Error {
    let at = if pointer.is_empty() {
        format!("{path}#")
    } else {
        format!("{path}#/{}", pointer.join("/"))
    };
    match e {
        DriverError::InvalidValue(message) => Error::InvalidValue { path: at, message },
        DriverError::Exists => Error::Exists { path: at },
        DriverError::NotFound => Error::NotFound {
            parent: String::new(),
            name: at,
            inside: true,
        },
        DriverError::WrongKind(m) | DriverError::Unsupported(m) => Error::Unsupported {
            path: at,
            message: m.into(),
        },
        e @ (DriverError::Syntax { .. } | DriverError::NotUtf8 { .. } | DriverError::TooDeep) => {
            Error::Unsupported {
                path: at,
                message: e.to_string(),
            }
        }
    }
}

/// A fresh temp name beside `path`: unique per process and call, short
/// enough for `NAME_MAX` (the stem is cut to fit).
fn temp_beside(path: &Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut stem: String = name.chars().take(160).collect();
    while stem.len() > 160 {
        stem.pop();
    }
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(".{stem}.fernet-{}-{n}.tmp", std::process::id()))
}

/// Writes `bytes` to a new file `tmp` with `like`'s permissions, fsynced.
/// `create_new`: never truncates a file someone else is writing.
fn write_temp(tmp: &Path, like: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = File::options()
        .write(true)
        .create_new(true)
        .open(tmp)
        .map_err(|e| Error::io(tmp, e))?;
    f.write_all(bytes).map_err(|e| Error::io(tmp, e))?;
    if let Ok(meta) = fs::metadata(like) {
        f.set_permissions(meta.permissions())
            .map_err(|e| Error::io(tmp, e))?;
    }
    f.sync_all().map_err(|e| Error::io(tmp, e))
}

fn sync_parent(path: &Path) -> Result<()> {
    let dir = path.parent().unwrap_or(path);
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| Error::io(dir, e))
}

/// Atomically replaces `path` with `bytes`.
pub(crate) fn replace_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_beside(path);
    write_temp(&tmp, path, bytes)?;
    fs::rename(&tmp, path).map_err(|e| Error::io(path, e))?;
    sync_parent(path)
}
