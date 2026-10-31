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

mod compaction {
    use super::*;
    use fernet_fs_store::compact::{CrashAfter, Policy, plan};

    const HOUR: i64 = 3_600 * 1_000_000_000;
    const DAY: i64 = 24 * HOUR;
    const NOW: i64 = 500 * DAY;

    /// A store with a long, dense history across every retention layer, two
    /// files, a live mark and a removed one. Segments are small so compaction
    /// writes several.
    fn aged_store() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        drop(Store::init(tmp.path()).unwrap());
        let config = tmp.path().join(".fernet/config.toml");
        let text = fs::read_to_string(&config)
            .unwrap()
            .replace("segment_bytes = 67108864", "segment_bytes = 2048");
        fs::write(&config, text).unwrap();
        let mut s = Store::open(tmp.path(), Mode::Write).unwrap();
        let mut t = NOW - 450 * DAY;
        let mut i = 0u32;
        while t < NOW {
            let path = if i.is_multiple_of(3) {
                "b.txt"
            } else {
                "a.txt"
            };
            write(&mut s, path, format!("v{i}").as_bytes(), t);
            if i == 40 {
                s.append(&Record {
                    kind: Kind::Mark,
                    time: t + 1,
                    path: String::new(),
                    hash: Hash::ZERO,
                    size: 0,
                    extra: "keep-me".into(),
                })
                .unwrap();
            }
            if i == 50 {
                s.append(&Record {
                    kind: Kind::Mark,
                    time: t + 1,
                    path: String::new(),
                    hash: Hash::ZERO,
                    size: 0,
                    extra: "temp".into(),
                })
                .unwrap();
                s.append(&Record {
                    kind: Kind::Unmark,
                    time: t + 2,
                    path: String::new(),
                    hash: Hash::ZERO,
                    size: 0,
                    extra: "temp".into(),
                })
                .unwrap();
            }
            // Denser towards the present: every 3 days, then hours, then minutes.
            let age = NOW - t;
            t += if age > 30 * DAY {
                3 * DAY
            } else if age > DAY {
                5 * HOUR
            } else {
                20 * 60 * 1_000_000_000
            };
            i += 1;
        }
        s.flush().unwrap();
        tmp
    }

    fn contents(s: &Store) -> Vec<Record> {
        s.records().unwrap().into_iter().map(|(_, r)| r).collect()
    }

    #[test]
    fn compaction_keeps_what_the_policy_promises() {
        let tmp = aged_store();
        let mut s = Store::open(tmp.path(), Mode::Write).unwrap();
        let before = s.records().unwrap();
        let keep = plan(&before, NOW, &Policy::default());
        let report = s
            .compact_with(NOW, &Policy::default(), false, None)
            .unwrap();
        assert_eq!(report.records_before as usize, before.len());
        assert!(report.records_after < report.records_before, "{report:?}");
        assert!(report.blobs_removed > 0);
        let after = contents(&s);
        let want: Vec<Record> = before
            .iter()
            .enumerate()
            .filter(|(i, _)| keep.contains(i))
            .map(|(_, (_, r))| r.clone())
            .collect();
        assert_eq!(after, want);
        // The newest version of each file and everything under 24 h survive.
        for p in ["a.txt", "b.txt"] {
            let newest = before.iter().rev().find(|(_, r)| r.path == p).unwrap();
            assert!(after.contains(&newest.1));
        }
        for (_, r) in &before {
            if NOW - r.time < DAY {
                assert!(after.contains(r), "snow-layer record dropped: {r:?}");
            }
        }
        // The live mark and the version it points at survive; the removed one doesn't.
        assert!(s.marks().unwrap().contains_key("keep-me"));
        assert!(!after.iter().any(|r| r.extra == "temp"));
        let mark_t = s.marks().unwrap()["keep-me"];
        for p in ["a.txt", "b.txt"] {
            let pointed = before
                .iter()
                .rfind(|(_, r)| r.path == p && r.time <= mark_t)
                .unwrap();
            assert!(after.contains(&pointed.1));
        }
        // Every kept blob is still there; the store verifies; it reopens.
        drop(s);
        assert!(verify(tmp.path()).unwrap().ok());
        let s = Store::open(tmp.path(), Mode::Write).unwrap();
        assert_eq!(contents(&s), want);
        // A dry run changes nothing.
        let mut s = s;
        let dry = s.compact_with(NOW, &Policy::default(), true, None).unwrap();
        assert_eq!(dry.records_before, dry.records_after);
        assert_eq!(contents(&s), want);
    }

    #[test]
    fn a_crash_at_any_step_leaves_a_valid_store() {
        let tmp = aged_store();
        let (before, want) = {
            let s = Store::open(tmp.path(), Mode::Write).unwrap();
            let before = s.records().unwrap();
            let keep = plan(&before, NOW, &Policy::default());
            let want: Vec<Record> = before
                .iter()
                .enumerate()
                .filter(|(i, _)| keep.contains(i))
                .map(|(_, (_, r))| r.clone())
                .collect();
            (before.into_iter().map(|(_, r)| r).collect::<Vec<_>>(), want)
        };
        let mut finished = false;
        let mut crash = 0;
        while !finished {
            let work = tempfile::tempdir().unwrap();
            copy_dir(&tmp.path().join(".fernet"), &work.path().join(".fernet"));
            {
                let mut s = Store::open(work.path(), Mode::Write).unwrap();
                finished = s
                    .compact_with(NOW, &Policy::default(), false, Some(CrashAfter(crash)))
                    .is_ok();
            }
            // Reopen as a fresh process would.
            let report = verify(work.path()).unwrap();
            assert!(
                report.ok(),
                "crash after step {crash}: {:?}",
                report.problems
            );
            let s = Store::open(work.path(), Mode::Write)
                .unwrap_or_else(|e| panic!("crash after step {crash}: {e}"));
            let got = contents(&s);
            assert!(
                got == before || got == want,
                "crash after step {crash}: history is neither before nor after"
            );
            // Queries still answer from whichever state is live.
            for p in ["a.txt", "b.txt"] {
                let v = s.version_at(p, NOW).unwrap().unwrap();
                assert!(
                    s.read(&v).is_ok(),
                    "crash after step {crash}: newest {p} unreadable"
                );
            }
            crash += 1;
            assert!(crash < 10_000);
        }
        assert!(
            crash > 5,
            "compaction should have several steps, had {crash}"
        );
    }
}

/// A complete record with a valid checksum but an invalid field at the tail
/// is corruption, not a torn write: it's reported and never truncated away.
#[test]
fn a_checksummed_bad_record_at_the_tail_is_corruption() {
    let tmp = store_with(2);
    let seg = seg_path(tmp.path(), 1);
    let mut bytes = fs::read(&seg).unwrap();
    let rec = Record {
        kind: Kind::Write,
        time: 9,
        path: "x".into(),
        hash: Hash([1; 32]),
        size: 1,
        extra: String::new(),
    };
    let mut enc = rec.encode().unwrap();
    enc[9] = 1; // flags must be 0
    let crc_at = enc.len() - 4;
    let crc = crc32c::crc32c(&enc[8..crc_at]);
    enc[crc_at..].copy_from_slice(&crc.to_le_bytes());
    bytes.extend_from_slice(&enc);
    fs::write(&seg, &bytes).unwrap();
    assert!(!verify(tmp.path()).unwrap().ok());
    assert!(Store::open(tmp.path(), Mode::Write).is_err());
    assert_eq!(fs::read(&seg).unwrap(), bytes, "nothing was truncated");
}

/// A compaction that stops after its commit point leaves this handle's index
/// matching the new log, even if the caller keeps appending.
#[test]
fn compaction_failure_after_commit_keeps_the_index_honest() {
    use fernet_fs_store::compact::{CrashAfter, Policy};
    let tmp = tempfile::tempdir().unwrap();
    const DAY: i64 = 86_400 * 1_000_000_000;
    let mut s = Store::init(tmp.path()).unwrap();
    for i in 0..20 {
        write(&mut s, "a", format!("v{i}").as_bytes(), i * DAY);
    }
    // Stop right after the manifest swap.
    let steps_before_commit = 2;
    let _ = s.compact_with(
        400 * DAY,
        &Policy::default(),
        false,
        Some(CrashAfter(steps_before_commit)),
    );
    write(&mut s, "a", b"after", 401 * DAY);
    drop(s);
    let s = Store::open(tmp.path(), Mode::Write).unwrap();
    let versions = s.versions("a").unwrap();
    let records = s.records().unwrap();
    assert_eq!(versions.len(), records.len());
    for v in &versions {
        s.read(v).unwrap();
    }
}
