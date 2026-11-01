//! Milestone 4: writing through the mount. Linux only; skipped without FUSE
//! unless `FERNET_REQUIRE_FUSE=1`.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Quiet period configured for these tests.
const WINDOW: Duration = Duration::from_millis(300);
/// How long a debounced capture may take on a slow machine.
const DEADLINE: Duration = Duration::from_secs(10);

fn fuse_available() -> bool {
    let ok = Path::new("/dev/fuse").exists()
        && Command::new("fusermount3")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
    assert!(
        ok || std::env::var_os("FERNET_REQUIRE_FUSE").is_none(),
        "FUSE is required (FERNET_REQUIRE_FUSE) but unavailable"
    );
    ok
}

fn bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("fernet")
}

struct Mounted {
    _tmp: tempfile::TempDir,
    backing: PathBuf,
    mnt: PathBuf,
}

impl Mounted {
    fn unmount(&self) {
        let _ = Command::new("fusermount3")
            .arg("-u")
            .arg(&self.mnt)
            .status();
    }

    /// Waits until `file` has exactly `n` kept versions, then holds for two
    /// more debounce windows and checks it didn't go past `n`. Waiting on the
    /// count, not a fixed sleep, keeps slow machines from failing; holding
    /// keeps "exactly one version per save" strict.
    fn settle(&self, file: &str, n: usize) {
        let start = std::time::Instant::now();
        while self.versions(file) < n && start.elapsed() < DEADLINE {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(WINDOW * 2);
        assert_eq!(self.versions(file), n, "{file}");
    }

    /// Kept versions of `file`, via the CLI (the mount holds the store).
    fn versions(&self, file: &str) -> usize {
        let out = Command::new(bin())
            .args(["ls", &format!("{file}@")])
            .current_dir(&self.backing)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.starts_with("20"))
            .count()
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        self.unmount();
    }
}

fn mount(files: &[(&str, &str)]) -> Option<Mounted> {
    mount_with(files, &[])
}

fn mount_with(files: &[(&str, &str)], env: &[(&str, &str)]) -> Option<Mounted> {
    if !fuse_available() {
        eprintln!("skipping: FUSE unavailable");
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    let backing = tmp.path().join("backing");
    let mnt = tmp.path().join("mnt");
    fs::create_dir_all(&backing).unwrap();
    fs::create_dir_all(&mnt).unwrap();
    for (p, c) in files {
        let p = backing.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, c).unwrap();
    }
    assert!(
        Command::new(bin())
            .arg("init")
            .current_dir(&backing)
            .status()
            .unwrap()
            .success()
    );
    let config = backing.join(".fernet/config.toml");
    let text = fs::read_to_string(&config).unwrap().replace(
        "debounce_ms = 2000",
        &format!("debounce_ms = {}", WINDOW.as_millis()),
    );
    fs::write(&config, text).unwrap();
    assert!(
        Command::new(bin())
            .arg("mount")
            .arg(&backing)
            .arg(&mnt)
            .arg("--daemon")
            .envs(env.iter().copied())
            .status()
            .unwrap()
            .success()
    );
    Some(Mounted {
        _tmp: tmp,
        backing,
        mnt,
    })
}

#[test]
fn values_written_through_hash_paths() {
    let Some(m) = mount(&[
        (
            "config.json",
            "{\n  \"server\": {\n    \"port\": 8080,\n    \"name\": \"api\"\n  },\n  \"tags\": [\"a\"]\n}\n",
        ),
        ("c.toml", "[db]\nurl = 'x'\npool = 4\n"),
        ("c.yaml", "image: api:1\nreplicas: 2\n"),
    ]) else {
        return;
    };
    let mnt = &m.mnt;
    let sh = |cmd: &str| {
        Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(mnt)
            .status()
            .unwrap()
            .success()
    };
    assert!(sh("echo 9090 > 'config.json#/server/port'"));
    assert!(sh("printf 'new name' > 'config.json#/server/name'"));
    assert!(sh("echo 3 > 'config.json#/server/workers'"));
    assert!(sh("echo b > 'config.json#/tags/1'"));
    assert!(sh("mkdir 'config.json#/limits'"));
    assert!(sh("echo 'y' > 'c.toml#/db/url'"));
    assert!(sh("echo 8 > 'c.toml#/db/pool'"));
    assert!(sh("rm 'c.toml#/db/pool'"));
    assert!(sh("echo api:2 > 'c.yaml#/image'"));
    assert_eq!(
        fs::read_to_string(m.backing.join("config.json")).unwrap(),
        "{\n  \"server\": {\n    \"port\": 9090,\n    \"name\": \"new name\",\n    \"workers\": 3\n  },\n  \"tags\": [\"a\", \"b\"],\n  \"limits\": {}\n}\n"
    );
    assert_eq!(
        fs::read_to_string(m.backing.join("c.toml")).unwrap(),
        "[db]\nurl = 'y'\n"
    );
    assert_eq!(
        fs::read_to_string(m.backing.join("c.yaml")).unwrap(),
        "image: api:2\nreplicas: 2\n"
    );
    // Reads through the mount see the new values.
    assert_eq!(
        fs::read_to_string(mnt.join("config.json#/server/workers")).unwrap(),
        "3\n"
    );
    // YAML can't add keys.
    assert!(!sh("echo x > 'c.yaml#/new' 2>/dev/null"));
    // Under @ everything is read-only.
    assert!(!sh("echo 1 > 'config.json@first#/server/port' 2>/dev/null"));
    assert!(!sh("touch 'config.json@first' 2>/dev/null"));
    // .fernet can't be created or reached.
    assert!(!sh("mkdir .fernet 2>/dev/null"));
}

#[test]
fn wrong_type_is_einval_and_changes_nothing() {
    let Some(m) = mount(&[("config.json", "{\"port\": 8080}\n")]) else {
        return;
    };
    let before = fs::read(m.backing.join("config.json")).unwrap();
    let mut f = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(m.mnt.join("config.json#/port"))
        .unwrap();
    f.write_all(b"not a number\n").unwrap();
    let err = f.sync_all().unwrap_err();
    assert_eq!(err.raw_os_error(), Some(22), "{err}");
    drop(f);
    assert_eq!(fs::read(m.backing.join("config.json")).unwrap(), before);
    // The CLI says the same with exit code 3.
    m.unmount();
    let out = Command::new(bin())
        .args(["write", "config.json#/port", "nope"])
        .current_dir(&m.backing)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(fs::read(m.backing.join("config.json")).unwrap(), before);
}

/// Saves `content` to `file` the way vim does by default: probe `4913`, swap
/// file, rename the original to `file~`, write a new file, delete the backup.
fn vim_save(dir: &Path, file: &str, content: &str) {
    let swap = dir.join(format!(".{file}.swp"));
    fs::write(&swap, "swap").unwrap();
    let probe = dir.join("4913");
    fs::write(&probe, "").unwrap();
    fs::remove_file(&probe).unwrap();
    let backup = dir.join(format!("{file}~"));
    fs::rename(dir.join(file), &backup).unwrap();
    let mut f = fs::File::create(dir.join(file)).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.sync_all().unwrap();
    drop(f);
    fs::remove_file(&backup).unwrap();
    fs::remove_file(&swap).unwrap();
}

/// VS Code writes in place: truncate and write.
fn vscode_save(dir: &Path, file: &str, content: &str) {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(dir.join(file))
        .unwrap();
    f.write_all(content.as_bytes()).unwrap();
    drop(f);
}

/// `JetBrains` "safe write": write a temp file, move the original aside, move
/// the temp file into place, delete the old one.
fn jetbrains_save(dir: &Path, file: &str, content: &str) {
    let tmp = dir.join(format!("{file}___jb_tmp___"));
    let old = dir.join(format!("{file}___jb_old___"));
    fs::write(&tmp, content).unwrap();
    fs::rename(dir.join(file), &old).unwrap();
    fs::rename(&tmp, dir.join(file)).unwrap();
    fs::remove_file(&old).unwrap();
}

/// Emacs: a lock symlink, a `file~` backup by rename, then a fresh file.
fn emacs_save(dir: &Path, file: &str, content: &str) {
    let lock = dir.join(format!(".#{file}"));
    std::os::unix::fs::symlink("user@host.123", &lock).unwrap();
    fs::rename(dir.join(file), dir.join(format!("{file}~"))).unwrap();
    fs::write(dir.join(file), content).unwrap();
    fs::remove_file(&lock).unwrap();
}

/// An editor's save routine: directory, file name, new content.
type Save = fn(&Path, &str, &str);

#[test]
fn editor_saves_are_one_version_each() {
    let Some(m) = mount(&[("notes.md", "v0\n")]) else {
        return;
    };
    let editors: [(&str, Save); 4] = [
        ("vim", vim_save),
        ("vscode", vscode_save),
        ("jetbrains", jetbrains_save),
        ("emacs", emacs_save),
    ];
    let mut expected = m.versions("notes.md");
    assert_eq!(expected, 1);
    for (name, save) in editors {
        for round in 0..2 {
            save(&m.mnt, "notes.md", &format!("{name} {round}\n"));
            expected += 1;
            m.settle("notes.md", expected);
            assert_eq!(
                fs::read_to_string(m.backing.join("notes.md")).unwrap(),
                format!("{name} {round}\n")
            );
        }
    }
    // No temp file left a trace in history.
    let log = Command::new(bin())
        .arg("log")
        .current_dir(&m.backing)
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    for junk in ["4913", ".swp", "~", "___jb_", ".#"] {
        assert!(!log.contains(junk), "{junk} in history:\n{log}");
    }
    // Two quick saves inside one window coalesce.
    vscode_save(&m.mnt, "notes.md", "quick 1\n");
    vscode_save(&m.mnt, "notes.md", "quick 2\n");
    m.settle("notes.md", expected + 1);
}

#[test]
fn outside_changes_are_captured_while_mounted() {
    let Some(m) = mount(&[("a.txt", "one\n")]) else {
        return;
    };
    fs::write(m.backing.join("a.txt"), "two\n").unwrap();
    fs::write(m.backing.join("b.txt"), "new\n").unwrap();
    m.settle("a.txt", 2);
    m.settle("b.txt", 1);
    // A write through the mount isn't counted twice by the watcher.
    fs::write(m.mnt.join("a.txt"), "three\n").unwrap();
    m.settle("a.txt", 3);
}

/// Every regular file under `dir` (relative path → bytes), skipping `.fernet`.
fn snapshot_tree(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
            if rel == ".fernet" {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                stack.push(p);
            } else {
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    out
}

#[test]
fn invariant_1_deleting_the_store_keeps_current_data() {
    let Some(m) = mount(&[("config.json", "{\"a\": 1}\n"), ("docs/readme.md", "hi\n")]) else {
        return;
    };
    let mnt = &m.mnt;
    // A session of edits through every write path.
    fs::write(mnt.join("config.json#/a"), "2\n").unwrap();
    fs::write(mnt.join("config.json#/b"), "text\n").unwrap();
    fs::create_dir(mnt.join("src")).unwrap();
    fs::write(mnt.join("src/main.rs"), "fn main() {}\n").unwrap();
    vim_save(mnt, "config.json", "{\"a\": 3}\n");
    jetbrains_save(&mnt.join("docs"), "readme.md", "hello\n");
    fs::rename(mnt.join("src/main.rs"), mnt.join("src/lib.rs")).unwrap();
    fs::remove_file(mnt.join("docs/readme.md")).unwrap();
    fs::write(mnt.join("docs/new.md"), "new\n").unwrap();
    let shown = snapshot_tree(mnt);
    m.unmount();
    std::thread::sleep(Duration::from_millis(200));
    fs::remove_dir_all(m.backing.join(".fernet")).unwrap();
    assert_eq!(snapshot_tree(&m.backing), shown);
    assert_eq!(
        shown.get("config.json").map(Vec::as_slice),
        Some(&b"{\"a\": 3}\n"[..])
    );
}

/// Regressions from review: replacing files while handles are open,
/// recreate-after-unlink, creates read back, huge truncates, FIFOs, symlinks,
/// renames under open value handles, chown.
#[test]
fn replacement_and_edge_cases() {
    use std::os::unix::fs::MetadataExt;
    let Some(m) = mount(&[
        ("d.json", "{\"n\": 1}\n"),
        ("f.txt", "hello\n"),
        ("o1", "target\n"),
    ]) else {
        return;
    };
    let mnt = &m.mnt;
    let sh = |cmd: &str| {
        let out = Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(mnt)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };

    // A # write replaces the file (temp + rename) while a reader holds it open:
    // later reads see the new content, and writes land in the new file.
    let (ok, out) = sh("exec 5<d.json; echo 42 > 'd.json#/n'; cat d.json");
    assert!(ok);
    assert_eq!(out, "{\"n\": 42}\n");
    let (ok, _) = sh("exec 5<d.json; echo 7 > 'd.json#/n'; echo OVERWRITE > d.json");
    assert!(ok);
    assert_eq!(
        fs::read_to_string(m.backing.join("d.json")).unwrap(),
        "OVERWRITE\n"
    );

    // Unlink with an open fd, then recreate: the new file is a new file.
    let (ok, out) = sh("exec 4<f.txt; rm f.txt; echo NEWDATA > f.txt; cat f.txt; cat <&4");
    assert!(ok, "{out}");
    assert_eq!(out, "NEWDATA\nhello\n");

    // A file read while its creator still has it open.
    let (ok, out) = sh("exec 3>new.txt; echo abc >&3; cat new.txt");
    assert!(ok, "{out}");
    assert_eq!(out, "abc\n");

    // fstat and ftruncate on an fd whose file was unlinked.
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(mnt.join("new.txt"))
        .unwrap();
    fs::remove_file(mnt.join("new.txt")).unwrap();
    assert_eq!(f.metadata().unwrap().len(), 4);
    f.set_len(1).unwrap();
    drop(f);

    // A huge truncate of a value is refused, not fatal.
    let (ok, _) = sh("truncate -s 1T 'd.json#/n' 2>/dev/null");
    assert!(!ok);
    assert!(sh("cat d.json").0, "the mount survived");

    // truncate(2) to zero writes the empty value (a string here).
    fs::write(m.backing.join("s.json"), "{\"a\": \"hello\"}\n").unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let (ok, _) = sh("truncate -s 0 's.json#/a'");
    assert!(ok);
    assert_eq!(
        fs::read_to_string(m.backing.join("s.json")).unwrap(),
        "{\"a\": \"\"}\n"
    );

    // Touching a FIFO doesn't hang; touching a symlink with -h leaves the target alone.
    Command::new("mkfifo")
        .arg(m.backing.join("pipe"))
        .status()
        .unwrap();
    std::os::unix::fs::symlink("o1", m.backing.join("sl")).unwrap();
    let before = fs::metadata(m.backing.join("o1")).unwrap().mtime();
    let (ok, _) = sh("timeout 5 touch pipe && touch -h -d 2010-01-01 sl");
    assert!(ok, "touch hung or failed");
    assert_eq!(fs::metadata(m.backing.join("o1")).unwrap().mtime(), before);
    assert_eq!(
        fs::symlink_metadata(m.backing.join("sl")).unwrap().mtime(),
        1_262_304_000
    );

    // An open value handle follows a rename of its file.
    fs::write(m.backing.join("c.json"), "{\"n\": 1}\n").unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let mut h = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(mnt.join("c.json#/n"))
        .unwrap();
    fs::rename(mnt.join("c.json"), mnt.join("c2.json")).unwrap();
    h.write_all(b"5\n").unwrap();
    h.sync_all().unwrap();
    drop(h);
    assert_eq!(
        fs::read_to_string(m.backing.join("c2.json")).unwrap(),
        "{\"n\": 5}\n"
    );

    // chown is refused, not silently ignored.
    let (ok, _) = sh("chown 12345 o1 2>/dev/null");
    assert!(!ok);
}

/// Runs a Python snippet in `dir`; returns its stdout, panicking on failure.
fn py(dir: &Path, code: &str) -> String {
    let out = Command::new("python3")
        .arg("-c")
        .arg(code)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{code}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn edge_case_regressions() {
    let Some(m) = mount(&[
        ("c.json", "{\"server\": {\"port\": 80}, \"empty\": {}}\n"),
        ("arr.json", "{\"tags\": [\"a\", \"b\", \"c\"]}\n"),
        ("n.md", "# A\ntext\n## B\n"),
        ("f.txt", "one\n"),
        ("m.txt", "old\n"),
    ]) else {
        return;
    };
    let (mnt, back) = (&m.mnt, &m.backing);
    let read = |f: &str| fs::read_to_string(back.join(f)).unwrap();

    // rmdir of a # container is like any directory's: only when empty.
    let err = fs::remove_dir(mnt.join("c.json#/server")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(39), "{err}"); // ENOTEMPTY
    let err = fs::remove_dir(mnt.join("n.md#/A")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(39), "{err}");
    fs::remove_dir(mnt.join("n.md#/A/B")).unwrap();
    fs::remove_dir(mnt.join("c.json#/empty")).unwrap();
    assert_eq!(read("c.json"), "{\"server\": {\"port\": 80}}\n");

    // A new key unlinked before it's inserted (an editor's swap file) is
    // never inserted.
    py(
        mnt,
        "import os\n\
         p='c.json#/server/tmp'\n\
         fd=os.open(p, os.O_CREAT|os.O_RDWR|os.O_EXCL, 0o600); os.close(fd); os.unlink(p)\n\
         fd=os.open(p+'2', os.O_CREAT|os.O_RDWR|os.O_EXCL, 0o600); os.unlink(p+'2')\n\
         os.write(fd, b'x'); os.close(fd)",
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(read("c.json"), "{\"server\": {\"port\": 80}}\n");

    // A handle on an array element doesn't write to its neighbour after a
    // sibling is removed: the write is refused.
    py(
        mnt,
        "import os, errno\n\
         fd=os.open('arr.json#/tags/1', os.O_WRONLY|os.O_TRUNC)\n\
         os.unlink('arr.json#/tags/0'); os.write(fd, b'X')\n\
         try:\n    os.close(fd); raise SystemExit('close succeeded')\n\
         except OSError as e:\n    assert e.errno == errno.ESTALE, e",
    );
    assert_eq!(read("arr.json"), "{\"tags\": [\"b\", \"c\"]}\n");

    // Lenses are read-only views: removing or renaming one is EROFS.
    for code in [
        "os.unlink('f.txt@first')",
        "os.rename('f.txt@first', 'x')",
        "os.rmdir('c.json#')",
    ] {
        py(
            mnt,
            &format!(
                "import os, errno\ntry:\n    {code}; raise SystemExit('ok')\n\
                 except OSError as e:\n    assert e.errno == errno.EROFS, e"
            ),
        );
    }

    // fchmod and futimens through an fd act on that fd's file, not on a new
    // file that has taken its name.
    py(
        mnt,
        "import os\n\
         fd=os.open('m.txt', os.O_RDWR); os.unlink('m.txt')\n\
         open('m.txt','w').write('new\\n'); os.chmod('m.txt', 0o644)\n\
         os.fchmod(fd, 0o600); os.utime(fd, (0, 0)); os.close(fd)",
    );
    let meta = fs::metadata(back.join("m.txt")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o644);
    assert_ne!(meta.modified().unwrap(), std::time::UNIX_EPOCH);

    // mmap of a value works.
    let out = py(
        mnt,
        "import mmap\nf=open('c.json#/server/port','rb')\n\
         print(mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)[:].decode(), end='')",
    );
    assert_eq!(out, "80\n");

    // `@now` follows new versions within the present TTL, not a minute later.
    fs::write(mnt.join("f.txt"), "two\n").unwrap();
    m.settle("f.txt", 2);
    assert_eq!(fs::read_to_string(mnt.join("f.txt@now")).unwrap(), "two\n");
    fs::write(mnt.join("f.txt"), "three\n").unwrap();
    m.settle("f.txt", 3);
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(
        fs::read_to_string(mnt.join("f.txt@now")).unwrap(),
        "three\n"
    );
}

#[test]
fn appends_land_at_the_real_end_without_passthrough() {
    let Some(m) = mount_with(&[("log.txt", "line1\n")], &[("FERNET_NO_PASSTHROUGH", "1")]) else {
        return;
    };
    let (mnt, back) = (&m.mnt, &m.backing);
    let _ = fs::metadata(mnt.join("log.txt")).unwrap();
    let _ = fs::read(mnt.join("log.txt")).unwrap();
    // Appended behind the mount: the kernel's cached size is stale now.
    fs::OpenOptions::new()
        .append(true)
        .open(back.join("log.txt"))
        .unwrap()
        .write_all(b"external\n")
        .unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(mnt.join("log.txt"))
        .unwrap()
        .write_all(b"mount\n")
        .unwrap();
    assert_eq!(
        fs::read_to_string(back.join("log.txt")).unwrap(),
        "line1\nexternal\nmount\n"
    );
}
