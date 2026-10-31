//! FUSE adapter over `fernet-fs-core`.
//!
//! Every kernel lookup is one call to [`Fernet::lookup`], the same step the
//! CLI folds over a path, so the mount and the CLI resolve identically
//! (invariant 7). This crate holds no resolution logic of its own: it maps
//! resolved nodes to inodes, attributes and bytes, and routes writes.
//!
//! - Real files pass straight through to the backing directory. A handle that
//!   was written triggers a debounced snapshot when it's released.
//! - Values inside files (`#`) are rendered at `open`, edited in a per-handle
//!   buffer, and applied with [`Fernet::write_value`] on `flush`/`release`.
//!   `mkdir` inserts an empty object, `rm` removes a key, and creating a new
//!   name inserts it.
//! - Anything under `@` is read-only (`EROFS`).

mod capture;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use capture::{Capture, DEFAULT_WINDOW};
use fernet_fs_core::{Entry, EntryKind, Error, Fernet, NewValue, NodeKind, Resolved, Source};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
    WriteFlags,
};

/// How long the kernel may cache present-day entries and attributes.
const TTL_PRESENT: Duration = Duration::from_secs(1);
/// History never changes; cache it longer.
const TTL_PAST: Duration = Duration::from_secs(60);

/// The largest value a `#` handle buffers. Bigger writes or truncates get
/// `EFBIG` rather than an allocation the size of the request.
const MAX_VALUE: usize = 16 << 20;

#[cfg(target_os = "linux")]
const ESTALE: i32 = 116;
#[cfg(not(target_os = "linux"))]
const ESTALE: i32 = 70;

#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(not(target_os = "linux"))]
const O_NONBLOCK: i32 = 0x0004;

/// "No such attribute": `ENODATA` on Linux, `ENOATTR` (93) on macOS.
fn no_xattr() -> Errno {
    if cfg!(target_os = "linux") {
        Errno::from_i32(61)
    } else {
        Errno::from_i32(93)
    }
}

#[cfg(target_os = "linux")]
const O_TRUNC: i32 = 0o1000;
#[cfg(not(target_os = "linux"))]
const O_TRUNC: i32 = 0x0400;

/// The fernet filesystem.
pub struct FernetFs {
    fernet: Arc<Fernet>,
    capture: Option<Capture>,
    writable: bool,
    inodes: Mutex<Inodes>,
    handles: Mutex<HashMap<u64, Handle>>,
    /// Sizes of keys created but not yet written (`create` then `write`).
    pending: Mutex<HashMap<u64, usize>>,
    next_fh: AtomicU64,
    uid: u32,
    gid: u32,
    /// Kernel passthrough (Linux 6.9+, needs `CAP_SYS_ADMIN`): reads and
    /// writes of real files go straight to the backing file.
    passthrough: std::sync::atomic::AtomicBool,
    /// Backing ids per inode, shared by its open handles.
    backing: Mutex<HashMap<u64, (Arc<fuser::BackingId>, usize)>>,
    /// Whether the kernel passes `O_TRUNC` to open (no separate truncate).
    atomic_trunc: std::sync::atomic::AtomicBool,
}

enum Handle {
    /// A real file in the backing directory.
    Real {
        file: File,
        path: PathBuf,
        written: bool,
        ino: u64,
        passthrough: bool,
    },
    /// Rendered bytes of a read-only virtual file.
    Bytes(Arc<Vec<u8>>),
    /// A value inside a file, open for writing.
    /// `written` says whether `write` was called since open: a flush before
    /// any write (a shell's `dup2` then `close`) must not apply the buffer yet.
    Value {
        /// The value's inode: resolved again at apply time, so a rename of
        /// the file while the handle is open is followed.
        ino: u64,
        buf: Vec<u8>,
        dirty: bool,
        written: bool,
    },
    /// A new key, inserted once written (or on release if never written).
    NewKey {
        /// The containing object's inode.
        parent: u64,
        key: String,
        buf: Vec<u8>,
        ino: u64,
        inserted: bool,
        written: bool,
    },
    /// A directory listing, fixed at opendir.
    Dir(Vec<(u64, FileType, OsString)>),
}

struct Node {
    resolved: Resolved,
    parent: u64,
    lookups: u64,
    /// For real nodes, the backing file's (device, inode) when registered. A
    /// path whose file has since been replaced gets a new FUSE inode.
    ident: Option<(u64, u64)>,
}

/// Inodes keyed by resolved identity. Real paths keep their inode while the
/// same file lives there; a node whose path now names another file (replaced,
/// unlinked, renamed over) is orphaned: out of `by_key`, kept in `by_ino` until
/// the kernel forgets it.
struct Inodes {
    by_ino: HashMap<u64, Node>,
    by_key: HashMap<Resolved, u64>,
    next: u64,
}

/// The (device, inode) of a real path, not following symlinks.
fn ident_of(r: &Resolved) -> Option<(u64, u64)> {
    match r {
        Resolved::RealFile { path } | Resolved::RealDir { path } => {
            fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
        }
        _ => None,
    }
}

impl Inodes {
    /// The inode for `resolved`, registered without a lookup reference.
    fn get_or_insert(&mut self, resolved: Resolved, parent: u64) -> u64 {
        let ident = ident_of(&resolved);
        if let Some(&ino) = self.by_key.get(&resolved) {
            let same = self
                .by_ino
                .get(&ino)
                .is_some_and(|n| n.ident == ident || ident.is_none());
            if same {
                return ino;
            }
            self.orphan(&resolved);
        }
        let ino = self.next;
        self.next += 1;
        self.by_key.insert(resolved.clone(), ino);
        self.by_ino.insert(
            ino,
            Node {
                resolved,
                parent,
                lookups: 0,
                ident,
            },
        );
        ino
    }

    /// Detaches `key` from its inode (the file there is gone or replaced).
    /// The node lives on until the kernel forgets it.
    fn orphan(&mut self, key: &Resolved) {
        if let Some(ino) = self.by_key.remove(key)
            && let Some(n) = self.by_ino.get(&ino)
            && n.lookups == 0
            && ino != INodeNo::ROOT.0
        {
            self.by_ino.remove(&ino);
        }
    }

    /// After a rename, moves every real path under `from` to `to`, and every
    /// value inside a moved file, keeping inode numbers. A replaced target
    /// is orphaned.
    fn rename(&mut self, from: &Path, to: &Path) {
        let moved: Vec<(Resolved, u64)> = self
            .by_key
            .iter()
            .filter(|(k, _)| match k {
                Resolved::RealFile { path }
                | Resolved::RealDir { path }
                | Resolved::Inside {
                    source: Source::Present(path),
                    ..
                } => path.starts_with(from),
                _ => false,
            })
            .map(|(k, &ino)| (k.clone(), ino))
            .collect();
        for (key, _) in &moved {
            self.by_key.remove(key);
        }
        for (old, ino) in moved {
            let new = match old {
                Resolved::RealFile { path } => Resolved::RealFile {
                    path: rebase(&path, from, to),
                },
                Resolved::RealDir { path } => Resolved::RealDir {
                    path: rebase(&path, from, to),
                },
                Resolved::Inside {
                    source: Source::Present(path),
                    pointer,
                    node,
                } => Resolved::Inside {
                    source: Source::Present(rebase(&path, from, to)),
                    pointer,
                    node,
                },
                other => other,
            };
            self.orphan(&new);
            self.by_key.insert(new.clone(), ino);
            if let Some(n) = self.by_ino.get_mut(&ino) {
                n.resolved = new;
            }
        }
    }
}

fn rebase(path: &Path, from: &Path, to: &Path) -> PathBuf {
    match path.strip_prefix(from) {
        // `join("")` would add a trailing separator.
        Ok(rest) if rest.as_os_str().is_empty() => to.to_owned(),
        Ok(rest) => to.join(rest),
        Err(_) => path.to_owned(),
    }
}

fn is_real(r: &Resolved) -> bool {
    matches!(r, Resolved::RealFile { .. } | Resolved::RealDir { .. })
}

/// Maps a resolver error to an errno.
fn errno(e: &Error) -> Errno {
    Errno::from_i32(e.errno())
}

fn kind_of(ft: fs::FileType) -> FileType {
    if ft.is_symlink() {
        FileType::Symlink
    } else if ft.is_dir() {
        FileType::Directory
    } else if ft.is_fifo() {
        FileType::NamedPipe
    } else if ft.is_socket() {
        FileType::Socket
    } else if ft.is_char_device() {
        FileType::CharDevice
    } else if ft.is_block_device() {
        FileType::BlockDevice
    } else {
        FileType::RegularFile
    }
}

fn entry_kind(k: EntryKind) -> FileType {
    match k {
        EntryKind::Dir => FileType::Directory,
        EntryKind::Symlink => FileType::Symlink,
        EntryKind::File | EntryKind::Other => FileType::RegularFile,
    }
}

fn system_time(nanos: i64) -> SystemTime {
    match u64::try_from(nanos) {
        Ok(n) => UNIX_EPOCH + Duration::from_nanos(n),
        Err(_) => UNIX_EPOCH,
    }
}

/// Splices `data` into `buf` at `offset`, growing it with zeros as needed,
/// up to [`MAX_VALUE`].
fn write_into(buf: &mut Vec<u8>, offset: u64, data: &[u8]) -> Result<(), Errno> {
    let start = usize::try_from(offset).map_err(|_| Errno::EFBIG)?;
    let end = start
        .checked_add(data.len())
        .filter(|&e| e <= MAX_VALUE)
        .ok_or(Errno::EFBIG)?;
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[start..end].copy_from_slice(data);
    Ok(())
}

impl FernetFs {
    /// A read-only filesystem over `fernet`.
    #[must_use]
    pub fn new(fernet: Arc<Fernet>) -> Self {
        Self::build(fernet, None, false)
    }

    /// A writable filesystem over `fernet`, capturing history with `capture`.
    #[must_use]
    pub fn writable(fernet: Arc<Fernet>, capture: Capture) -> Self {
        Self::build(fernet, Some(capture), true)
    }

    fn build(fernet: Arc<Fernet>, capture: Option<Capture>, writable: bool) -> Self {
        let (uid, gid) = fs::metadata(fernet.root()).map_or((0, 0), |m| (m.uid(), m.gid()));
        let mut inodes = Inodes {
            by_ino: HashMap::new(),
            by_key: HashMap::new(),
            next: 2,
        };
        let root = fernet.root_node();
        inodes.by_key.insert(root.clone(), INodeNo::ROOT.0);
        inodes.by_ino.insert(
            INodeNo::ROOT.0,
            Node {
                resolved: root,
                parent: INodeNo::ROOT.0,
                lookups: 1,
                ident: None,
            },
        );
        Self {
            fernet,
            capture,
            writable,
            inodes: Mutex::new(inodes),
            handles: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            uid,
            gid,
            passthrough: std::sync::atomic::AtomicBool::new(false),
            backing: Mutex::new(HashMap::new()),
            atomic_trunc: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn node(&self, ino: INodeNo) -> Option<(Resolved, u64)> {
        let inodes = self.inodes.lock().unwrap_or_else(PoisonError::into_inner);
        inodes
            .by_ino
            .get(&ino.0)
            .map(|n| (n.resolved.clone(), n.parent))
    }

    /// Registers a node without a lookup reference.
    fn register(&self, r: Resolved, parent: u64) -> u64 {
        self.inodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(r, parent)
    }

    /// Takes one kernel lookup reference on `ino` (after a successful reply).
    fn add_ref(&self, ino: u64) {
        if let Some(n) = self
            .inodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .by_ino
            .get_mut(&ino)
        {
            n.lookups += 1;
        }
    }

    /// The (device, inode) a real node was registered with.
    fn ident(&self, ino: u64) -> Option<(u64, u64)> {
        self.inodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .by_ino
            .get(&ino)
            .and_then(|n| n.ident)
    }

    /// Orphans the inode of a real path that was just removed.
    fn forget_path(&self, path: &Path) {
        let mut inodes = self.inodes.lock().unwrap_or_else(PoisonError::into_inner);
        inodes.orphan(&Resolved::RealFile {
            path: path.to_owned(),
        });
        inodes.orphan(&Resolved::RealDir {
            path: path.to_owned(),
        });
    }

    fn touch(&self, path: &Path) {
        if let Some(c) = &self.capture
            && !self.fernet.ignore().ignored(path, false)
        {
            c.touch(path);
        }
    }

    fn virtual_attr(
        &self,
        ino: u64,
        kind: FileType,
        size: u64,
        mtime: SystemTime,
        writable: bool,
    ) -> FileAttr {
        let dir = kind == FileType::Directory;
        let perm = match (dir, writable) {
            (true, true) => 0o755,
            (true, false) => 0o555,
            (false, true) => 0o644,
            (false, false) => 0o444,
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm,
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// Attributes of a real file from its metadata.
    fn real_attr(ino: u64, m: &fs::Metadata) -> FileAttr {
        let t = |s: i64, ns: i64| system_time(s.saturating_mul(1_000_000_000).saturating_add(ns));
        FileAttr {
            ino: INodeNo(ino),
            size: m.len(),
            blocks: m.blocks(),
            atime: t(m.atime(), m.atime_nsec()),
            mtime: t(m.mtime(), m.mtime_nsec()),
            ctime: t(m.ctime(), m.ctime_nsec()),
            crtime: m.created().unwrap_or(UNIX_EPOCH),
            kind: kind_of(m.file_type()),
            #[allow(clippy::cast_possible_truncation)] // Permission bits fit in 12 bits.
            perm: (m.permissions().mode() & 0o7777) as u16,
            #[allow(clippy::cast_possible_truncation)] // Link counts beyond u32 aren't real.
            nlink: m.nlink() as u32,
            uid: m.uid(),
            gid: m.gid(),
            #[allow(clippy::cast_possible_truncation)] // Device numbers fit in u32 on Linux and macOS.
            rdev: m.rdev() as u32,
            #[allow(clippy::cast_possible_truncation)] // Block sizes are small.
            blksize: m.blksize() as u32,
            flags: 0,
        }
    }

    /// The backing file of an open real-file handle.
    fn handle_file(&self, fh: u64) -> Option<File> {
        let handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        match handles.get(&fh) {
            Some(Handle::Real { file, .. }) => file.try_clone().ok(),
            _ => None,
        }
    }

    /// Any open handle's file for real inode `ino` (`fstat` on an unlinked
    /// file arrives without a handle).
    fn any_open_file(&self, ino: u64) -> Option<File> {
        let handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        handles.values().find_map(|h| match h {
            Handle::Real { file, ino: i, .. } if *i == ino => file.try_clone().ok(),
            _ => None,
        })
    }

    /// Attributes and a cache lifetime for a node.
    fn attr(&self, ino: u64, r: &Resolved) -> Result<(FileAttr, Duration), Errno> {
        match r {
            Resolved::RealFile { path } | Resolved::RealDir { path } => {
                let m = fs::symlink_metadata(path).map_err(Errno::from)?;
                Ok((Self::real_attr(ino, &m), TTL_PRESENT))
            }
            Resolved::PastFile { version, .. } => Ok((
                self.virtual_attr(
                    ino,
                    FileType::RegularFile,
                    version.size,
                    system_time(version.time),
                    false,
                ),
                TTL_PAST,
            )),
            Resolved::PastDir { at, .. } => Ok((
                self.virtual_attr(ino, FileType::Directory, 0, system_time(*at), false),
                TTL_PAST,
            )),
            Resolved::VersionListing { path } => {
                let mtime = fs::metadata(path)
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                Ok((
                    self.virtual_attr(ino, FileType::Directory, 0, mtime, false),
                    TTL_PRESENT,
                ))
            }
            Resolved::Inside { source, node, .. } => {
                let (mtime, ttl, writable) = match source {
                    Source::Present(p) => (
                        fs::metadata(p)
                            .and_then(|m| m.modified())
                            .unwrap_or(UNIX_EPOCH),
                        TTL_PRESENT,
                        self.writable,
                    ),
                    Source::Past { version, .. } => (system_time(version.time), TTL_PAST, false),
                };
                if let NodeKind::Scalar(_) = node {
                    let size = match self.fernet.read(r) {
                        Ok(bytes) => bytes.len() as u64,
                        Err(e) => match self
                            .pending
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .get(&ino)
                        {
                            Some(&n) => n as u64,
                            None => return Err(errno(&e)),
                        },
                    };
                    Ok((
                        self.virtual_attr(ino, FileType::RegularFile, size, mtime, writable),
                        ttl,
                    ))
                } else {
                    Ok((
                        self.virtual_attr(ino, FileType::Directory, 0, mtime, writable),
                        ttl,
                    ))
                }
            }
            Resolved::Missing => Err(Errno::ENOENT),
        }
    }

    fn new_handle(&self, h: Handle) -> FileHandle {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(fh, h);
        FileHandle(fh)
    }

    /// An inode number for a listed entry, without taking a lookup reference.
    /// Real entries are registered (their inodes are stable); virtual ones
    /// use a known inode if there is one, else a placeholder, so listing a
    /// big file's keys doesn't grow the table.
    fn entry_ino(&self, dir: &Resolved, parent: u64, e: &Entry) -> u64 {
        match self.fernet.lookup(dir, &e.name) {
            Ok(r) if is_real(&r) => self.register(r, parent),
            Ok(r) if r != Resolved::Missing => {
                let inodes = self.inodes.lock().unwrap_or_else(PoisonError::into_inner);
                inodes.by_key.get(&r).copied().unwrap_or(u64::MAX)
            }
            // Any non-zero value will do: the kernel looks a name up before
            // using it.
            _ => u64::MAX,
        }
    }

    /// The parent directory for a namespace change (create, mkdir, unlink,
    /// rename): checks writability and refuses `.fernet` at the root.
    fn parent_for_change(&self, parent: INodeNo, name: &OsStr) -> Result<Resolved, Errno> {
        let Some((dir, _)) = self.node(parent) else {
            return Err(Errno::ENOENT);
        };
        if !self.writable || dir.is_past() {
            return Err(Errno::EROFS);
        }
        if parent == INodeNo::ROOT && fernet_fs_core::is_store_name(name) {
            return Err(Errno::EACCES);
        }
        Ok(dir)
    }

    fn entry_reply(&self, r: &Resolved, parent: u64, reply: ReplyEntry) {
        let ino = self.register(r.clone(), parent);
        match self.attr(ino, r) {
            Ok((attr, ttl)) => {
                // The reference is the kernel's only once it has the entry.
                self.add_ref(ino);
                reply.entry(&ttl, &attr, Generation(0));
            }
            Err(e) => reply.error(e),
        }
    }

    /// What an inode resolves to now (after any renames).
    fn current(&self, ino: u64) -> Option<Resolved> {
        self.node(INodeNo(ino)).map(|(r, _)| r)
    }

    /// Applies a buffered value or new key. On `flush` (`releasing` false)
    /// only after a write; on release whatever is pending. Returns an errno to
    /// report from `flush`/`close` when the write is refused.
    fn apply(&self, fh: u64, releasing: bool) -> Result<(), Errno> {
        let mut handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        match handles.get_mut(&fh) {
            Some(Handle::Value {
                ino,
                buf,
                dirty,
                written,
            }) if *dirty && (*written || releasing) => {
                let text = String::from_utf8(buf.clone()).map_err(|_| Errno::EINVAL)?;
                let ino = *ino;
                *dirty = false;
                drop(handles);
                let node = self.current(ino).ok_or(Errno::ENOENT)?;
                self.fernet.write_value(&node, &text).map_err(|e| errno(&e))
            }
            Some(Handle::NewKey {
                parent,
                key,
                buf,
                ino,
                inserted,
                written,
            }) if !*inserted && (*written || releasing) => {
                let text = String::from_utf8(buf.clone()).map_err(|_| Errno::EINVAL)?;
                let (parent, key, ino) = (*parent, key.clone(), *ino);
                *inserted = true;
                *written = false;
                drop(handles);
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&ino);
                let parent = self.current(parent).ok_or(Errno::ENOENT)?;
                self.fernet
                    .insert(&parent, &key, NewValue::Text(text))
                    .map_err(|e| errno(&e))
            }
            // More writes after the insert: update the new value in place.
            Some(Handle::NewKey {
                parent,
                key,
                buf,
                inserted: true,
                written,
                ..
            }) if *written => {
                let text = String::from_utf8(buf.clone()).map_err(|_| Errno::EINVAL)?;
                let (parent, name) = (
                    *parent,
                    fernet_fs_path::encode_token(key).unwrap_or_default(),
                );
                *written = false;
                drop(handles);
                let parent = self.current(parent).ok_or(Errno::ENOENT)?;
                let node = self
                    .fernet
                    .lookup(&parent, OsStr::new(&name))
                    .map_err(|e| errno(&e))?;
                self.fernet.write_value(&node, &text).map_err(|e| errno(&e))
            }
            _ => Ok(()),
        }
    }

    /// The passthrough backing id for `ino`, shared by its open handles.
    /// `None` when passthrough is off or the kernel refuses (then it's turned
    /// off for the rest of the mount).
    fn backing_for(
        &self,
        ino: u64,
        file: &File,
        open_backing: impl FnOnce(&File) -> std::io::Result<fuser::BackingId>,
    ) -> Option<Arc<fuser::BackingId>> {
        if !self.passthrough.load(Ordering::Relaxed) {
            return None;
        }
        let mut map = self.backing.lock().unwrap_or_else(PoisonError::into_inner);
        // Inodes are per file identity (see `Inodes`), so a cached backing id
        // always refers to the file this inode names.
        if let Some((id, count)) = map.get_mut(&ino) {
            *count += 1;
            return Some(Arc::clone(id));
        }
        match open_backing(file) {
            Ok(id) => {
                let id = Arc::new(id);
                map.insert(ino, (Arc::clone(&id), 1));
                Some(id)
            }
            Err(e) => {
                tracing::debug!(error = %e, "passthrough unavailable; using the page cache");
                self.passthrough.store(false, Ordering::Relaxed);
                None
            }
        }
    }

    fn release_backing(&self, ino: u64) {
        let mut map = self.backing.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, count)) = map.get_mut(&ino) {
            *count -= 1;
            if *count == 0 {
                map.remove(&ino);
            }
        }
    }

    /// Captures everything pending now (for tests and clean unmounts).
    pub fn flush_history(&self) {
        if let Some(c) = &self.capture {
            c.flush();
        }
    }
}

impl Filesystem for FernetFs {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        // Pass O_TRUNC to open instead of a separate truncate, so `echo x >
        // file#/key` arrives as one open-write-close on a fresh buffer.
        if config
            .add_capabilities(fuser::InitFlags::FUSE_ATOMIC_O_TRUNC)
            .is_ok()
        {
            self.atomic_trunc.store(true, Ordering::Relaxed);
        }
        // Real files keep their page cache across opens (FOPEN_KEEP_CACHE);
        // the kernel drops it when a file's mtime or size changes, including
        // changes made outside the mount.
        let _ = config.add_capabilities(fuser::InitFlags::FUSE_AUTO_INVAL_DATA);
        // Passthrough when the kernel offers it; the page cache otherwise.
        if std::env::var_os("FERNET_NO_PASSTHROUGH").is_none()
            && config
                .add_capabilities(fuser::InitFlags::FUSE_PASSTHROUGH)
                .is_ok()
        {
            let _ = config.set_max_stack_depth(2);
            self.passthrough.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn destroy(&mut self) {
        if let Some(c) = &self.capture {
            c.stop();
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some((dir, _)) = self.node(parent) else {
            return reply.error(Errno::ENOENT);
        };
        tracing::trace!(parent = parent.0, ?name, ?dir, "lookup");
        match self.fernet.lookup(&dir, name) {
            Ok(Resolved::Missing) => reply.error(Errno::ENOENT),
            Ok(r) => self.entry_reply(&r, parent.0, reply),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let mut inodes = self.inodes.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(n) = inodes.by_ino.get_mut(&ino.0) else {
            return;
        };
        n.lookups = n.lookups.saturating_sub(nlookup);
        if n.lookups == 0 && ino != INodeNo::ROOT {
            let key = n.resolved.clone();
            let current = inodes.by_key.get(&key) == Some(&ino.0);
            // Real nodes stay while their path still names them; virtual nodes
            // and orphans go.
            if !current || !is_real(&key) {
                inodes.by_ino.remove(&ino.0);
                if current {
                    inodes.by_key.remove(&key);
                }
            }
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        // An open real file answers from its handle, even after it was
        // unlinked or renamed over.
        let file = fh
            .and_then(|fh| self.handle_file(fh.0))
            .or_else(|| self.any_open_file(ino.0));
        let gone = matches!(self.node(ino), Some((Resolved::RealFile { path }, _)) if fs::symlink_metadata(&path).map(|m| (m.dev(), m.ino())).ok() != self.ident(ino.0));
        if let Some(meta) = file
            .filter(|_| fh.is_some() || gone)
            .and_then(|f| f.metadata().ok())
        {
            return reply.attr(&TTL_PRESENT, &Self::real_attr(ino.0, &meta));
        }
        let Some((r, _)) = self.node(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.attr(ino.0, &r) {
            Ok((attr, ttl)) => reply.attr(&ttl, &attr),
            Err(e) => reply.error(e),
        }
    }

    #[allow(clippy::too_many_lines)] // One FUSE operation; its checks read best in one place.
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some((r, _)) = self.node(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let changes = mode.is_some()
            || size.is_some()
            || atime.is_some()
            || mtime.is_some()
            || uid.is_some()
            || gid.is_some();
        if changes && (!self.writable || r.is_past()) {
            return reply.error(Errno::EROFS);
        }
        let res: Result<(), Errno> = (|| {
            match &r {
                Resolved::RealFile { path } | Resolved::RealDir { path } => {
                    let handle = fh.and_then(|fh| self.handle_file(fh.0));
                    let meta = match &handle {
                        Some(f) => f.metadata(),
                        None => fs::symlink_metadata(path),
                    }
                    .map_err(Errno::from)?;
                    // Changing ownership needs privileges fernet doesn't use.
                    if uid.is_some_and(|u| u != meta.uid()) || gid.is_some_and(|g| g != meta.gid())
                    {
                        return Err(Errno::EPERM);
                    }
                    if let Some(m) = mode {
                        if meta.file_type().is_symlink() {
                            return Err(Errno::EOPNOTSUPP);
                        }
                        fs::set_permissions(path, fs::Permissions::from_mode(m & 0o7777))
                            .map_err(Errno::from)?;
                    }
                    if let Some(len) = size {
                        match &handle {
                            Some(f) => f.set_len(len).map_err(Errno::from)?,
                            // Non-blocking: a FIFO must not stall a worker.
                            None => OpenOptions::new()
                                .write(true)
                                .custom_flags(O_NONBLOCK)
                                .open(path)
                                .and_then(|f| f.set_len(len))
                                .map_err(Errno::from)?,
                        }
                        self.touch(path);
                    }
                    if atime.is_some() || mtime.is_some() {
                        // utimensat without following symlinks, without opening.
                        let t = |v: Option<TimeOrNow>| match v {
                            None => nix::sys::time::TimeSpec::UTIME_OMIT,
                            Some(TimeOrNow::Now) => nix::sys::time::TimeSpec::UTIME_NOW,
                            Some(TimeOrNow::SpecificTime(t)) => {
                                let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                                nix::sys::time::TimeSpec::from_duration(d)
                            }
                        };
                        nix::sys::stat::utimensat(
                            nix::fcntl::AT_FDCWD,
                            path.as_path(),
                            &t(atime),
                            &t(mtime),
                            nix::sys::stat::UtimensatFlags::NoFollowSymlink,
                        )
                        .map_err(|e| Errno::from_i32(e as i32))?;
                    }
                }
                Resolved::Inside {
                    node: NodeKind::Scalar(_),
                    ..
                } => {
                    if mode.is_some() || uid.is_some() || gid.is_some() {
                        return Err(Errno::EPERM);
                    }
                    if let Some(len) = size {
                        let len = usize::try_from(len)
                            .ok()
                            .filter(|&l| l <= MAX_VALUE)
                            .ok_or(Errno::EFBIG)?;
                        let mut handles =
                            self.handles.lock().unwrap_or_else(PoisonError::into_inner);
                        match fh.and_then(|fh| handles.get_mut(&fh.0)) {
                            // An explicit truncate is a change: apply it at flush.
                            Some(Handle::Value {
                                buf,
                                dirty,
                                written,
                                ..
                            }) => {
                                buf.resize(len, 0);
                                *dirty = true;
                                *written = true;
                            }
                            Some(Handle::NewKey { buf, written, .. }) => {
                                buf.resize(len, 0);
                                *written = true;
                            }
                            // `O_TRUNC` without atomic truncation sends size 0
                            // before the open that writes: nothing to do yet.
                            _ if len == 0 && !self.atomic_trunc.load(Ordering::Relaxed) => {}
                            _ => {
                                drop(handles);
                                // truncate(2) on a path: write the shortened value now.
                                let mut cur = self.fernet.read(&r).map_err(|e| errno(&e))?;
                                cur.resize(len, 0);
                                let text = String::from_utf8(cur).map_err(|_| Errno::EINVAL)?;
                                self.fernet.write_value(&r, &text).map_err(|e| errno(&e))?;
                            }
                        }
                    }
                }
                _ if mode.is_some() || uid.is_some() || gid.is_some() => return Err(Errno::EPERM),
                _ => {}
            }
            Ok(())
        })();
        if let Err(e) = res {
            return reply.error(e);
        }
        if let Some(meta) = fh
            .and_then(|fh| self.handle_file(fh.0))
            .and_then(|f| f.metadata().ok())
        {
            return reply.attr(&TTL_PRESENT, &Self::real_attr(ino.0, &meta));
        }
        match self.attr(ino.0, &r) {
            Ok((mut attr, ttl)) => {
                if let (Some(len), Resolved::Inside { .. }) = (size, &r) {
                    attr.size = len;
                }
                reply.attr(&ttl, &attr);
            }
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.node(ino) {
            Some((Resolved::RealFile { path } | Resolved::RealDir { path }, _)) => {
                match fs::read_link(&path) {
                    Ok(target) => reply.data(target.as_os_str().as_encoded_bytes()),
                    Err(e) => reply.error(Errno::from(e)),
                }
            }
            _ => reply.error(Errno::EINVAL),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let dir = match self.parent_for_change(parent, name) {
            Ok(d) => d,
            Err(e) => return reply.error(e),
        };
        let res = match &dir {
            Resolved::RealDir { path } => {
                let p = path.join(name);
                fs::create_dir(&p)
                    .and_then(|()| {
                        fs::set_permissions(&p, fs::Permissions::from_mode(mode & !umask & 0o7777))
                    })
                    .map_err(Errno::from)
            }
            Resolved::Inside {
                node: NodeKind::Object | NodeKind::Array,
                ..
            } => {
                let Some(key) = name.to_str().and_then(fernet_fs_path::decode_token) else {
                    return reply.error(Errno::EINVAL);
                };
                self.fernet
                    .insert(&dir, &key, NewValue::EmptyObject)
                    .map_err(|e| errno(&e))
            }
            _ => Err(Errno::ENOTDIR),
        };
        if let Err(e) = res {
            return reply.error(e);
        }
        match self.fernet.lookup(&dir, name) {
            Ok(r) if r != Resolved::Missing => self.entry_reply(&r, parent.0, reply),
            Ok(_) => reply.error(Errno::EIO),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let dir = match self.parent_for_change(parent, name) {
            Ok(d) => d,
            Err(e) => return reply.error(e),
        };
        let res = match &dir {
            Resolved::RealDir { path } => {
                let p = path.join(name);
                fs::remove_file(&p).map_err(Errno::from).map(|()| {
                    self.forget_path(&p);
                    self.touch(&p);
                })
            }
            Resolved::Inside { .. } => match self.fernet.lookup(&dir, name) {
                Ok(Resolved::Missing) => Err(Errno::ENOENT),
                Ok(child) => self.fernet.remove(&child).map_err(|e| errno(&e)),
                Err(e) => Err(errno(&e)),
            },
            _ => Err(Errno::ENOTDIR),
        };
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let dir = match self.parent_for_change(parent, name) {
            Ok(d) => d,
            Err(e) => return reply.error(e),
        };
        let res = match &dir {
            Resolved::RealDir { path } => {
                let p = path.join(name);
                fs::remove_dir(&p)
                    .map_err(Errno::from)
                    .map(|()| self.forget_path(&p))
            }
            Resolved::Inside { .. } => match self.fernet.lookup(&dir, name) {
                Ok(Resolved::Missing) => Err(Errno::ENOENT),
                Ok(child) => self.fernet.remove(&child).map_err(|e| errno(&e)),
                Err(e) => Err(errno(&e)),
            },
            _ => Err(Errno::ENOTDIR),
        };
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let dir = match self.parent_for_change(parent, link_name) {
            Ok(d) => d,
            Err(e) => return reply.error(e),
        };
        let Resolved::RealDir { path } = &dir else {
            return reply.error(Errno::EPERM);
        };
        let p = path.join(link_name);
        if let Err(e) = std::os::unix::fs::symlink(target, &p) {
            return reply.error(Errno::from(e));
        }
        self.entry_reply(&Resolved::RealFile { path: p }, parent.0, reply);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (from_dir, to_dir) = match (
            self.parent_for_change(parent, name),
            self.parent_for_change(newparent, newname),
        ) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return reply.error(e),
        };
        let (Resolved::RealDir { path: a }, Resolved::RealDir { path: b }) = (&from_dir, &to_dir)
        else {
            return reply.error(Errno::EXDEV);
        };
        let (from, to) = (a.join(name), b.join(newname));
        #[cfg(target_os = "linux")]
        {
            if flags.contains(RenameFlags::RENAME_EXCHANGE) {
                return reply.error(Errno::EINVAL);
            }
            if flags.contains(RenameFlags::RENAME_NOREPLACE) && fs::symlink_metadata(&to).is_ok() {
                return reply.error(Errno::EEXIST);
            }
        }
        #[cfg(not(target_os = "linux"))]
        if !flags.is_empty() {
            return reply.error(Errno::EINVAL);
        }
        if let Err(e) = fs::rename(&from, &to) {
            return reply.error(Errno::from(e));
        }
        self.inodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .rename(&from, &to);
        // An editor's atomic save renames a temp file over the original: the
        // target gets a new version. The source, if it had history, gets a
        // tombstone. Both go through the debouncer.
        self.touch(&from);
        self.touch(&to);
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some((r, _)) = self.node(ino) else {
            return reply.error(Errno::ENOENT);
        };
        tracing::trace!(ino = ino.0, ?r, "open");
        let write = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let trunc = flags.0 & O_TRUNC != 0;
        if write && (!self.writable || r.is_past()) {
            return reply.error(Errno::EROFS);
        }
        match &r {
            Resolved::RealFile { path } => {
                let res = OpenOptions::new()
                    .read(flags.acc_mode() != OpenAccMode::O_WRONLY)
                    .write(write)
                    .truncate(trunc && write)
                    .open(path);
                let file = match res {
                    Ok(f) => f,
                    Err(e) => return reply.error(Errno::from(e)),
                };
                // The path now names another file (replaced since lookup):
                // ESTALE makes the kernel look it up again and get a fresh inode.
                let opened = file.metadata().ok().map(|m| (m.dev(), m.ino()));
                if opened.is_some() && self.ident(ino.0).is_some_and(|i| Some(i) != opened) {
                    return reply.error(Errno::from_i32(ESTALE));
                }
                if let Some(id) = self.backing_for(ino.0, &file, |f| reply.open_backing(f)) {
                    // Writes bypass us under passthrough: assume a writable
                    // handle wrote, so release snapshots it.
                    let fh = self.new_handle(Handle::Real {
                        file,
                        path: path.clone(),
                        written: write,
                        ino: ino.0,
                        passthrough: true,
                    });
                    return reply.opened_passthrough(fh, FopenFlags::empty(), &id);
                }
                let fh = self.new_handle(Handle::Real {
                    file,
                    path: path.clone(),
                    written: trunc && write,
                    ino: ino.0,
                    passthrough: false,
                });
                reply.opened(fh, FopenFlags::FOPEN_KEEP_CACHE);
            }
            Resolved::Inside {
                node: NodeKind::Scalar(_),
                ..
            } if write => {
                let buf = if trunc {
                    Vec::new()
                } else {
                    match self.fernet.read(&r) {
                        Ok(b) => b,
                        Err(e) => return reply.error(errno(&e)),
                    }
                };
                let fh = self.new_handle(Handle::Value {
                    ino: ino.0,
                    buf,
                    dirty: trunc,
                    written: false,
                });
                // The size changes as the buffer does: bypass the page cache.
                reply.opened(fh, FopenFlags::FOPEN_DIRECT_IO);
            }
            _ => match self.fernet.read(&r) {
                Ok(bytes) => {
                    // History is immutable: let the kernel keep its cache.
                    let keep = if r.is_past() {
                        FopenFlags::FOPEN_KEEP_CACHE
                    } else {
                        FopenFlags::FOPEN_DIRECT_IO
                    };
                    reply.opened(self.new_handle(Handle::Bytes(Arc::new(bytes))), keep);
                }
                Err(e) => reply.error(errno(&e)),
            },
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let dir = match self.parent_for_change(parent, name) {
            Ok(d) => d,
            Err(e) => return reply.error(e),
        };
        match &dir {
            Resolved::RealDir { path } => {
                let p = path.join(name);
                let res = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(flags & O_TRUNC != 0)
                    .mode(mode & !umask & 0o7777)
                    .open(&p);
                let file = match res {
                    Ok(f) => f,
                    Err(e) => return reply.error(Errno::from(e)),
                };
                let r = Resolved::RealFile { path: p.clone() };
                let ino = self.register(r.clone(), parent.0);
                let (attr, ttl) = match self.attr(ino, &r) {
                    Ok(a) => a,
                    Err(e) => return reply.error(e),
                };
                self.add_ref(ino);
                // Same I/O mode as later opens of this inode: the kernel
                // refuses to mix passthrough and cached opens on one inode.
                if let Some(id) = self.backing_for(ino, &file, |f| reply.open_backing(f)) {
                    let fh = self.new_handle(Handle::Real {
                        file,
                        path: p,
                        written: true,
                        ino,
                        passthrough: true,
                    });
                    return reply.created_passthrough(
                        &ttl,
                        &attr,
                        Generation(0),
                        fh,
                        FopenFlags::empty(),
                        &id,
                    );
                }
                let fh = self.new_handle(Handle::Real {
                    file,
                    path: p,
                    written: true,
                    ino,
                    passthrough: false,
                });
                reply.created(&ttl, &attr, Generation(0), fh, FopenFlags::FOPEN_KEEP_CACHE);
            }
            Resolved::Inside {
                source,
                pointer,
                node: NodeKind::Object | NodeKind::Array,
            } => {
                let Some(key) = name.to_str().and_then(fernet_fs_path::decode_token) else {
                    return reply.error(Errno::EINVAL);
                };
                if matches!(self.fernet.lookup(&dir, name), Ok(r) if r != Resolved::Missing) {
                    return reply.error(Errno::EEXIST);
                }
                // Fail now, not at close (which shells ignore), if keys can't be added.
                if !self.fernet.capabilities(&dir).is_some_and(|c| c.insert) {
                    return reply.error(Errno::EOPNOTSUPP);
                }
                let mut ptr = pointer.clone();
                ptr.push(key.clone());
                let r = Resolved::Inside {
                    source: source.clone(),
                    pointer: ptr,
                    node: NodeKind::Scalar(fernet_fs_core::ScalarType::String),
                };
                let ino = self.register(r.clone(), parent.0);
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(ino, 0);
                let fh = self.new_handle(Handle::NewKey {
                    parent: parent.0,
                    key,
                    buf: Vec::new(),
                    ino,
                    inserted: false,
                    written: false,
                });
                match self.attr(ino, &r) {
                    Ok((attr, ttl)) => {
                        self.add_ref(ino);
                        reply.created(&ttl, &attr, Generation(0), fh, FopenFlags::FOPEN_DIRECT_IO);
                    }
                    Err(e) => reply.error(e),
                }
            }
            _ => reply.error(Errno::ENOTDIR),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        let slice = |bytes: &[u8]| {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let end = start.saturating_add(size as usize).min(bytes.len());
            bytes[start..end].to_vec()
        };
        match handles.get(&fh.0) {
            Some(Handle::Real { file, .. }) => {
                let f = file.try_clone();
                drop(handles);
                let mut buf = vec![0; size as usize];
                match f.and_then(|f| f.read_at(&mut buf, offset)) {
                    Ok(n) => reply.data(&buf[..n]),
                    Err(e) => reply.error(Errno::from(e)),
                }
            }
            Some(Handle::Bytes(bytes)) => reply.data(&slice(bytes)),
            Some(Handle::Value { buf, .. } | Handle::NewKey { buf, .. }) => reply.data(&slice(buf)),
            Some(Handle::Dir(_)) | None => reply.error(Errno::EBADF),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        let n = u32::try_from(data.len()).unwrap_or(u32::MAX);
        match handles.get_mut(&fh.0) {
            Some(Handle::Real { file, written, .. }) => {
                *written = true;
                // No lock held across the syscall: other workers carry on.
                let f = file.try_clone();
                drop(handles);
                match f.and_then(|f| f.write_at(data, offset)) {
                    Ok(w) => reply.written(u32::try_from(w).unwrap_or(u32::MAX)),
                    Err(e) => reply.error(Errno::from(e)),
                }
            }
            Some(Handle::Value {
                buf,
                dirty,
                written,
                ..
            }) => match write_into(buf, offset, data) {
                Ok(()) => {
                    *dirty = true;
                    *written = true;
                    reply.written(n);
                }
                Err(e) => reply.error(e),
            },
            Some(Handle::NewKey {
                buf, ino, written, ..
            }) => {
                if let Err(e) = write_into(buf, offset, data) {
                    return reply.error(e);
                }
                *written = true;
                let (ino, len) = (*ino, buf.len());
                drop(handles);
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(ino, len);
                reply.written(n);
            }
            Some(Handle::Bytes(_) | Handle::Dir(_)) | None => reply.error(Errno::EBADF),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.apply(fh.0, false) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let res = match self.handle_file(fh.0) {
            Some(f) => if datasync {
                f.sync_data()
            } else {
                f.sync_all()
            }
            .map_err(Errno::from),
            None => Ok(()),
        };
        match res.and_then(|()| self.apply(fh.0, false)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let res = self.apply(fh.0, true);
        let handle = self
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&fh.0);
        match handle {
            Some(Handle::Real {
                path,
                written,
                ino,
                passthrough,
                ..
            }) => {
                if passthrough {
                    self.release_backing(ino);
                }
                if written {
                    self.touch(&path);
                }
            }
            Some(Handle::NewKey { ino, .. }) => {
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&ino);
            }
            _ => {}
        }
        match res {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let Some((r, parent)) = self.node(ino) else {
            return reply.error(Errno::ENOENT);
        };
        let entries = match self.fernet.list(&r) {
            Ok(e) => e,
            Err(e) => return reply.error(errno(&e)),
        };
        let mut listing = vec![
            (ino.0, FileType::Directory, OsString::from(".")),
            (parent, FileType::Directory, OsString::from("..")),
        ];
        for e in entries {
            let child = self.entry_ino(&r, ino.0, &e);
            listing.push((child, entry_kind(e.kind), e.name));
        }
        reply.opened(self.new_handle(Handle::Dir(listing)), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(Handle::Dir(listing)) = handles.get(&fh.0) else {
            return reply.error(Errno::EBADF);
        };
        let skip = usize::try_from(offset).unwrap_or(usize::MAX);
        for (i, (ino, kind, name)) in listing.iter().enumerate().skip(skip) {
            if reply.add(INodeNo(*ino), i as u64 + 1, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(no_xattr());
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }
}

/// Mount options: kernel permission checks against the attributes we report,
/// a recognisable name, and `ro` for read-only mounts.
#[must_use]
pub fn config(read_only: bool) -> fuser::Config {
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::FSName("fernet".into()),
        fuser::MountOption::Subtype("fernet".into()),
        fuser::MountOption::DefaultPermissions,
        fuser::MountOption::NoAtime,
    ];
    if read_only {
        config.mount_options.push(fuser::MountOption::RO);
    }
    config.n_threads = Some(4);
    config
}

/// Mounts `fernet` at `mountpoint` in a background thread. Writable mounts
/// capture history: the store must be open for writing. `window` is the
/// capture debounce. Dropping the session unmounts.
///
/// # Errors
///
/// When the mount fails (no FUSE, a bad mountpoint).
pub fn spawn(
    fernet: Arc<Fernet>,
    mountpoint: &Path,
    writable: bool,
    window: Duration,
) -> std::io::Result<fuser::BackgroundSession> {
    let fs = if writable {
        let capture = Capture::start(&fernet, window, true);
        FernetFs::writable(fernet, capture)
    } else {
        FernetFs::new(fernet)
    };
    fuser::spawn_mount(fs, mountpoint, &config(!writable))
}
