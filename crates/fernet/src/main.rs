//! The `fernet` command-line interface.

mod style;

use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use fernet_fs_core::{EntryKind, Error, Fernet, Options, Resolved, STORE_DIR};
use fernet_fs_path::FernetPath;
use fernet_fs_store::{Kind, Mode};

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
    /// Create .fernet/ in dir and capture the first snowfall.
    Init {
        /// The directory. Default: the current directory.
        dir: Option<PathBuf>,
    },
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
    /// Core sample: the history of a file or value, newest first.
    Core {
        /// A fernet path.
        path: OsString,
    },
    /// Versions, newest first.
    Log {
        /// Only this file.
        path: Option<OsString>,
        /// Show how much space the store uses.
        #[arg(long)]
        size: bool,
    },
    /// Add a tephra mark, or remove one with --rm.
    Mark {
        /// The mark name: a-z, 0-9 and '-', starting with a letter.
        name: String,
        /// Remove the mark instead.
        #[arg(long)]
        rm: bool,
    },
    /// Unified diff of any two fernet paths.
    Diff {
        /// The old side.
        a: OsString,
        /// The new side.
        b: OsString,
    },
    /// Check the log, blobs and manifest.
    Verify,
    /// Rebuild the index from the log.
    Reindex,
    /// Mount dir at mountpoint (read-only), in the foreground unless --daemon.
    #[cfg(feature = "mount")]
    Mount {
        /// The backing directory.
        dir: PathBuf,
        /// Where to mount it.
        mountpoint: PathBuf,
        /// Detach and run in the background.
        #[arg(long)]
        daemon: bool,
    },
    /// Capture changes in the foreground until interrupted.
    Watch {
        /// The directory. Default: the root.
        dir: Option<PathBuf>,
    },
}

/// Exit codes from the brief, section 4.7.
mod exit {
    pub(crate) const NOT_FOUND: u8 = 1;
    pub(crate) const USAGE: u8 = 2;
    pub(crate) const INVALID: u8 = 3;
    pub(crate) const READ_ONLY: u8 = 4;
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
        Ok(code) => ExitCode::from(code),
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
    let msg = e.to_string();
    match e {
        Error::NotFound {
            parent,
            name,
            inside,
        } => {
            let f = Failure::new(msg, exit::NOT_FOUND);
            if name.contains('@') && !*inside {
                let base = name.rsplit_once('@').map_or(name.as_str(), |(b, _)| b);
                let at = if parent.is_empty() {
                    base.to_owned()
                } else {
                    format!("{parent}/{base}")
                };
                f.hint(format!("fernet ls {at}@"))
            } else {
                let at = if parent.is_empty() {
                    ".".to_owned()
                } else {
                    parent.clone()
                };
                f.hint(format!("fernet ls {at}"))
            }
        }
        Error::NoHistory { .. } => Failure::new(msg, exit::STORE).hint("fernet init"),
        Error::NoMark { .. } => Failure::new(msg, exit::NOT_FOUND).hint("fernet log"),
        Error::NoVersion { path, .. } => {
            Failure::new(msg, exit::NOT_FOUND).hint(format!("fernet ls {path}@"))
        }
        Error::ReadOnly { .. } => Failure::new(msg, exit::READ_ONLY),
        Error::IsADirectory { .. } => {
            Failure::new(msg, exit::USAGE).hint(format!("fernet ls {shown}"))
        }
        Error::NotADirectory { .. } => {
            Failure::new(msg, exit::USAGE).hint(format!("fernet cat {shown}"))
        }
        Error::OutsideRoot { .. } => {
            Failure::new(msg, exit::USAGE).hint("pass --root <dir> to widen the root")
        }
        Error::Parse { .. } => Failure::new(msg, exit::INVALID),
        Error::Store(s) => store_failure(s),
        Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            Failure::new(msg, exit::NOT_FOUND)
        }
        Error::Io { .. } => Failure::new(msg, exit::INTERNAL),
    }
}

fn store_failure(e: &fernet_fs_store::Error) -> Failure {
    use fernet_fs_store::Error as S;
    let msg = e.to_string().to_lowercase();
    match e {
        S::Mark(_) => Failure::new(msg, exit::INVALID),
        S::Busy => Failure::new(msg, exit::STORE)
            .hint("stop the other fernet process (fernet watch or a mount)"),
        S::ReadOnly => Failure::new(msg, exit::READ_ONLY),
        S::Exists(_) => Failure::new(msg, exit::USAGE),
        S::NotAStore(_) => Failure::new(msg, exit::STORE).hint("fernet init"),
        S::Corrupt { .. } | S::BadBlob { .. } => {
            Failure::new(msg, exit::STORE).hint("fernet verify")
        }
        _ => Failure::new(msg, exit::STORE),
    }
}

/// The nearest ancestor of `cwd` (inclusive) holding a store, else `cwd`.
fn find_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|d| d.join(STORE_DIR).is_dir())
        .unwrap_or(cwd)
        .to_owned()
}

struct Ctx {
    fernet: Fernet,
    cwd: PathBuf,
}

impl Ctx {
    fn open(cli: &Cli, mode: Mode) -> Result<Self, Failure> {
        let cwd = std::env::current_dir()?;
        let root = cli.root.clone().unwrap_or_else(|| find_root(&cwd));
        let fernet = Fernet::open_with(
            &root,
            Options {
                mode,
                ..Options::default()
            },
        )
        .map_err(|e| failure(&e, root.as_os_str()))?;
        // Resolve relative to the canonical cwd so it lines up with the root.
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        Ok(Self { fernet, cwd })
    }

    fn resolve(&self, arg: &OsStr) -> Result<Resolved, Failure> {
        self.fernet
            .resolve_user_path(&self.cwd, arg)
            .map_err(|e| failure(&e, arg))
    }

    fn read(&self, arg: &OsStr) -> Result<Vec<u8>, Failure> {
        let r = self.resolve(arg)?;
        self.fernet.read(&r).map_err(|e| failure(&e, arg))
    }

    /// A user path as a root-relative fernet path (for whole-path operations).
    fn fernet_path(&self, arg: &OsStr) -> Result<FernetPath, Failure> {
        let names = self
            .fernet
            .user_names(&self.cwd, arg)
            .map_err(|e| failure(&e, arg))?;
        let text: Vec<String> = names
            .iter()
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        FernetPath::parse(&text.join("/")).map_err(|e| Failure::new(e.to_string(), exit::USAGE))
    }

    fn need_store(&self) -> Result<std::sync::MutexGuard<'_, fernet_fs_store::Store>, Failure> {
        self.fernet
            .store()
            .ok_or_else(|| Failure::new("there is no store here", exit::STORE).hint("fernet init"))
    }
}

fn run(cli: &Cli) -> Result<u8, Failure> {
    let mut out = std::io::stdout().lock();
    let colour = style::colour(out.is_terminal());
    match &cli.command {
        Command::Init { dir } => init(cli, dir.as_deref(), &mut out)?,
        Command::Cat { path } => {
            let ctx = Ctx::open(cli, Mode::Read)?;
            out.write_all(&ctx.read(path)?)?;
        }
        Command::Ls { path } => {
            let ctx = Ctx::open(cli, Mode::Read)?;
            let path = path.clone().unwrap_or_else(|| ".".into());
            let r = ctx.resolve(&path)?;
            let entries = ctx.fernet.list(&r).map_err(|e| failure(&e, &path))?;
            for e in entries {
                out.write_all(e.name.as_encoded_bytes())?;
                if out.is_terminal() && e.kind == EntryKind::Dir {
                    out.write_all(b"/")?;
                }
                out.write_all(b"\n")?;
            }
        }
        Command::Core { path } => {
            let ctx = Ctx::open(cli, Mode::Read)?;
            let p = ctx.fernet_path(path)?;
            let strata = ctx.fernet.core_sample(&p).map_err(|e| failure(&e, path))?;
            let marks = ctx.need_store()?.marks().map_err(|e| store_failure(&e))?;
            core_sample(&mut out, &ctx, &strata, &marks, colour)?;
        }
        Command::Log { path, size } => {
            let ctx = Ctx::open(cli, Mode::Read)?;
            if *size {
                let u = ctx.need_store()?.usage().map_err(|e| store_failure(&e))?;
                writeln!(out, "log      {}", style::size(u.log))?;
                writeln!(
                    out,
                    "objects  {} in {} blobs",
                    style::size(u.objects),
                    u.blobs
                )?;
                writeln!(out, "index    {}", style::size(u.index))?;
            } else {
                log(&mut out, &ctx, path.as_deref(), colour)?;
            }
        }
        Command::Mark { name, rm } => {
            let ctx = Ctx::open(cli, Mode::Write)?;
            let r = if *rm {
                ctx.fernet.unmark(name)
            } else {
                ctx.fernet.mark(name)
            };
            r.map_err(|e| failure(&e, OsStr::new(name)))?;
        }
        Command::Diff { a, b } => diff(cli, a, b, &mut out)?,
        Command::Verify => return verify(cli, &mut out),
        Command::Reindex => {
            let ctx = Ctx::open(cli, Mode::Write)?;
            let n = ctx.need_store()?.reindex().map_err(|e| store_failure(&e))?;
            writeln!(out, "reindexed {n} records")?;
        }
        #[cfg(feature = "mount")]
        Command::Mount {
            dir,
            mountpoint,
            daemon,
        } => {
            drop(out);
            return mount(dir, mountpoint, *daemon);
        }
        Command::Watch { dir } => {
            drop(out);
            return start_watch(cli, dir.as_deref());
        }
    }
    out_flush()?;
    Ok(0)
}

fn init(cli: &Cli, dir: Option<&Path>, out: &mut impl Write) -> Result<(), Failure> {
    let dir = match dir.or(cli.root.as_deref()) {
        Some(d) => d.to_owned(),
        None => std::env::current_dir()?,
    };
    let f = Fernet::init(&dir, Options::default()).map_err(|e| failure(&e, dir.as_os_str()))?;
    let n = f
        .snapshot_tree(f.root())
        .map_err(|e| failure(&e, dir.as_os_str()))?;
    let files = if n == 1 { "file" } else { "files" };
    writeln!(
        out,
        "initialised {} ({n} {files} captured)",
        dir.join(STORE_DIR).display()
    )?;
    Ok(())
}

fn diff(cli: &Cli, a: &OsStr, b: &OsStr, out: &mut impl Write) -> Result<(), Failure> {
    let ctx = Ctx::open(cli, Mode::Read)?;
    let (old, new) = (ctx.read(a)?, ctx.read(b)?);
    if old == new {
        return Ok(());
    }
    let (a, b) = (a.to_string_lossy(), b.to_string_lossy());
    match (std::str::from_utf8(&old), std::str::from_utf8(&new)) {
        (Ok(o), Ok(n)) => {
            let diff = similar::TextDiff::from_lines(o, n);
            let mut unified = diff.unified_diff();
            out.write_all(unified.header(&a, &b).to_string().as_bytes())?;
        }
        _ => writeln!(out, "binary files {a} and {b} differ")?,
    }
    Ok(())
}

fn verify(cli: &Cli, out: &mut impl Write) -> Result<u8, Failure> {
    let cwd = std::env::current_dir()?;
    let root = cli.root.clone().unwrap_or_else(|| find_root(&cwd));
    let report = fernet_fs_store::verify(&root).map_err(|e| store_failure(&e))?;
    for p in &report.problems {
        writeln!(out, "problem: {p}")?;
    }
    for n in &report.notes {
        writeln!(out, "note: {n}")?;
    }
    writeln!(
        out,
        "{} segments, {} records, {} blobs",
        report.segments, report.records, report.blobs
    )?;
    out.flush()?;
    Ok(if report.ok() { 0 } else { exit::STORE })
}

fn start_watch(cli: &Cli, dir: Option<&Path>) -> Result<u8, Failure> {
    let root = match dir.or(cli.root.as_deref()) {
        Some(d) => d.to_owned(),
        None => find_root(&std::env::current_dir()?),
    };
    let f = Fernet::open_with(
        &root,
        Options {
            mode: Mode::Write,
            ..Options::default()
        },
    )
    .map_err(|e| failure(&e, root.as_os_str()))?;
    if f.store().is_none() {
        return Err(Failure::new("there is no store here", exit::STORE).hint("fernet init"));
    }
    watch(&f)?;
    Ok(0)
}

/// Whether something is mounted at `path`: its device differs from its parent's.
#[cfg(feature = "mount")]
fn is_mounted(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let parent = path.parent().unwrap_or(path);
    match (std::fs::metadata(path), std::fs::metadata(parent)) {
        (Ok(a), Ok(b)) => a.dev() != b.dev(),
        _ => false,
    }
}

#[cfg(feature = "mount")]
fn mount(dir: &Path, mountpoint: &Path, daemon: bool) -> Result<u8, Failure> {
    let mountpoint = std::fs::canonicalize(mountpoint).map_err(|e| {
        Failure::new(
            format!("{}: {}", mountpoint.display(), e).to_lowercase(),
            exit::USAGE,
        )
    })?;
    if daemon {
        // Re-run in the foreground, detached from this terminal's process group.
        use std::os::unix::process::CommandExt;
        let exe = std::env::current_exe()?;
        let mut child = std::process::Command::new(exe)
            .arg("mount")
            .arg(dir)
            .arg(&mountpoint)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()?;
        for _ in 0..100 {
            if is_mounted(&mountpoint) {
                return Ok(0);
            }
            if let Some(status) = child.try_wait()? {
                return Err(
                    Failure::new(format!("mount failed ({status})"), exit::INTERNAL).hint(format!(
                        "fernet mount {} {}",
                        dir.display(),
                        mountpoint.display()
                    )),
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        return Err(Failure::new(
            "mount didn't appear within 5 s",
            exit::INTERNAL,
        ));
    }
    let fernet = Fernet::open_with(
        dir,
        Options {
            mode: Mode::Read,
            ..Options::default()
        },
    )
    .map_err(|e| failure(&e, dir.as_os_str()))?;
    let root = fernet.root().to_owned();
    if mountpoint.starts_with(&root) {
        return Err(Failure::new(
            "the mountpoint can't be inside the backing directory",
            exit::USAGE,
        ));
    }
    let session = fernet_fs_fuse::spawn(std::sync::Arc::new(fernet), &mountpoint).map_err(|e| {
        Failure::new(
            format!("can't mount at {}: {e}", mountpoint.display()).to_lowercase(),
            exit::INTERNAL,
        )
    })?;
    let (tx, rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .map_err(|e| {
        Failure::new(
            format!("can't handle signals: {e}").to_lowercase(),
            exit::INTERNAL,
        )
    })?;
    let mut err = std::io::stderr().lock();
    writeln!(
        err,
        "mounted {} at {}; ctrl-c to unmount",
        root.display(),
        mountpoint.display()
    )?;
    loop {
        if rx.recv_timeout(Duration::from_millis(250)).is_ok() {
            session.umount_and_join()?;
            return Ok(0);
        }
        // Unmounted from outside (fusermount -u, umount).
        if session.guard.is_finished() {
            session.join()?;
            return Ok(0);
        }
    }
}

fn out_flush() -> std::io::Result<()> {
    std::io::stdout().flush()
}

fn core_sample(
    out: &mut impl Write,
    ctx: &Ctx,
    strata: &[fernet_fs_core::Stratum],
    marks: &std::collections::BTreeMap<String, i64>,
    colour: bool,
) -> std::io::Result<()> {
    let now = ctx.fernet.now();
    let tz = ctx.fernet.tz();
    // Marks, newest first. A mark is drawn above the stratum that was
    // current when it was made: everything above the rule came after it.
    let mut marks: Vec<(&String, i64)> = marks.iter().map(|(n, t)| (n, *t)).collect();
    marks.sort_by_key(|&(_, t)| std::cmp::Reverse(t));
    let mut pending = marks.into_iter().peekable();
    for (i, s) in strata.iter().enumerate() {
        while let Some((name, _)) = pending.next_if(|&(_, t)| t >= s.since) {
            writeln!(out, "{}", style::dim(&style::tephra(name), colour))?;
        }
        let label = if i == 0 {
            "now".to_owned()
        } else {
            style::when(s.since, now, tz)
        };
        let value = match &s.value {
            Some(v) => String::from_utf8_lossy(v)
                .trim_end_matches('\n')
                .replace('\n', "⏎"),
            None => style::dim("(none)", colour),
        };
        writeln!(
            out,
            "{}  {value}",
            style::dim(&format!("{label:<10}"), colour)
        )?;
    }
    Ok(())
}

fn log(out: &mut impl Write, ctx: &Ctx, path: Option<&OsStr>, colour: bool) -> Result<(), Failure> {
    let only = match path {
        Some(p) => {
            let names = ctx
                .fernet
                .user_names(&ctx.cwd, p)
                .map_err(|e| failure(&e, p))?;
            Some(
                names
                    .iter()
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/"),
            )
        }
        None => None,
    };
    let records = ctx.need_store()?.records().map_err(|e| store_failure(&e))?;
    let tz = ctx.fernet.tz();
    for (_, r) in records.iter().rev() {
        let line = match r.kind {
            Kind::Mark => Some(style::dim(&style::tephra(&r.extra), colour)),
            Kind::Unmark => None,
            _ if only.as_ref().is_some_and(|p| *p != r.path && *p != r.extra) => None,
            Kind::Write => Some(format!(
                "{}  {:>9}  {}",
                style::dim(&style::stamp(r.time, tz), colour),
                style::size(r.size),
                r.path
            )),
            Kind::Delete => Some(format!(
                "{}  {:>9}  {}",
                style::dim(&style::stamp(r.time, tz), colour),
                "deleted",
                r.path
            )),
            Kind::Rename => Some(format!(
                "{}  {:>9}  {} ← {}",
                style::dim(&style::stamp(r.time, tz), colour),
                style::size(r.size),
                r.path,
                r.extra
            )),
        };
        if let Some(line) = line {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

/// Captures changes until interrupted. Events are debounced: a batch is
/// captured once the tree has been quiet for 200 ms.
fn watch(f: &Fernet) -> Result<(), Failure> {
    use notify::{RecursiveMode, Watcher};
    let root = f.root().to_owned();
    let mut err = std::io::stderr().lock();
    let n = f
        .snapshot_tree(&root)
        .map_err(|e| failure(&e, root.as_os_str()))?;
    writeln!(
        err,
        "watching {} ({n} captured at start); ctrl-c to stop",
        root.display()
    )?;
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev.paths);
        }
    })
    .map_err(|e| Failure::new(format!("can't watch: {e}").to_lowercase(), exit::INTERNAL))?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| Failure::new(format!("can't watch: {e}").to_lowercase(), exit::INTERNAL))?;
    let quiet = Duration::from_millis(200);
    loop {
        let Ok(first) = rx.recv() else { return Ok(()) };
        let mut batch: Vec<PathBuf> = first;
        while let Ok(more) = rx.recv_timeout(quiet) {
            batch.extend(more);
        }
        batch.sort();
        batch.dedup();
        for path in batch {
            let is_dir = path.is_dir();
            if f.ignore().ignored(&path, is_dir) {
                continue;
            }
            let res = if is_dir {
                f.snapshot_tree(&path).map(|n| n > 0)
            } else {
                f.snapshot(&path).map(|v| v.is_some())
            };
            match res {
                Ok(true) => {
                    let shown = path
                        .strip_prefix(&root)
                        .unwrap_or(&path)
                        .display()
                        .to_string();
                    writeln!(
                        err,
                        "{}",
                        style::dim(
                            &format!("snowfall  {shown}"),
                            style::colour(err.is_terminal())
                        )
                    )?;
                }
                Ok(false) => {}
                Err(e) => writeln!(err, "fernet: {e}")?,
            }
        }
        f.flush().map_err(|e| failure(&e, root.as_os_str()))?;
    }
}
