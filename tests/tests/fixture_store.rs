//! The committed v1 fixture store (`tests/fixtures/store-v1`): every later
//! version must read it (brief 6, compatibility promise).
//!
//! Regenerate it (only when deliberately changing the fixture, never the
//! format) with: `FERNET_WRITE_FIXTURE=1 cargo test -p fernet-tests --test fixture_store`.

#![allow(clippy::unwrap_used, clippy::format_push_string)]

use std::path::{Path, PathBuf};

use fernet_fs_store::{Hash, Kind, Mode, Record, Store};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/store-v1")
}

/// The fixture's history: (kind, time, path, content or extra).
#[must_use]
pub fn history() -> Vec<(Kind, i64, &'static str, &'static str)> {
    const S: i64 = 1_000_000_000;
    let t0 = 1_790_000_000 * S; // 2026-09-21
    vec![
        (Kind::Write, t0, "notes.md", "# notes\nfirst\n"),
        (Kind::Write, t0, "config.json", "{\"port\": 8080}\n"),
        (Kind::Write, t0 + 60 * S, "notes.md", "# notes\nsecond\n"),
        (Kind::Mark, t0 + 90 * S, "", "before-migration"),
        (
            Kind::Write,
            t0 + 120 * S,
            "config.json",
            "{\"port\": 9090}\n",
        ),
        (Kind::Write, t0 + 150 * S, "src/lib.rs", "pub fn api() {}\n"),
        (Kind::Rename, t0 + 180 * S, "src/api.rs", "src/lib.rs"),
        (Kind::Delete, t0 + 240 * S, "notes.md", ""),
        (Kind::Mark, t0 + 300 * S, "", "temp"),
        (Kind::Unmark, t0 + 301 * S, "", "temp"),
        (Kind::Write, t0 + 360 * S, "notes.md", "# notes\nback\n"),
        // Identical bytes again: a new record, the same blob.
        (
            Kind::Write,
            t0 + 420 * S,
            "config.json",
            "{\"port\": 8080}\n",
        ),
    ]
}

fn write_fixture(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let mut s = Store::init(dir).unwrap();
    for (kind, time, path, text) in history() {
        let r = match kind {
            Kind::Write => {
                let hash = s.put_blob(text.as_bytes()).unwrap();
                Record {
                    kind,
                    time,
                    path: path.into(),
                    hash,
                    size: text.len() as u64,
                    extra: String::new(),
                }
            }
            Kind::Rename => {
                let content = b"pub fn api() {}\n";
                let hash = s.put_blob(content).unwrap();
                Record {
                    kind,
                    time,
                    path: path.into(),
                    hash,
                    size: content.len() as u64,
                    extra: text.into(),
                }
            }
            Kind::Delete => Record {
                kind,
                time,
                path: path.into(),
                hash: Hash::ZERO,
                size: 0,
                extra: String::new(),
            },
            Kind::Mark | Kind::Unmark => Record {
                kind,
                time,
                path: String::new(),
                hash: Hash::ZERO,
                size: 0,
                extra: text.into(),
            },
        };
        s.append(&r).unwrap();
    }
    s.flush().unwrap();
    drop(s);
    // A plain-text account of what the store holds, for readers written from
    // FORMAT.md alone (tools/format-reader): kind, time, path, extra, blake3.
    let mut expected = String::new();
    for (kind, time, path, text) in history() {
        let (extra, content): (&str, &[u8]) = match kind {
            Kind::Write => ("", text.as_bytes()),
            Kind::Rename => (text, b"pub fn api() {}\n"),
            Kind::Delete => ("", b""),
            Kind::Mark | Kind::Unmark => (text, b""),
        };
        let hash = if matches!(kind, Kind::Write | Kind::Rename) {
            Hash::of(content).to_hex()
        } else {
            "-".into()
        };
        expected.push_str(&format!(
            "{}\t{time}\t{path}\t{extra}\t{hash}\n",
            kind as u8
        ));
    }
    std::fs::write(dir.join("expected.txt"), expected).unwrap();
    // The index and lock are not part of the format.
    let store = dir.join(".fernet");
    let _ = std::fs::remove_dir_all(store.join("index"));
    let _ = std::fs::remove_file(store.join("lock"));
}

#[test]
fn every_version_reads_the_v1_fixture() {
    if std::env::var_os("FERNET_WRITE_FIXTURE").is_some() {
        write_fixture(&fixture_dir());
    }
    // Work on a copy: opening creates an index and a lock file.
    let tmp = tempfile::tempdir().unwrap();
    copy(&fixture_dir(), tmp.path());
    let s = Store::open(tmp.path(), Mode::Read).unwrap();
    let records = s.records().unwrap();
    let h = history();
    assert_eq!(records.len(), h.len());
    for ((_, r), (kind, time, path, text)) in records.iter().zip(&h) {
        assert_eq!((r.kind, r.time), (*kind, *time));
        match kind {
            Kind::Write => {
                assert_eq!(r.path, *path);
                let v = s.version_at(path, *time).unwrap().unwrap();
                assert_eq!(s.read(&v).unwrap(), text.as_bytes());
            }
            Kind::Rename => assert_eq!((r.path.as_str(), r.extra.as_str()), (*path, *text)),
            Kind::Mark | Kind::Unmark => assert_eq!(r.extra, *text),
            Kind::Delete => assert_eq!(r.path, *path),
        }
    }
    assert!(fernet_fs_store::verify(tmp.path()).unwrap().ok());
    assert_eq!(
        s.marks().unwrap().keys().collect::<Vec<_>>(),
        ["before-migration"]
    );
}

fn copy(from: &Path, to: &Path) {
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&dest).unwrap();
            copy(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}
