//! Milestone 3: the read-only mount. Linux only; needs `/dev/fuse` and
//! `fusermount3`. Skipped when FUSE is unavailable unless
//! `FERNET_REQUIRE_FUSE=1` (set in CI), which turns a skip into a failure.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fernet_fs_core::{Fernet, Options};

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

struct Mounted {
    tmp: tempfile::TempDir,
    backing: PathBuf,
    mnt: PathBuf,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("fusermount3")
            .arg("-u")
            .arg(&self.mnt)
            .status();
    }
}

fn fernet_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("fernet")
}

/// A backing directory with history:
/// - `infra.yaml`: two versions (image api:2.3.9 → api:2.4.0), a mark between
/// - `config.json`: two versions
/// - `notes.md`: one version, then deleted
/// - `src/lib.rs`, `src/deep/mod.rs`: one version each
/// - `odd@-2h#`: a real file with lens characters in its name
fn fixture() -> Option<Mounted> {
    if !fuse_available() {
        eprintln!("skipping: FUSE unavailable");
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    let backing = tmp.path().join("backing");
    let mnt = tmp.path().join("mnt");
    fs::create_dir_all(backing.join("src/deep")).unwrap();
    fs::create_dir_all(&mnt).unwrap();
    let w = |p: &str, c: &str| fs::write(backing.join(p), c).unwrap();
    w(
        "infra.yaml",
        "services:\n  api:\n    image: api:2.3.9\n    port: 8080\n",
    );
    w(
        "config.json",
        "{\"server\": {\"port\": 8080, \"name\": \"api\"}, \"tags\": [\"a\", \"b\"]}\n",
    );
    w("notes.md", "# notes\nhello\n");
    w("src/lib.rs", "pub fn api() {}\n");
    w("src/deep/mod.rs", "// deep api\n");
    w("odd@-2h#", "odd");
    {
        let f = Fernet::init(&backing, Options::default()).unwrap();
        f.snapshot_tree(&backing).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        f.mark("before-migration").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        w(
            "infra.yaml",
            "services:\n  api:\n    image: api:2.4.0\n    port: 8080\n",
        );
        w(
            "config.json",
            "{\"server\": {\"port\": 9090, \"name\": \"api\"}, \"tags\": [\"a\", \"b\", \"c\"]}\n",
        );
        fs::remove_file(backing.join("notes.md")).unwrap();
        f.snapshot_tree(&backing).unwrap();
        f.flush().unwrap();
    }
    let st = Command::new(fernet_bin())
        .arg("mount")
        .arg(&backing)
        .arg(&mnt)
        .arg("--daemon")
        .status()
        .unwrap();
    assert!(st.success(), "mount failed");
    Some(Mounted { tmp, backing, mnt })
}

fn sh(cmd: &str, cwd: &Path) -> (i32, String) {
    let out = Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn cli(backing: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new(fernet_bin())
        .args(args)
        .current_dir(backing)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fernet {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// The version-listing name of the first version of `file`.
fn first_version(backing: &Path, file: &str) -> String {
    String::from_utf8(cli(backing, &["ls", &format!("{file}@")]))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned()
}

#[test]
fn shell_suite() {
    let Some(m) = fixture() else { return };
    let mnt = &m.mnt;

    // cat: real, inside, past, past + inside, version listing entry.
    assert_eq!(
        sh("cat infra.yaml", mnt).1,
        fs::read_to_string(m.backing.join("infra.yaml")).unwrap()
    );
    assert_eq!(
        sh("cat 'infra.yaml#/services/api/image'", mnt).1,
        "api:2.4.0\n"
    );
    assert_eq!(
        sh("cat 'infra.yaml@first#/services/api/image'", mnt).1,
        "api:2.3.9\n"
    );
    assert_eq!(
        sh("cat 'infra.yaml@before-migration'", mnt).1,
        "services:\n  api:\n    image: api:2.3.9\n    port: 8080\n"
    );
    assert_eq!(sh("cat 'config.json@~1#/server/port'", mnt).1, "8080\n");
    assert_eq!(sh("cat 'notes.md@first'", mnt).1, "# notes\nhello\n");
    assert_eq!(sh("cat 'odd@-2h#'", mnt).1, "odd");
    let v = first_version(&m.backing, "infra.yaml");
    assert_eq!(
        sh(&format!("cat 'infra.yaml@/{v}#/services/api/image'"), mnt).1,
        "api:2.3.9\n"
    );

    // ls: real dirs equal the backing dir minus .fernet; lenses only when named.
    let (_, real) = sh("ls -A", &m.backing);
    let real: Vec<&str> = real.lines().filter(|l| *l != ".fernet").collect();
    assert_eq!(sh("ls -A", mnt).1.lines().collect::<Vec<_>>(), real);
    assert_eq!(sh("ls 'config.json#'", mnt).1, "server\ntags\n");
    assert_eq!(sh("ls 'config.json#/tags'", mnt).1, "0\n1\n2\n");
    assert_eq!(sh("ls 'src@first'", mnt).1, "deep\nlib.rs\n");
    assert_eq!(sh("ls 'infra.yaml@'", mnt).1.lines().count(), 3);
    assert_eq!(sh("ls notes.md", mnt).0, 2);

    // diff between a past version and the present.
    let (code, out) = sh("diff 'infra.yaml@first' infra.yaml", mnt);
    assert_eq!(code, 1);
    assert!(
        out.contains("<     image: api:2.3.9") && out.contains(">     image: api:2.4.0"),
        "{out}"
    );
    assert_eq!(sh("diff 'src@first/lib.rs' src/lib.rs", mnt).0, 0);

    // cp out of the past and out of a file.
    let out_dir = m.tmp.path();
    sh(
        &format!(
            "cp 'config.json@first' {0}/old.json && cp 'config.json#/server/port' {0}/port",
            out_dir.display()
        ),
        mnt,
    );
    assert!(
        fs::read_to_string(out_dir.join("old.json"))
            .unwrap()
            .contains("8080")
    );
    assert_eq!(fs::read_to_string(out_dir.join("port")).unwrap(), "9090\n");

    // grep -r sees only real files.
    let (_, hits) = sh("grep -rl api . | sort", mnt);
    assert_eq!(
        hits,
        "./config.json\n./infra.yaml\n./src/deep/mod.rs\n./src/lib.rs\n"
    );

    // stat: real files keep their mode and size; past and inside are read-only.
    let (_, s) = sh(
        "stat -c '%A %s' infra.yaml 'infra.yaml@first' 'infra.yaml#/services/api/port' 'src@first'",
        mnt,
    );
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "-rw-r--r-- {}",
            fs::metadata(m.backing.join("infra.yaml")).unwrap().len()
        )
    );
    assert_eq!(lines[1], "-r--r--r-- 53");
    assert_eq!(lines[2], "-r--r--r-- 5");
    assert!(lines[3].starts_with("dr-xr-xr-x"));

    // Writes are refused: the mount is read-only in milestone 3.
    let (code, _) = sh("echo x > new.txt", mnt);
    assert_ne!(code, 0);
    assert!(!m.backing.join("new.txt").exists());
    // .fernet is hidden.
    assert_ne!(sh("ls .fernet", mnt).0, 0);
}

#[test]
fn find_and_rg_never_descend_into_lenses() {
    let Some(m) = fixture() else { return };
    let (_, backing) = sh("find . -path ./.fernet -prune -o -print | sort", &m.backing);
    let (_, mounted) = sh("find . | sort", &m.mnt);
    assert_eq!(mounted, backing);
    assert!(!mounted.contains('#') || mounted.contains("odd@-2h#"));
    if Command::new("rg").arg("--version").output().is_ok() {
        let (_, files) = sh("rg --files --hidden | sort", &m.mnt);
        let (_, want) = sh("find . -type f | sed 's|^./||' | sort", &m.mnt);
        assert_eq!(files, want);
    }
}

#[test]
fn mount_matches_cli_byte_for_byte() {
    let Some(m) = fixture() else { return };
    let v = first_version(&m.backing, "infra.yaml");
    let files = [
        "infra.yaml".to_owned(),
        "config.json".into(),
        "src/lib.rs".into(),
        "odd@-2h#".into(),
        "infra.yaml#/services/api/image".into(),
        "infra.yaml#/services/api/port".into(),
        "config.json#/server/name".into(),
        "config.json#/tags/2".into(),
        "infra.yaml@first".into(),
        "infra.yaml@~1".into(),
        "infra.yaml@before-migration".into(),
        "infra.yaml@before-migration#/services/api/image".into(),
        format!("infra.yaml@/{v}"),
        "config.json@first#/tags/1".into(),
        "notes.md@first".into(),
        "src@first/deep/mod.rs".into(),
        "src@first/lib.rs".into(),
    ];
    for p in &files {
        let mounted = fs::read(m.mnt.join(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
        assert_eq!(mounted, cli(&m.backing, &["cat", p]), "cat {p}");
    }
    let dirs = [
        ".",
        "src",
        "src/deep",
        "config.json#",
        "config.json#/server",
        "config.json#/tags",
        "infra.yaml@",
        "src@first",
        "src@first/deep",
        "config.json@first#",
        "infra.yaml@first#/services/api",
    ];
    for p in dirs {
        let mounted: Vec<String> = fs::read_dir(m.mnt.join(p))
            .unwrap_or_else(|e| panic!("{p}: {e}"))
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let listed: Vec<String> = String::from_utf8(cli(&m.backing, &["ls", p]))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(mounted, listed, "ls {p}");
    }
}
