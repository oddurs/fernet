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
use fernet_fs_core::{
    Entry, EntryKind, Error, Fernet, NewValue, NodeKind, Resolved, STORE_DIR, Source,
};
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

/// "No such attribute": `ENODATA` on Linux, `ENOATTR` (93) on macOS.
fn no_xattr() -> Errno {
    if cfg!(target_os = "linux") { Errno::from_i32(61) } else { Errno::from_i32(93) }
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
}

enum Handle {
    /// A real file in the backing directory.
    Real {
        file: File,
        path: PathBuf,
        written: bool,
    },
    /// Rendered bytes of a read-only virtual file.
    Bytes(Arc<Vec<u8>>),
    /// A value inside a file, open for writing.
    /// `written` says whether `write` was called since open: a flush before
    /// any write (a shell's `dup2` then `close`) must not apply the buffer yet.
    Value {
        node: Resolved,
        buf: Vec<u8>,
        dirty: bool,
        written: bool,
    },
    /// A new key, inserted once written (or on release if never written).
    NewKey {
        parent: Resolved,
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
}

/// Inodes keyed by resolved identity. Real paths keep their inode for the
/// life of the mount; virtual nodes are dropped once the kernel forgets them.
struct Inodes {
    by_ino: HashMap<u64, Node>,
    by_key: HashMap<Resolved, u64>,
    next: u64,
}

impl Inodes {
    fn get_or_insert(&mut self, resolved: Resolved, parent: u64) -> u64 {
        if let Some(&ino) = self.by_key.get(&resolved) {
            return ino;
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
            },
        );
        ino
    }

    /// After a rename, moves every real path under `from` to `to`, keeping
    /// inode numbers.
    fn rename(&mut self, from: &Path, to: &Path) {
        let moved: Vec<(Resolved, u64)> = self
            .by_key
            .iter()
            .filter_map(|(k, &ino)| match k {
                Resolved::RealFile { path } | Resolved::RealDir { path }
                    if path.starts_with(from) =>
                {
                    Some((k.clone(), ino))
                }
                _ => None,
            })
            .collect();
        for (old, ino) in moved {
            self.by_key.remove(&old);
            let new = match &old {
                Resolved::RealFile { path } => Resolved::RealFile {
                    path: rebase(path, from, to),
                },
                Resolved::RealDir { path } => Resolved::RealDir {
                    path: rebase(path, from, to),
                },
                other => other.clone(),
            };
            // A replaced target loses its old inode mapping.
            if let Some(prev) = self.by_key.insert(new.clone(), ino) {
                self.by_ino.remove(&prev);
            }
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

fn time_of(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    }
}

/// Splices `data` into `buf` at `offset`, growing it with zeros as needed.
fn write_into(buf: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let end = start.saturating_add(data.len());
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[start..end].copy_from_slice(data);
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
        }
    }

    fn node(&self, ino: INodeNo) -> Option<(Resolved, u64)> {
        let inodes = self.inodes.lock().unwrap_or_else(PoisonError::into_inner);
        inodes
            .by_ino
            .get(&ino.0)
            .map(|n| (n.resolved.clone(), n.parent))
    }

    /// Registers a looked-up (or created) node, taking one lookup reference.
    fn remember(&self, r: Resolved, parent: u64) -> u64 {
        let mut inodes = self.inodes.lock().unwrap_or_else(PoisonError::into_inner);
        let ino = inodes.get_or_insert(r, parent);
        if let Some(n) = inodes.by_ino.get_mut(&ino) {
            n.lookups += 1;
        }
        ino
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

    /// Attributes and a cache lifetime for a node.
    fn attr(&self, ino: u64, r: &Resolved) -> Result<(FileAttr, Duration), Errno> {
        match r {
            Resolved::RealFile { path } | Resolved::RealDir { path } => {
                let m = fs::symlink_metadata(path).map_err(Errno::from)?;
                let t = |s: i64, ns: i64| {
                    system_time(s.saturating_mul(1_000_000_000).saturating_add(ns))
                };
                let attr = FileAttr {
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
                };
                Ok((attr, TTL_PRESENT))
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
    fn entry_ino(&self, dir: &Resolved, parent: u64, e: &Entry) -> u64 {
        match self.fernet.lookup(dir, &e.name) {
            Ok(r) if r != Resolved::Missing => self
                .inodes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert(r, parent),
            // Something changed between listing and lookup: any non-zero value
            // will do, the kernel looks the name up before using it.
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
        if parent == INodeNo::ROOT && name == STORE_DIR {
            return Err(Errno::EACCES);
        }
        Ok(dir)
    }

    fn entry_reply(&self, r: &Resolved, parent: u64, reply: ReplyEntry) {
        let ino = self.remember(r.clone(), parent);
        match self.attr(ino, r) {
            Ok((attr, ttl)) => reply.entry(&ttl, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    /// Applies a buffered value or new key. On `flush` (`releasing` false)
    /// only after a write; on release whatever is pending. Returns an errno to
    /// report from `flush`/`close` when the write is refused.
    fn apply(&self, fh: u64, releasing: bool) -> Result<(), Errno> {
        let mut handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
        match handles.get_mut(&fh) {
            Some(Handle::Value {
                node,
                buf,
                dirty,
                written,
            }) if *dirty && (*written || releasing) => {
                let text = String::from_utf8(buf.clone()).map_err(|_| Errno::EINVAL)?;
                let node = node.clone();
                *dirty = false;
                drop(handles);
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
                let (parent, key, ino) = (parent.clone(), key.clone(), *ino);
                *inserted = true;
                *written = false;
                drop(handles);
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&ino);
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
                    parent.clone(),
                    fernet_fs_path::encode_token(key).unwrap_or_default(),
                );
                *written = false;
                drop(handles);
                let node = self
                    .fernet
                    .lookup(&parent, OsStr::new(&name))
                    .map_err(|e| errno(&e))?;
                self.fernet.write_value(&node, &text).map_err(|e| errno(&e))
            }
            _ => Ok(()),
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
        let _ = config.add_capabilities(fuser::InitFlags::FUSE_ATOMIC_O_TRUNC);
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
        if n.lookups == 0 && ino != INodeNo::ROOT && !is_real(&n.resolved) {
            let key = n.resolved.clone();
            inodes.by_ino.remove(&ino.0);
            inodes.by_key.remove(&key);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some((r, _)) = self.node(ino) else {
            return reply.error(Errno::ENOENT);
        };
        match self.attr(ino.0, &r) {
            Ok((attr, ttl)) => reply.attr(&ttl, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
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
        let changes = mode.is_some() || size.is_some() || atime.is_some() || mtime.is_some();
        if changes && (!self.writable || r.is_past()) {
            return reply.error(Errno::EROFS);
        }
        let res: Result<(), Errno> = (|| {
            match &r {
                Resolved::RealFile { path } | Resolved::RealDir { path } => {
                    if let Some(m) = mode {
                        fs::set_permissions(path, fs::Permissions::from_mode(m & 0o7777))
                            .map_err(Errno::from)?;
                    }
                    if let Some(len) = size {
                        OpenOptions::new()
                            .write(true)
                            .open(path)
                            .and_then(|f| f.set_len(len))
                            .map_err(Errno::from)?;
                        self.touch(path);
                    }
                    if atime.is_some() || mtime.is_some() {
                        let f = File::options()
                            .write(true)
                            .open(path)
                            .or_else(|_| File::open(path))
                            .map_err(Errno::from)?;
                        let mut times = fs::FileTimes::new();
                        if let Some(a) = atime {
                            times = times.set_accessed(time_of(a));
                        }
                        if let Some(m) = mtime {
                            times = times.set_modified(time_of(m));
                        }
                        f.set_times(times).map_err(Errno::from)?;
                    }
                }
                Resolved::Inside {
                    node: NodeKind::Scalar(_),
                    ..
                } => {
                    if let Some(len) = size {
                        let mut handles =
                            self.handles.lock().unwrap_or_else(PoisonError::into_inner);
                        match fh.and_then(|fh| handles.get_mut(&fh.0)) {
                            Some(Handle::Value { buf, dirty, .. }) => {
                                buf.resize(usize::try_from(len).unwrap_or(usize::MAX), 0);
                                *dirty = true;
                            }
                            Some(Handle::NewKey { buf, .. }) => {
                                buf.resize(usize::try_from(len).unwrap_or(usize::MAX), 0);
                            }
                            // `O_TRUNC` without atomic truncation sends size 0 before the
                            // open that writes the new value: nothing to do yet.
                            _ if len == 0 => {}
                            _ => {
                                drop(handles);
                                // truncate(2) on a path: write the shortened value now.
                                let mut cur = self.fernet.read(&r).map_err(|e| errno(&e))?;
                                cur.resize(usize::try_from(len).unwrap_or(usize::MAX), 0);
                                let text = String::from_utf8(cur).map_err(|_| Errno::EINVAL)?;
                                self.fernet.write_value(&r, &text).map_err(|e| errno(&e))?;
                            }
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(e) = res {
            return reply.error(e);
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
                fs::remove_file(&p)
                    .map_err(Errno::from)
                    .map(|()| self.touch(&p))
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
            Resolved::RealDir { path } => fs::remove_dir(path.join(name)).map_err(Errno::from),
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
                match res {
                    Ok(file) => {
                        let fh = self.new_handle(Handle::Real {
                            file,
                            path: path.clone(),
                            written: trunc && write,
                        });
                        reply.opened(fh, FopenFlags::empty());
                    }
                    Err(e) => reply.error(Errno::from(e)),
                }
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
                    node: r.clone(),
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
                let ino = self.remember(r.clone(), parent.0);
                let fh = self.new_handle(Handle::Real {
                    file,
                    path: p,
                    written: true,
                });
                match self.attr(ino, &r) {
                    Ok((attr, ttl)) => {
                        reply.created(&ttl, &attr, Generation(0), fh, FopenFlags::empty());
                    }
                    Err(e) => reply.error(e),
                }
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
                let ino = self.remember(r.clone(), parent.0);
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(ino, 0);
                let fh = self.new_handle(Handle::NewKey {
                    parent: dir.clone(),
                    key,
                    buf: Vec::new(),
                    ino,
                    inserted: false,
                    written: false,
                });
                match self.attr(ino, &r) {
                    Ok((attr, ttl)) => {
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
            Some(Handle::Real { file, written, .. }) => match file.write_at(data, offset) {
                Ok(w) => {
                    *written = true;
                    reply.written(u32::try_from(w).unwrap_or(u32::MAX));
                }
                Err(e) => reply.error(Errno::from(e)),
            },
            Some(Handle::Value {
                buf,
                dirty,
                written,
                ..
            }) => {
                write_into(buf, offset, data);
                *dirty = true;
                *written = true;
                reply.written(n);
            }
            Some(Handle::NewKey {
                buf, ino, written, ..
            }) => {
                write_into(buf, offset, data);
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
        let res = {
            let handles = self.handles.lock().unwrap_or_else(PoisonError::into_inner);
            match handles.get(&fh.0) {
                Some(Handle::Real { file, .. }) => if datasync {
                    file.sync_data()
                } else {
                    file.sync_all()
                }
                .map_err(Errno::from),
                _ => Ok(()),
            }
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
        if let Some(Handle::Real {
            path,
            written: true,
            ..
        }) = handle
        {
            self.touch(&path);
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
