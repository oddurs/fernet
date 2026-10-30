//! The `fernet` command-line interface.

use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use fernet_fs_core::{EntryKind, Error, Fernet, STORE_DIR};

/// every file has depth, every moment is kept.
#[derive(Parser)]
#[command(name = "fernet", version, about, disable_help_subcommand = true)]
struct Cli {
    /// The fernet root. Default: the nearest directory above the current one
    /// that holds a `.fernet/` store, else the current directory.
    #[arg(long, global = true, value_name = "dir")]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print a file, past file or value.
    Cat {
        /// A fernet path.
        path: OsString,
    },
    /// List a directory, version listing or keys.
    Ls {
        /// A fernet path. Default: the current directory.
        path: Option<OsString>,
    },
}

/// Exit codes from the brief, section 4.7.
mod exit {
    pub(crate) const NOT_FOUND: u8 = 1;
    pub(crate) const USAGE: u8 = 2;
    pub(crate) const INVALID: u8 = 3;
    pub(crate) const STORE: u8 = 5;
    pub(crate) const INTERNAL: u8 = 10;
}

fn main() -> ExitCode {
    init_logging();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr() { exit::USAGE } else { 0 };
            let _ = e.print();
            return ExitCode::from(code);
        }
    };
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "fernet: {}", failure.message);
            if let Some(hint) = failure.hint {
                let _ = writeln!(err, "  try: {hint}");
            }
            ExitCode::from(failure.code)
        }
    }
}

/// `FERNET_LOG=debug` turns on tracing; off by default.
fn init_logging() {
    if let Ok(filter) = std::env::var("FERNET_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .try_init();
    }
}

struct Failure {
    message: String,
    hint: Option<String>,
    code: u8,
}

impl Failure {
    fn new(message: impl Into<String>, code: u8) -> Self {
        Self {
            message: message.into(),
            hint: None,
            code,
        }
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Self::new(e.to_string().to_lowercase(), exit::INTERNAL)
    }
}

/// Maps a resolver error to a message, a next step and an exit code.
fn failure(e: &Error, arg: &OsStr) -> Failure {
    let shown = arg.to_string_lossy();
    match e {
        Error::NotFound {
            parent,
            name,
            inside,
        } => {
            let mut f = Failure::new(e.to_string(), exit::NOT_FOUND);
            if name.contains('@') && !*inside {
                f = f.hint("check the time spec; see docs/GRAMMAR.md (example: notes.md@-2h)");
            } else {
                let at = if parent.is_empty() {
                    ".".to_owned()
                } else {
                    parent.clone()
                };
                f = f.hint(format!("fernet ls {at}"));
            }
            f
        }
        Error::NoHistory { .. } => Failure::new(e.to_string(), exit::STORE).hint("fernet init"),
        Error::IsADirectory { .. } => {
            Failure::new(e.to_string(), exit::USAGE).hint(format!("fernet ls {shown}"))
        }
        Error::NotADirectory { .. } => {
            Failure::new(e.to_string(), exit::USAGE).hint(format!("fernet cat {shown}"))
        }
        Error::OutsideRoot { .. } => {
            Failure::new(e.to_string(), exit::USAGE).hint("pass --root <dir> to widen the root")
        }
        Error::Parse { .. } => Failure::new(e.to_string(), exit::INVALID),
        Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            Failure::new(e.to_string(), exit::NOT_FOUND)
        }
        Error::Io { .. } => Failure::new(e.to_string(), exit::INTERNAL),
    }
}

/// The nearest ancestor of `cwd` (inclusive) holding a store, else `cwd`.
fn find_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|d| d.join(STORE_DIR).is_dir())
        .unwrap_or(cwd)
        .to_owned()
}

fn run(cli: &Cli) -> Result<(), Failure> {
    let cwd = std::env::current_dir()?;
    let root = cli.root.clone().unwrap_or_else(|| find_root(&cwd));
    let fernet = Fernet::open(&root).map_err(|e| failure(&e, root.as_os_str()))?;
    // Resolve relative to the canonical cwd so it lines up with the root.
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let mut out = std::io::stdout().lock();
    match &cli.command {
        Command::Cat { path } => {
            let r = fernet
                .resolve_user_path(&cwd, path)
                .map_err(|e| failure(&e, path))?;
            let bytes = fernet.read(&r).map_err(|e| failure(&e, path))?;
            out.write_all(&bytes)?;
        }
        Command::Ls { path } => {
            let path = path.clone().unwrap_or_else(|| ".".into());
            let r = fernet
                .resolve_user_path(&cwd, &path)
                .map_err(|e| failure(&e, &path))?;
            let entries = fernet.list(&r).map_err(|e| failure(&e, &path))?;
            let tty = out.is_terminal();
            for e in entries {
                out.write_all(e.name.as_encoded_bytes())?;
                if tty && e.kind == EntryKind::Dir {
                    out.write_all(b"/")?;
                }
                out.write_all(b"\n")?;
            }
        }
    }
    out.flush()?;
    Ok(())
}
