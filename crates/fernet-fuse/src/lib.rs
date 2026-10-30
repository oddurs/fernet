//! FUSE adapter over `fernet-fs-core`.
//!
//! Every kernel lookup is one call to [`Fernet::lookup`], the same step the
//! CLI folds over a path, so the mount and the CLI resolve identically
//! (invariant 7). This crate holds no resolution logic of its own: it maps
//! resolved nodes to inodes, attributes and bytes.
//!
//! Milestone 3 mounts read-only. Real files are read straight from the backing
//! directory; virtual files (past versions, values inside files) are rendered
//! once at `open` so a reader sees one consistent snapshot.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fernet_fs_core::{Entry, EntryKind, Error, Fernet, Resolved, Source};
use fernet_fs_formats::NodeKind;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyXattr, Request,
};

/// How long the kernel may cache present-day entries and attributes.
const TTL_PRESENT: Duration = Duration::from_secs(1);
/// History never changes; cache it longer.
const TTL_PAST: Duration = Duration::from_secs(60);

/// The fernet filesystem.
pub struct FernetFs {
    fernet: Arc<Fernet>,
    inodes: Mutex<Inodes>,
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
    uid: u32,
    gid: u32,
}

enum Handle {
    /// A real file, read straight from the backing directory.
    Real(File),
    /// Rendered bytes of a virtual file.
    Bytes(Arc<Vec<u8>>),
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

impl FernetFs {
    /// A filesystem over `fernet`.
    #[must_use]
    pub fn new(fernet: Arc<Fernet>) -> Self {
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
            inodes: Mutex::new(inodes),
            handles: Mutex::new(HashMap::new()),
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

    fn virtual_attr(&self, ino: u64, kind: FileType, size: u64, mtime: SystemTime) -> FileAttr {
        let dir = kind == FileType::Directory;
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm: if dir { 0o555 } else { 0o444 },
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
                ),
                TTL_PAST,
            )),
            Resolved::PastDir { at, .. } => Ok((
                self.virtual_attr(ino, FileType::Directory, 0, system_time(*at)),
                TTL_PAST,
            )),
            Resolved::VersionListing { path } => {
                let mtime = fs::metadata(path)
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                Ok((
                    self.virtual_attr(ino, FileType::Directory, 0, mtime),
                    TTL_PRESENT,
                ))
            }
            Resolved::Inside { source, node, .. } => {
                let (mtime, ttl) = match source {
                    Source::Present(p) => (
                        fs::metadata(p)
                            .and_then(|m| m.modified())
                            .unwrap_or(UNIX_EPOCH),
                        TTL_PRESENT,
                    ),
                    Source::Past { version, .. } => (system_time(version.time), TTL_PAST),
                };
                if let NodeKind::Scalar(_) = node {
                    let size = self.fernet.read(r).map_err(|e| errno(&e))?.len() as u64;
                    Ok((
                        self.virtual_attr(ino, FileType::RegularFile, size, mtime),
                        ttl,
                    ))
                } else {
                    Ok((self.virtual_attr(ino, FileType::Directory, 0, mtime), ttl))
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
}

impl Filesystem for FernetFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some((dir, _)) = self.node(parent) else {
            return reply.error(Errno::ENOENT);
        };
        let r = match self.fernet.lookup(&dir, name) {
            Ok(Resolved::Missing) => return reply.error(Errno::ENOENT),
            Ok(r) => r,
            Err(e) => return reply.error(errno(&e)),
        };
        let ino = {
            let mut inodes = self.inodes.lock().unwrap_or_else(PoisonError::into_inner);
            let ino = inodes.get_or_insert(r.clone(), parent.0);
            if let Some(n) = inodes.by_ino.get_mut(&ino) {
                n.lookups += 1;
            }
            ino
        };
        match self.attr(ino, &r) {
            Ok((attr, ttl)) => reply.entry(&ttl, &attr, Generation(0)),
            Err(e) => reply.error(e),
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

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some((r, _)) = self.node(ino) else {
            return reply.error(Errno::ENOENT);
        };
        if flags.0 & 0b11 != 0 {
            // O_WRONLY or O_RDWR: the mount is read-only in milestone 3.
            return reply.error(Errno::EROFS);
        }
        match &r {
            Resolved::RealFile { path } => match File::open(path) {
                Ok(f) => reply.opened(self.new_handle(Handle::Real(f)), FopenFlags::empty()),
                Err(e) => reply.error(Errno::from(e)),
            },
            _ => match self.fernet.read(&r) {
                Ok(bytes) => {
                    // History is immutable: let the kernel keep its cache.
                    let keep = if r.is_past() {
                        FopenFlags::FOPEN_KEEP_CACHE
                    } else {
                        FopenFlags::empty()
                    };
                    reply.opened(self.new_handle(Handle::Bytes(Arc::new(bytes))), keep);
                }
                Err(e) => reply.error(errno(&e)),
            },
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
        match handles.get(&fh.0) {
            Some(Handle::Real(f)) => {
                let mut buf = vec![0; size as usize];
                let f = f.try_clone();
                drop(handles);
                let res = f.and_then(|f| f.read_at(&mut buf, offset));
                match res {
                    Ok(n) => reply.data(&buf[..n]),
                    Err(e) => reply.error(Errno::from(e)),
                }
            }
            Some(Handle::Bytes(bytes)) => {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(bytes.len());
                let end = start.saturating_add(size as usize).min(bytes.len());
                reply.data(&bytes[start..end]);
            }
            Some(Handle::Dir(_)) | None => reply.error(Errno::EBADF),
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
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&fh.0);
        reply.ok();
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
        reply.error(Errno::ENODATA);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }
}

/// Mount options fernet uses: read-only in milestone 3, kernel permission
/// checks against the attributes we report, and a recognisable name.
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

/// Mounts `fernet` at `mountpoint` in a background thread. Dropping the
/// session unmounts.
///
/// # Errors
///
/// When the mount fails (no FUSE, a bad mountpoint).
pub fn spawn(fernet: Arc<Fernet>, mountpoint: &Path) -> std::io::Result<fuser::BackgroundSession> {
    fuser::spawn_mount(FernetFs::new(fernet), mountpoint, &config(true))
}
