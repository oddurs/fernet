//! "Real names win": a real entry is served as-is even when its name contains
//! `@` or `#`. Lenses exist only where no real entry does.

#![allow(clippy::unwrap_used)]

use std::fs;

use fernet_fs_core::{Error, Fernet, Resolved};
use fernet_fs_path::FernetPath;

fn setup() -> (tempfile::TempDir, Fernet) {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path();
    fs::write(
        p.join("config.json"),
        r#"{"server": {"port": 8080}, "a/b": "slash", "": "empty"}"#,
    )
    .unwrap();
    fs::write(p.join("config.json#"), "i am a real file").unwrap();
    fs::write(p.join("a@-2h"), "real a").unwrap();
    fs::write(p.join("user@example.com"), "mail").unwrap();
    fs::create_dir(p.join("weird#")).unwrap();
    fs::write(p.join("weird#/inner"), "inner").unwrap();
    fs::write(p.join("data.yaml"), "list: [x, y]\n").unwrap();
    let f = Fernet::open(p).unwrap();
    (tmp, f)
}

fn read(f: &Fernet, path: &str) -> Result<Vec<u8>, Error> {
    let r = f.resolve(&FernetPath::parse(path).unwrap())?;
    f.read(&r)
}

#[test]
fn real_entries_beat_lenses() {
    let (_tmp, f) = setup();
    assert_eq!(read(&f, "config.json#").unwrap(), b"i am a real file");
    assert_eq!(read(&f, "a@-2h").unwrap(), b"real a");
    assert_eq!(read(&f, "user@example.com").unwrap(), b"mail");
    assert_eq!(read(&f, "weird#/inner").unwrap(), b"inner");
}

#[test]
fn lenses_where_no_real_entry() {
    let (_tmp, f) = setup();
    assert_eq!(read(&f, "data.yaml#/list/1").unwrap(), b"y\n");
    assert_eq!(
        read(&f, "config.json#/server/port")
            .unwrap_err()
            .to_string(),
        "config.json# is not a directory"
    );
    assert!(matches!(read(&f, "b@-2h"), Err(Error::NoHistory { .. })));
    assert!(matches!(read(&f, "nope#"), Err(Error::NotFound { .. })));
    assert!(matches!(read(&f, "x@-02h"), Err(Error::NotFound { .. })));
}

#[test]
fn escapes_and_extension_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("c.json"),
        r#"{"a/b": 1, "m~n": 2, "": 3, ".": 4, "..": 5, "x": {"0": "zero"}}"#,
    )
    .unwrap();
    let f = Fernet::open(tmp.path()).unwrap();
    for (path, want) in [
        ("c.json#/a~1b", "1"),
        ("c.json#/m~0n", "2"),
        ("c.json#/~e", "3"),
        ("c.json#/~.", "4"),
        ("c.json#/~..", "5"),
        ("c.json#/x/0", "zero"),
    ] {
        assert_eq!(
            read(&f, path).unwrap(),
            format!("{want}\n").as_bytes(),
            "{path}"
        );
    }
    let r = f.resolve(&FernetPath::parse("c.json#").unwrap()).unwrap();
    let names: Vec<_> = f
        .list(&r)
        .unwrap()
        .into_iter()
        .map(|e| e.name.into_string().unwrap())
        .collect();
    assert_eq!(names, ["a~1b", "m~0n", "~e", "~.", "~..", "x"]);
}

#[test]
fn array_indices_are_canonical() {
    let (_tmp, f) = setup();
    assert!(read(&f, "data.yaml#/list/01").is_err());
    assert!(read(&f, "data.yaml#/list/2").is_err());
    assert!(read(&f, "data.yaml#/list/-1").is_err());
}

#[test]
fn unknown_formats_have_no_inside() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("notes.txt"), "hello").unwrap();
    fs::write(tmp.path().join("blob.bin"), [0u8, 1, 2]).unwrap();
    let f = Fernet::open(tmp.path()).unwrap();
    assert!(matches!(
        read(&f, "notes.txt#"),
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(read(&f, "blob.bin#"), Err(Error::NotFound { .. })));
}

#[test]
fn parse_errors_are_reported() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("bad.json"), "{\n  \"a\" 1}").unwrap();
    let f = Fernet::open(tmp.path()).unwrap();
    let e = read(&f, "bad.json#/a").unwrap_err();
    assert_eq!(
        e.to_string(),
        "bad.json is not valid json: expected ':' at 2:7"
    );
}

#[test]
fn files_are_not_directories() {
    let (_tmp, f) = setup();
    let r = f.resolve(&FernetPath::parse("data.yaml").unwrap()).unwrap();
    assert!(matches!(r, Resolved::RealFile { .. }));
    assert!(matches!(f.list(&r), Err(Error::NotADirectory { .. })));
    assert!(matches!(
        read(&f, "data.yaml/x"),
        Err(Error::NotADirectory { .. })
    ));
}
