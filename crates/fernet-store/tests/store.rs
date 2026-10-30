//! Store acceptance tests for milestone 2: crash safety, corruption
//! detection, reindexing (invariant 5) and deduplication.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation
)]

use std::fs;
use std::path::Path;

use fernet_fs_store::{Hash, Kind, Mode, Record, Store, segment, verify};
use proptest::prelude::*;

fn write(store: &mut Store, path: &str, content: &[u8], time: i64) {
    let hash = store.put_blob(content).unwrap();
    store
        .append(&Record {
            kind: Kind::Write,
            time,
            path: path.into(),
            hash,
            size: content.len() as u64,
            extra: String::new(),
        })
        .unwrap();
}

fn seg_path(root: &Path, id: u64) -> std::path::PathBuf {
    root.join(".fernet/log").join(segment::file_name(id))
}

/// A store with `n` writes, closed.
fn store_with(n: usize) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::init(tmp.path()).unwrap();
    for i in 0..n {
        write(
            &mut s,
            &format!("f{}.txt", i % 3),
            format!("content {i}").as_bytes(),
            i as i64 * 1000,
        );
    }
    s.flush().unwrap();
    drop(s);
    tmp
}

#[test]
fn crash_at_every_byte_of_the_last_record() {
    let n = 5;
    let tmp = store_with(n);
    let seg = seg_path(tmp.path(), 1);
    let full = fs::read(&seg).unwrap();
    let records = Store::open(tmp.path(), Mode::Read)
        .unwrap()
        .records()
        .unwrap();
    assert_eq!(records.len(), n);
    let last_start = records[n - 1].0.offset as usize;
    for cut in last_start..full.len() {
        let work = tempfile::tempdir().unwrap();
        copy_dir(&tmp.path().join(".fernet"), &work.path().join(".fernet"));
        fs::write(seg_path(work.path(), 1), &full[..cut]).unwrap();
        let s = Store::open(work.path(), Mode::Write).unwrap_or_else(|e| panic!("cut {cut}: {e}"));
        let got = s.records().unwrap();
        assert_eq!(got.len(), n - 1, "cut {cut}");
        assert_eq!(got[..], records[..n - 1], "cut {cut}");
        assert_eq!(
            s.versions("f1.txt").unwrap().len(),
            records[..n - 1]
                .iter()
                .filter(|(_, r)| r.path == "f1.txt")
                .count()
        );
        drop(s);
        let report = verify(work.path()).unwrap();
        assert!(report.ok(), "cut {cut}: {:?}", report.problems);
        assert_eq!(report.records, (n - 1) as u64);
    }
}

#[test]
fn every_byte_flip_in_a_sealed_segment_is_reported() {
    let tmp = store_with(4);
    // Seal segment 1 by rolling to segment 2 by hand.
    {
        let mut s = Store::open(tmp.path(), Mode::Write).unwrap();
        s.flush().unwrap();
    }
    fs::write(seg_path(tmp.path(), 2), segment::header(2)).unwrap();
    fs::write(
        tmp.path().join(".fernet/MANIFEST"),
        "fernet-manifest 1\n1\n2\n",
    )
    .unwrap();
    assert!(verify(tmp.path()).unwrap().ok());
    let seg = seg_path(tmp.path(), 1);
    let full = fs::read(&seg).unwrap();
    for i in 0..full.len() {
        let mut b = full.clone();
        b[i] ^= 0x5a;
        fs::write(&seg, &b).unwrap();
        let report = verify(tmp.path()).unwrap();
        assert!(!report.ok(), "flip at byte {i} went unreported");
    }
    fs::write(&seg, &full).unwrap();
    assert!(verify(tmp.path()).unwrap().ok());
}

#[test]
fn corruption_before_the_tail_refuses_to_open() {
    let tmp = store_with(3);
    let seg = seg_path(tmp.path(), 1);
    let mut b = fs::read(&seg).unwrap();
    b[segment::HEADER_LEN + 12] ^= 1;
    fs::write(&seg, &b).unwrap();
    assert!(Store::open(tmp.path(), Mode::Write).is_err());
    assert!(!verify(tmp.path()).unwrap().ok());
}

#[test]
fn identical_saves_store_one_blob() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = Store::init(tmp.path()).unwrap();
    for i in 0..40 {
        write(&mut s, "same.txt", b"identical bytes", i);
    }
    assert_eq!(s.usage().unwrap().blobs, 1);
}

#[test]
fn segments_roll_and_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    drop(Store::init(tmp.path()).unwrap());
    let config = tmp.path().join(".fernet/config.toml");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("segment_bytes = 67108864", "segment_bytes = 1024");
    fs::write(&config, text).unwrap();
    let mut s = Store::open(tmp.path(), Mode::Write).unwrap();
    let blob = s.put_blob(b"x").unwrap();
    for i in 0..200 {
        s.append(&Record {
            kind: Kind::Write,
            time: i,
            path: format!("dir/file-{i}"),
            hash: blob,
            size: 1,
            extra: String::new(),
        })
        .unwrap();
    }
    assert!(s.segments().len() >= 10, "{:?}", s.segments());
    drop(s);
    let s = Store::open(tmp.path(), Mode::Write).unwrap();
    assert_eq!(s.records().unwrap().len(), 200);
    assert_eq!(s.versions("dir/file-150").unwrap().len(), 1);
    assert!(verify(tmp.path()).unwrap().ok());
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            fs::copy(e.path(), dest).unwrap();
        }
    }
}

/// A query result fingerprint over many paths, times and marks.
fn answers(s: &Store, paths: &[String], times: &[i64]) -> Vec<String> {
    let mut out = Vec::new();
    for p in paths {
        out.push(format!("{p} {:?}", s.versions(p).unwrap()));
        for &t in times {
            out.push(format!("{p}@{t} {:?}", s.version_at(p, t).unwrap()));
        }
    }
    out.push(format!("{:?}", s.paths("").unwrap()));
    out.push(format!("{:?}", s.paths("d/").unwrap()));
    out.push(format!("{:?}", s.marks().unwrap()));
    out
}

#[derive(Debug, Clone)]
enum Op {
    Write(u8, u8),
    Delete(u8),
    Rename(u8, u8),
    Mark(u8),
    Unmark(u8),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u8..6, any::<u8>()).prop_map(|(p, c)| Op::Write(p, c)),
        1 => (0u8..6).prop_map(Op::Delete),
        1 => (0u8..6, 0u8..6).prop_map(|(a, b)| Op::Rename(a, b)),
        1 => (0u8..3).prop_map(Op::Mark),
        1 => (0u8..3).prop_map(Op::Unmark),
    ]
}

fn name(p: u8) -> String {
    if p.is_multiple_of(2) {
        format!("d/f{p}")
    } else {
        format!("f{p}")
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Invariant 5: deleting the index and reindexing gives identical answers,
    /// and so does the in-memory index a read-only open builds.
    #[test]
    fn reindex_gives_identical_answers(ops in proptest::collection::vec(op(), 1..60), times in proptest::collection::vec(-5i64..400, 1..12)) {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Store::init(tmp.path()).unwrap();
        for (i, op) in ops.iter().enumerate() {
            let t = (i as i64) * 5;
            let r = match op {
                Op::Write(p, c) => {
                    let hash = s.put_blob(&[*c]).unwrap();
                    Record { kind: Kind::Write, time: t, path: name(*p), hash, size: 1, extra: String::new() }
                }
                Op::Delete(p) => Record { kind: Kind::Delete, time: t, path: name(*p), hash: Hash::ZERO, size: 0, extra: String::new() },
                Op::Rename(a, b) => {
                    let hash = s.put_blob(b"r").unwrap();
                    Record { kind: Kind::Rename, time: t, path: name(*b), hash, size: 1, extra: name(*a) }
                }
                Op::Mark(m) => Record { kind: Kind::Mark, time: t, path: String::new(), hash: Hash::ZERO, size: 0, extra: format!("m{m}") },
                Op::Unmark(m) => Record { kind: Kind::Unmark, time: t, path: String::new(), hash: Hash::ZERO, size: 0, extra: format!("m{m}") },
            };
            s.append(&r).unwrap();
        }
        let paths: Vec<String> = (0..6).map(name).collect();
        let before = answers(&s, &paths, &times);

        // Deleting the index and reopening rebuilds it.
        drop(s);
        fs::remove_dir_all(tmp.path().join(".fernet/index")).unwrap();
        let mut s = Store::open(tmp.path(), Mode::Write).unwrap();
        prop_assert_eq!(&answers(&s, &paths, &times), &before);

        // An explicit reindex.
        s.reindex().unwrap();
        prop_assert_eq!(&answers(&s, &paths, &times), &before);

        // A read-only open while the writer holds the lock uses memory.
        let reader = Store::open(tmp.path(), Mode::Read).unwrap();
        prop_assert!(!reader.writable());
        prop_assert_eq!(&answers(&reader, &paths, &times), &before);
    }
}

#[test]
fn writer_lock_is_exclusive() {
    let tmp = store_with(1);
    let _w = Store::open(tmp.path(), Mode::Write).unwrap();
    assert!(matches!(
        Store::open(tmp.path(), Mode::Write),
        Err(fernet_fs_store::Error::Busy)
    ));
    assert!(Store::open(tmp.path(), Mode::Read).is_ok());
}
