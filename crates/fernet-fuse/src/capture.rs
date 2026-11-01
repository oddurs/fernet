//! Capturing history while mounted: a trailing-edge debouncer per path, fed
//! by writes through the mount and by a watcher on the backing directory.
//!
//! Every change to a path restarts that path's quiet period; the snapshot runs
//! once the path has been quiet for the window (2 s by default). Editors'
//! atomic saves (rename the original away, write a new file, rename back) are
//! a burst of changes to one path, so they land as one version. Changes made
//! outside the mount and changes made through it coalesce the same way, and
//! identical content is never recorded twice, so nothing is double-counted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fernet_fs_core::Fernet;

/// Default quiet period before a path is captured.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(2);

enum Msg {
    Touch(PathBuf),
    Flush(mpsc::Sender<()>),
    Stop,
}

/// Debounced capture for one root.
pub struct Capture {
    tx: Mutex<Sender<Msg>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    _watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

impl Capture {
    /// Starts the capture thread and, if `watch` is set, a watcher on the
    /// backing directory.
    #[must_use]
    pub fn start(fernet: &Arc<Fernet>, window: Duration, watch: bool) -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        let f = Arc::clone(fernet);
        let thread = std::thread::Builder::new()
            .name("fernet-capture".into())
            .spawn(move || {
                let mut due: HashMap<PathBuf, Instant> = HashMap::new();
                // Handles one message; true to stop.
                let handle = |msg: Msg, due: &mut HashMap<PathBuf, Instant>| match msg {
                    Msg::Touch(p) => {
                        due.insert(p, Instant::now() + window);
                        false
                    }
                    Msg::Flush(done) => {
                        for (p, _) in due.drain() {
                            capture(&f, &p);
                        }
                        let _ = f.flush();
                        let _ = done.send(());
                        false
                    }
                    Msg::Stop => true,
                };
                loop {
                    let wait = due.values().min().map_or(Duration::from_secs(3600), |t| {
                        t.saturating_duration_since(Instant::now())
                    });
                    let mut stop = match rx.recv_timeout(wait) {
                        Ok(msg) => handle(msg, &mut due),
                        Err(RecvTimeoutError::Disconnected) => true,
                        Err(RecvTimeoutError::Timeout) => false,
                    };
                    // Take everything queued before scanning `due` again: a
                    // burst of touches costs one scan, not one each.
                    while !stop && let Ok(msg) = rx.try_recv() {
                        stop = handle(msg, &mut due);
                    }
                    if stop {
                        for (p, _) in due.drain() {
                            capture(&f, &p);
                        }
                        let _ = f.flush();
                        return;
                    }
                    let now = Instant::now();
                    let ready: Vec<PathBuf> = due
                        .iter()
                        .filter(|(_, t)| **t <= now)
                        .map(|(p, _)| p.clone())
                        .collect();
                    for p in &ready {
                        due.remove(p);
                        capture(&f, p);
                    }
                    if !ready.is_empty() {
                        let _ = f.flush();
                    }
                }
            })
            .ok();
        let watcher = if watch {
            start_watcher(fernet, tx.clone())
        } else {
            None
        };
        Self {
            tx: Mutex::new(tx),
            thread: Mutex::new(thread),
            _watcher: Mutex::new(watcher),
        }
    }

    /// Notes that `path` changed. Ignored paths and `.fernet/` are skipped.
    pub fn touch(&self, path: &Path) {
        let _ = self
            .tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(Msg::Touch(path.to_owned()));
    }

    /// Captures everything pending now and waits for it.
    pub fn flush(&self) {
        let (done, wait) = mpsc::channel();
        if self
            .tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(Msg::Flush(done))
            .is_ok()
        {
            let _ = wait.recv();
        }
    }

    /// Captures everything pending and stops.
    pub fn stop(&self) {
        let _ = self
            .tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(Msg::Stop);
        if let Some(t) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = t.join();
        }
    }
}

fn capture(f: &Fernet, path: &Path) {
    // Symlinks are never followed: a link to a directory is not a directory.
    let res = if is_real_dir(path) {
        f.snapshot_tree(path).map(|_| ())
    } else {
        f.snapshot(path).map(|_| ())
    };
    if let Err(e) = res {
        tracing::warn!(path = %path.display(), error = %e, "capture failed");
    }
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// A watcher that feeds changes made outside the mount into the debouncer.
fn start_watcher(fernet: &Arc<Fernet>, tx: Sender<Msg>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let f = Arc::clone(fernet);
    let mut w = notify::RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            // Opening or reading changes nothing, and capture itself opens
            // directories: following those events would rescan forever.
            if matches!(
                ev.kind,
                notify::EventKind::Access(
                    notify::event::AccessKind::Open(_)
                        | notify::event::AccessKind::Read
                        | notify::event::AccessKind::Close(
                            notify::event::AccessMode::Read | notify::event::AccessMode::Execute
                        )
                        | notify::event::AccessKind::Any
                        | notify::event::AccessKind::Other
                )
            ) {
                return;
            }
            for p in ev.paths {
                if !f.ignore().ignored(&p, is_real_dir(&p)) {
                    let _ = tx.send(Msg::Touch(p));
                }
            }
        },
        // Never follow symlinks: they may lead outside the root or into the store.
        notify::Config::default().with_follow_symlinks(false),
    )
    .ok()?;
    w.watch(fernet.root(), notify::RecursiveMode::Recursive)
        .ok()?;
    Some(w)
}
