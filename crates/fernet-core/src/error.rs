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
        match self {
            Self::NotFound { .. } | Self::NoHistory { .. } | Self::OutsideRoot { .. } => ENOENT,
            Self::NotADirectory { .. } => ENOTDIR,
            Self::IsADirectory { .. } => EISDIR,
            Self::Parse { .. } => EINVAL,
            Self::Io { source, .. } => source.raw_os_error().unwrap_or(EIO),
        }
    }
}
