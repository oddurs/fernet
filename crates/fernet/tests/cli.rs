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
