//! End-to-end tests of the `fernet` binary.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;

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
