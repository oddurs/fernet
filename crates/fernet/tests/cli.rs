//! End-to-end tests of the `fernet` binary.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::PredicateStrExt;

fn fernet(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("fernet").unwrap();
    cmd.current_dir(dir).env_remove("FERNET_LOG");
    cmd
}

fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    fs::write(
        p.join("config.json"),
        "{\n  // c\n  \"server\": {\"port\": 8080, \"name\": \"api\\tx\"},\n}\n",
    )
    .unwrap();
    fs::write(p.join("ci.yml"), "jobs:\n  test:\n    steps: [a, b]\n").unwrap();
    fs::create_dir(p.join("src")).unwrap();
    fs::write(p.join("src/Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    fs::write(p.join("odd@-2h#"), "odd").unwrap();
    fs::create_dir(p.join(".fernet")).unwrap();
    tmp
}

#[test]
fn ls_equals_read_dir() {
    let tmp = fixture();
    let out = fernet(tmp.path())
        .arg("ls")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let mut want: Vec<String> = fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != ".fernet")
        .collect();
    want.sort();
    let got: Vec<String> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(got, want);
}

#[test]
fn cat_values_and_files() {
    let tmp = fixture();
    let cases = [
        ("config.json#/server/port", "8080\n"),
        ("config.json#/server/name", "api\tx\n"),
        ("ci.yml#/jobs/test/steps/1", "b\n"),
        ("src/Cargo.toml#/package/name", "x\n"),
        ("odd@-2h#", "odd"),
    ];
    for (path, want) in cases {
        fernet(tmp.path())
            .args(["cat", path])
            .assert()
            .success()
            .stdout(want);
    }
    // From a subdirectory, relative paths and `..` work; the store above sets the root.
    fernet(&tmp.path().join("src"))
        .args(["cat", "Cargo.toml#/package/name"])
        .assert()
        .success()
        .stdout("x\n");
    fernet(&tmp.path().join("src"))
        .args(["cat", "../ci.yml#/jobs/test/steps/0"])
        .assert()
        .success()
        .stdout("a\n");
}

#[test]
fn ls_keys_in_source_order() {
    let tmp = fixture();
    fernet(tmp.path())
        .args(["ls", "config.json#/server"])
        .assert()
        .success()
        .stdout("port\nname\n");
}

#[test]
fn exit_codes_and_messages() {
    let tmp = fixture();
    fernet(tmp.path())
        .args(["cat", "config.json#/server/nope"])
        .assert()
        .code(1)
        .stderr(
            "fernet: no key nope in config.json#/server\n  try: fernet ls config.json#/server\n",
        );
    fernet(tmp.path())
        .args(["cat", "missing.txt"])
        .assert()
        .code(1);
    fernet(tmp.path())
        .args(["cat", "config.json@-2h"])
        .assert()
        .code(5);
    fernet(tmp.path())
        .args(["cat", "config.json#/server"])
        .assert()
        .code(2);
    fernet(tmp.path()).args(["ls", "ci.yml"]).assert().code(2);
    fernet(tmp.path())
        .args(["cat", "/etc/hostname"])
        .assert()
        .code(2);
    fernet(tmp.path()).arg("frobnicate").assert().code(2);
    fs::write(tmp.path().join("bad.toml"), "a = \n").unwrap();
    fernet(tmp.path())
        .args(["cat", "bad.toml#/a"])
        .assert()
        .code(3);
}

#[test]
fn explicit_root() {
    let tmp = fixture();
    fernet(Path::new("/"))
        .args(["--root"])
        .arg(tmp.path())
        .args(["cat"])
        .arg(tmp.path().join("ci.yml#/jobs/test/steps/0"))
        .assert()
        .success()
        .stdout("a\n");
}

fn history_fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    fs::write(
        p.join("infra.yaml"),
        "services:\n  api:\n    image: api:2.3.9\n",
    )
    .unwrap();
    fs::write(p.join("notes.md"), "hello\n").unwrap();
    fernet(p).arg("init").assert().success();
    tmp
}

#[test]
fn init_mark_core_log() {
    let tmp = history_fixture();
    let p = tmp.path();
    fernet(p).arg("init").assert().code(2);
    fernet(p)
        .args(["mark", "before-migration"])
        .assert()
        .success();
    fernet(p)
        .args(["mark", "before-migration"])
        .assert()
        .code(3);
    fernet(p).args(["mark", "Bad_Name"]).assert().code(3);
    fernet(p).args(["mark", "--rm", "nope"]).assert().code(1);
    let out = fernet(p)
        .args(["core", "infra.yaml#/services/api/image"])
        .env("NO_COLOR", "1")
        .assert()
        .success();
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(
        text,
        "──────────── tephra · before-migration ────────────\nnow         api:2.3.9\n"
    );
    let out = fernet(p).arg("log").assert().success();
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        text.starts_with("──────────── tephra · before-migration"),
        "{text}"
    );
    assert_eq!(text.lines().count(), 3, "{text}");
    fernet(p).args(["log", "--size"]).assert().success();
}

#[test]
fn time_lens_reads_and_errors() {
    let tmp = history_fixture();
    let p = tmp.path();
    fernet(p)
        .args(["cat", "notes.md@first"])
        .assert()
        .success()
        .stdout("hello\n");
    fernet(p)
        .args(["cat", "infra.yaml@~0#/services"])
        .assert()
        .code(1);
    fernet(p)
        .args(["cat", "notes.md@1999-01-01"])
        .assert()
        .code(1)
        .stderr(predicate_contains(
            "no version of notes.md before 1999-01-01",
        ));
    fernet(p).args(["cat", "notes.md@nomark"]).assert().code(1);
    let out = fernet(p).args(["ls", "notes.md@"]).assert().success();
    let names = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(names.lines().count(), 1);
    let version = names.trim().to_owned();
    fernet(p)
        .args(["cat", &format!("notes.md@/{version}")])
        .assert()
        .success()
        .stdout("hello\n");
    fernet(p)
        .args(["diff", "notes.md@first", "notes.md"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn verify_and_reindex() {
    let tmp = history_fixture();
    let p = tmp.path();
    fernet(p).arg("verify").assert().success();
    fs::remove_dir_all(p.join(".fernet/index")).unwrap();
    fernet(p)
        .arg("reindex")
        .assert()
        .success()
        .stdout("reindexed 2 records\n");
    fernet(p)
        .args(["cat", "notes.md@first"])
        .assert()
        .success()
        .stdout("hello\n");
    // Damage a blob: verify reports it and exits 5.
    let objects = p.join(".fernet/objects");
    let dir = fs::read_dir(&objects)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let blob = fs::read_dir(dir).unwrap().next().unwrap().unwrap().path();
    fs::write(blob, b"garbage").unwrap();
    fernet(p).arg("verify").assert().code(5);
}

#[test]
fn no_store_errors() {
    let tmp = fixture();
    fs::remove_dir(tmp.path().join(".fernet")).unwrap();
    fernet(tmp.path())
        .args(["cat", "ci.yml@-1h"])
        .assert()
        .code(5)
        .stderr(predicate_contains("try: fernet init"));
    fernet(tmp.path()).arg("log").assert().code(5);
}

fn predicate_contains(s: &'static str) -> impl predicates::Predicate<[u8]> {
    predicates::str::contains(s).from_utf8()
}

#[test]
fn watch_captures_outside_changes() {
    let tmp = history_fixture();
    let p = tmp.path();
    let bin = assert_cmd::cargo::cargo_bin("fernet");
    let mut child = std::process::Command::new(bin)
        .arg("watch")
        .current_dir(p)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // While the watcher holds the lock, writers are refused but readers work.
    std::thread::sleep(std::time::Duration::from_millis(800));
    fernet(p).args(["mark", "x"]).assert().code(5);
    fs::write(
        p.join("infra.yaml"),
        "services:\n  api:\n    image: api:2.4.0\n",
    )
    .unwrap();
    fs::create_dir(p.join("new")).unwrap();
    fs::write(p.join("new/file.txt"), "fresh").unwrap();
    fs::remove_file(p.join("notes.md")).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let out = fernet(p)
            .args(["core", "infra.yaml#/services/api/image"])
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        let log = fernet(p).arg("log").output().unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        if String::from_utf8_lossy(&out.stdout).lines().count() == 2
            && log.contains("new/file.txt")
            && log.contains("deleted  notes.md")
        {
            ok = true;
            break;
        }
    }
    child.kill().unwrap();
    let _ = child.wait();
    assert!(ok, "watcher didn't capture the changes in time");
    fernet(p)
        .args(["cat", "infra.yaml@first#/services/api/image"])
        .assert()
        .success()
        .stdout("api:2.3.9\n");
    fernet(p).arg("verify").assert().success();
}

#[test]
fn completions_and_size_warning() {
    let tmp = history_fixture();
    let p = tmp.path();
    for shell in ["bash", "zsh", "fish"] {
        let out = fernet(p)
            .args(["completions", shell])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert!(
            String::from_utf8(out).unwrap().contains("fernet complete"),
            "{shell}"
        );
    }
    fernet(p).args(["completions", "tcsh"]).assert().code(2);
    fernet(p)
        .args(["complete", "--", "infra.yaml#/services/"])
        .assert()
        .success()
        .stdout("infra.yaml#/services/api/\n");
    fernet(p)
        .args(["complete", "--", "infra.yaml#/services/api/im"])
        .assert()
        .success()
        .stdout("infra.yaml#/services/api/image\n");
    let out = fernet(p)
        .args(["complete", "--", "notes.md@fi"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(String::from_utf8(out).unwrap(), "notes.md@first\n");
    fernet(p)
        .args(["complete", "--", "inf"])
        .assert()
        .success()
        .stdout("infra.yaml\n");
    // A tiny warning limit triggers the warning on `fernet log`.
    let config = p.join(".fernet/config.toml");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("store_warn_bytes = 1073741824", "store_warn_bytes = 10");
    fs::write(&config, text).unwrap();
    fernet(p)
        .arg("log")
        .assert()
        .success()
        .stderr(predicate_contains("warning size"));
}

#[test]
fn write_takes_binary_stdin_for_files() {
    let tmp = fixture();
    let p = tmp.path();
    fernet(p)
        .args(["write", "b.bin"])
        .write_stdin(vec![0xff, 0xfe, 0x00])
        .assert()
        .success();
    assert_eq!(fs::read(p.join("b.bin")).unwrap(), [0xff, 0xfe, 0x00]);
    fernet(p)
        .args(["write", "config.json#/server/name"])
        .write_stdin(vec![0xff])
        .assert()
        .code(3);
}

#[test]
#[cfg(unix)]
fn writes_stay_in_the_root_and_out_of_the_store() {
    use std::os::unix::fs::symlink;
    let outer = tempfile::tempdir().unwrap();
    let p = outer.path().join("root");
    fs::create_dir(&p).unwrap();
    fs::write(p.join("notes.txt"), "v1\n").unwrap();
    fs::create_dir(p.join("sub")).unwrap();
    fernet(&p).arg("init").assert().success();
    fs::create_dir(outer.path().join("outside")).unwrap();
    symlink("../outside", p.join("out")).unwrap();
    symlink(".", p.join("self")).unwrap();
    symlink(".fernet/MANIFEST", p.join("mlink")).unwrap();
    let manifest = fs::read(p.join(".fernet/MANIFEST")).unwrap();

    // New files through a link out of the root, or into the store: refused.
    for path in ["out/new", "mlink"] {
        fernet(&p).args(["write", path, "x"]).assert().code(3);
    }
    // Behind a link to the root, the store is hidden: not found.
    for path in ["self/.fernet/new", "self/.fernet/MANIFEST"] {
        fernet(&p).args(["write", path, "x"]).assert().code(1);
    }
    assert!(!outer.path().join("outside/new").exists());
    assert!(!p.join(".fernet/new").exists());
    assert_eq!(fs::read(p.join(".fernet/MANIFEST")).unwrap(), manifest);
    // The store stays hidden behind a link to the root.
    fernet(&p).args(["ls", "self/.fernet"]).assert().code(1);
    fernet(&p).arg("verify").assert().success();

    // A lens that finds nothing never becomes a file; the past is read-only.
    for path in [
        "notes.txt@~5",
        "sub@~1",
        "sub@first/new.txt",
        "notes.txt@/new",
    ] {
        fernet(&p).args(["write", path, "x"]).assert().code(4);
    }
    fernet(&p)
        .args(["write", "notes.txt#", "x"])
        .assert()
        .code(3);
    let names: Vec<_> = fs::read_dir(&p)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.contains(['@', '#']))
        .collect();
    assert!(names.is_empty(), "{names:?}");
    // Names that only look like lenses are still plain files.
    fernet(&p)
        .args(["write", "user@example.com", "x"])
        .assert()
        .success();
    assert!(p.join("user@example.com").exists());
}

#[test]
#[cfg(unix)]
fn paths_through_a_linked_root() {
    let outer = tempfile::tempdir().unwrap();
    let p = outer.path().join("root");
    fs::create_dir(&p).unwrap();
    fs::write(p.join("notes.txt"), "hi\n").unwrap();
    fernet(&p).arg("init").assert().success();
    std::os::unix::fs::symlink("root", outer.path().join("link")).unwrap();
    let abs = outer.path().join("link/notes.txt");
    fernet(&p)
        .arg("cat")
        .arg(&abs)
        .assert()
        .success()
        .stdout("hi\n");
    fernet(outer.path())
        .args(["--root", "link", "cat", "link/notes.txt"])
        .assert()
        .success()
        .stdout("hi\n");
}

#[test]
fn a_stamp_selects_its_listing_entry() {
    let tmp = history_fixture();
    let p = tmp.path();
    for v in ["v2", "v3", "v4"] {
        fernet(p).args(["write", "notes.md", v]).assert().success();
    }
    let out = fernet(p).args(["ls", "notes.md@"]).output().unwrap();
    let listing = String::from_utf8(out.stdout).unwrap();
    for name in listing.lines() {
        let by_listing = fernet(p)
            .arg("cat")
            .arg(format!("notes.md@/{name}"))
            .output()
            .unwrap()
            .stdout;
        if name.contains('.') {
            continue; // `.N` names are listing-only
        }
        let by_stamp = fernet(p)
            .arg("cat")
            .arg(format!("notes.md@{name}"))
            .output()
            .unwrap()
            .stdout;
        assert_eq!(by_stamp, by_listing, "{name}");
    }
    fernet(p)
        .args(["cat", "notes.md@9999-12-31"])
        .assert()
        .success()
        .stdout("v4");
}

#[test]
fn completion_lets_real_names_and_keys_win() {
    let tmp = history_fixture();
    let p = tmp.path();
    fs::write(p.join("user@example.com"), "x").unwrap();
    fs::write(p.join("e.json"), r#"{"emails":{"a@b.com":1,"x#":2}}"#).unwrap();
    fs::create_dir(p.join("sub")).unwrap();
    let complete = |word: &str| {
        let out = fernet(p).args(["complete", "--", word]).output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(complete("user@ex"), "user@example.com\n");
    assert_eq!(complete("e.json#/emails/a@"), "e.json#/emails/a@b.com\n");
    assert_eq!(complete("e.json#/emails/x#"), "e.json#/emails/x#\n");
    let dir = complete("sub@");
    assert!(dir.contains("sub@first") && !dir.contains("sub@/") && !dir.contains("sub@~1"));
    let file = complete("notes.md@");
    assert!(file.contains("notes.md@/") && file.contains("notes.md@~1"));
}

/// A nested `.fernet` directory that isn't a store is user data: root
/// discovery keeps walking up to the real store.
#[test]
fn a_nested_fernet_directory_does_not_stop_root_discovery() {
    let tmp = history_fixture();
    let p = tmp.path();
    fs::create_dir_all(p.join("sub/.fernet")).unwrap();
    fs::create_dir_all(p.join("sub/deep")).unwrap();
    fernet(&p.join("sub/deep"))
        .args(["cat", "../../notes.md@first"])
        .assert()
        .success()
        .stdout("hello\n");
}

/// A `config.toml` that doesn't parse is reported, and compaction refuses
/// rather than applying default retention.
#[test]
fn a_broken_config_is_reported() {
    let tmp = history_fixture();
    let p = tmp.path();
    fs::write(
        p.join(".fernet/config.toml"),
        "[retention\nsnow_hours = 1\n",
    )
    .unwrap();
    fernet(p)
        .arg("log")
        .assert()
        .success()
        .stderr(predicate_contains("config.toml is not valid toml"));
    fernet(p)
        .arg("compact")
        .assert()
        .code(5)
        .stderr(predicate_contains("config.toml is not valid toml"));
    // Completion stays quiet.
    fernet(p)
        .args(["complete", "no"])
        .assert()
        .success()
        .stderr("");
}

/// An unreadable file doesn't abort init: it's skipped with a warning.
#[test]
fn init_skips_unreadable_files() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    fs::write(p.join("a.txt"), "a").unwrap();
    fs::write(p.join("secret.txt"), "s").unwrap();
    fs::set_permissions(p.join("secret.txt"), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(p.join("secret.txt")).is_ok() {
        return; // Running as root: permissions don't apply.
    }
    fernet(p)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate_contains("(1 file captured)"))
        .stderr(predicate_contains("skipped secret.txt"));
    fernet(p).args(["cat", "a.txt@first"]).assert().success();
}

/// The watcher never follows symlinks to directories and applies ignore
/// rules as the initial walk does.
#[test]
fn watch_ignores_symlinked_directories_and_reincluded_files() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    let outside = tempfile::tempdir().unwrap();
    fs::write(p.join(".fernetignore"), "logs/\n!logs/keep.txt\n").unwrap();
    fs::create_dir(p.join("logs")).unwrap();
    std::os::unix::fs::symlink(outside.path(), p.join("out")).unwrap();
    std::os::unix::fs::symlink(".", p.join("self")).unwrap();
    fernet(p).arg("init").assert().success();
    let bin = assert_cmd::cargo::cargo_bin("fernet");
    let mut child = std::process::Command::new(bin)
        .arg("watch")
        .current_dir(p)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    fs::write(outside.path().join("secret.txt"), "s").unwrap();
    fs::write(p.join("logs/keep.txt"), "k").unwrap();
    fs::write(p.join("a.txt"), "a").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut log = String::new();
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let out = fernet(p).arg("log").output().unwrap();
        log = String::from_utf8_lossy(&out.stdout).into_owned();
        if log.contains("a.txt") {
            break;
        }
    }
    // Give stragglers from the same batch a moment.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let out = fernet(p).arg("log").output().unwrap();
    child.kill().unwrap();
    let _ = child.wait();
    let final_log = String::from_utf8_lossy(&out.stdout);
    assert!(log.contains("a.txt"), "watcher didn't capture a.txt: {log}");
    for bad in ["secret.txt", "keep.txt", "self/", "out/", "MANIFEST"] {
        assert!(!final_log.contains(bad), "{bad} captured:\n{final_log}");
    }
}
