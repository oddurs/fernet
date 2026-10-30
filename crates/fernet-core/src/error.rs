//! Resolver errors.

use std::path::Path;

use fernet_fs_formats::DriverError;

/// Resolver result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong resolving or reading a fernet path. Messages
/// are lowercase and name the path relative to the root.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A component doesn't exist.
    #[error("{}", not_found(parent, name, *inside))]
    NotFound {
        /// The path walked so far, relative to the root.
        parent: String,
        /// The missing component.
        name: String,
        /// True if the missing name is a key inside a file.
        inside: bool,
    },
    /// A time lens, but no store (milestone 2).
    #[error("no history for {path}: there is no store here")]
    NoHistory {
        /// The path, relative to the root.
        path: String,
    },
    /// A file has history, but nothing at or before the requested instant.
    #[error("no version of {path} before {before} (first kept: {first})")]
    NoVersion {
        /// The file, relative to the root.
        path: String,
        /// The requested instant, local time.
        before: String,
        /// The first kept version, local time.
        first: String,
    },
    /// No mark by that name.
    #[error("no mark called {name}")]
    NoMark {
        /// The mark.
        name: String,
    },
    /// The path is in the past, or the store is open read-only.
    #[error("{path} is read-only")]
    ReadOnly {
        /// The path, relative to the root.
        path: String,
    },
    /// A store problem.
    #[error(transparent)]
    Store(#[from] fernet_fs_store::Error),
    /// Looked up a name inside a file or value.
    #[error("{path} is not a directory")]
    NotADirectory {
        /// The path, relative to the root.
        path: String,
    },
    /// Read a directory as a file.
    #[error("{path} is a directory")]
    IsADirectory {
        /// The path, relative to the root.
        path: String,
    },
    /// The path leaves the root.
    #[error("{path} is outside the root")]
    OutsideRoot {
        /// The path as given.
        path: String,
    },
    /// A file couldn't be parsed by its driver.
    #[error("{path} is not valid {driver}: {source}")]
    Parse {
        /// The file, relative to the root.
        path: String,
        /// The driver's name.
        driver: &'static str,
        /// The driver's error.
        source: DriverError,
    },
    /// A value doesn't fit the type it replaces.
    #[error("{path}: {message}")]
    InvalidValue {
        /// The path written.
        path: String,
        /// Why.
        message: String,
    },
    /// The format can't make this edit.
    #[error("{path}: {message}")]
    Unsupported {
        /// The path written.
        path: String,
        /// Why.
        message: String,
    },
    /// A key being created already exists.
    #[error("{path} already exists")]
    Exists {
        /// The path.
        path: String,
    },
    /// The file changed on disk while being edited, twice in a row.
    #[error("{path} kept changing while fernet was writing it; nothing was written")]
    Changed {
        /// The file.
        path: String,
    },
    /// An I/O error.
    #[error("{path}: {}", lowercase(source))]
    Io {
        /// The path involved.
        path: String,
        /// The error.
        source: std::io::Error,
    },
}

fn not_found(parent: &str, name: &str, inside: bool) -> String {
    match (inside, parent.is_empty()) {
        (true, _) => format!("no key {name} in {parent}"),
        (false, true) => format!("no such file: {name}"),
        (false, false) => format!("no such file: {parent}/{name}"),
    }
}

fn lowercase(e: &std::io::Error) -> String {
    let s = e.to_string();
    let s = s.split(" (os error").next().unwrap_or(&s);
    s.to_lowercase()
}

impl Error {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }

    /// The errno the mount should report.
    #[must_use]
    pub fn errno(&self) -> i32 {
        // Values are the same on Linux and macOS for these.
        const ENOENT: i32 = 2;
        const EIO: i32 = 5;
        const ENOTDIR: i32 = 20;
        const EISDIR: i32 = 21;
        const EINVAL: i32 = 22;
        const EEXIST: i32 = 17;
        const EAGAIN: i32 = if cfg!(target_os = "macos") { 35 } else { 11 };
        const EOPNOTSUPP: i32 = if cfg!(target_os = "macos") { 102 } else { 95 };
        const EROFS: i32 = 30;
        match self {
            Self::NotFound { .. }
            | Self::NoHistory { .. }
            | Self::OutsideRoot { .. }
            | Self::NoMark { .. }
            | Self::NoVersion { .. } => ENOENT,
            Self::ReadOnly { .. } => EROFS,
            Self::Store(_) => EIO,
            Self::NotADirectory { .. } => ENOTDIR,
            Self::IsADirectory { .. } => EISDIR,
            Self::Parse { .. } | Self::InvalidValue { .. } => EINVAL,
            Self::Unsupported { .. } => EOPNOTSUPP,
            Self::Exists { .. } => EEXIST,
            Self::Changed { .. } => EAGAIN,
            Self::Io { source, .. } => source.raw_os_error().unwrap_or(EIO),
        }
    }
}
