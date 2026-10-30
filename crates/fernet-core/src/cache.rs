//! A bounded cache of parsed trees.

use std::collections::VecDeque;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use fernet_fs_formats::Tree;

/// Cache key: what makes a parse reusable.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    /// A present file, identified by path, mtime and size.
    Present {
        path: PathBuf,
        mtime: Option<SystemTime>,
        size: u64,
    },
    /// A blob. The path is part of the key because it picks the driver.
    Blob { path: PathBuf, hash: [u8; 32] },
}

impl Key {
    pub(crate) fn present(path: &Path, meta: &Metadata) -> Self {
        Self::Present {
            path: path.to_owned(),
            mtime: meta.modified().ok(),
            size: meta.len(),
        }
    }
}

/// Parsed trees, least recently used first out, bounded by the total size of
/// their source files (default 64 MiB).
pub struct TreeCache {
    inner: Mutex<Inner>,
}

struct Inner {
    budget: usize,
    used: usize,
    /// Most recently used at the back.
    entries: VecDeque<(Key, Arc<Tree>, usize)>,
}

impl Default for TreeCache {
    fn default() -> Self {
        Self::with_budget(64 << 20)
    }
}

impl TreeCache {
    /// A cache holding trees whose sources total at most `budget` bytes.
    #[must_use]
    pub fn with_budget(budget: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                budget,
                used: 0,
                entries: VecDeque::new(),
            }),
        }
    }

    pub(crate) fn get(&self, key: &Key) -> Option<Arc<Tree>> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let i = inner.entries.iter().position(|(k, _, _)| k == key)?;
        let entry = inner.entries.remove(i)?;
        let tree = Arc::clone(&entry.1);
        inner.entries.push_back(entry);
        Some(tree)
    }

    pub(crate) fn insert(&self, key: Key, tree: Arc<Tree>, size: usize) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if size > inner.budget {
            return;
        }
        // Drop stale parses of the same present file.
        if let Key::Present { path, .. } = &key {
            let path = path.clone();
            inner
                .entries
                .retain(|(k, _, _)| !matches!(k, Key::Present { path: p, .. } if *p == path));
        }
        inner.used = inner.entries.iter().map(|e| e.2).sum();
        while inner.used + size > inner.budget {
            let Some((_, _, freed)) = inner.entries.pop_front() else {
                break;
            };
            inner.used -= freed;
        }
        inner.used += size;
        inner.entries.push_back((key, tree, size));
    }
}
