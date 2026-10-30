//! The independent reader recovers every version in the v1 fixture store.

#[path = "../src/main.rs"]
#[allow(dead_code)]
mod reader;

use std::path::Path;

#[test]
fn recovers_every_version() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/store-v1");
    let records = reader::read_log(&root).unwrap();
    let expected = std::fs::read_to_string(root.join("expected.txt")).unwrap();
    let expected: Vec<Vec<&str>> = expected.lines().map(|l| l.split('\t').collect()).collect();
    assert_eq!(records.len(), expected.len());
    let (paths, marks) = reader::history(&records);
    for (r, e) in records.iter().zip(&expected) {
        assert_eq!(r.kind.to_string(), e[0]);
        assert_eq!(r.time.to_string(), e[1]);
        assert_eq!(r.path, e[2]);
        assert_eq!(r.extra, e[3]);
        if r.kind == 1 || r.kind == 3 {
            // The version is recoverable and its bytes hash to what was written.
            let bytes = reader::read_blob(&root, &r.hash, r.size).unwrap();
            assert_eq!(blake3::hash(&bytes).to_hex().as_str(), e[4]);
            let v = paths[&r.path]
                .iter()
                .find(|v| v.ordinal == r.ordinal)
                .unwrap();
            assert_eq!(v.content.map(|c| c.0), Some(r.hash));
        }
    }
    // notes.md: two versions, a tombstone, then back.
    let notes: Vec<bool> = paths["notes.md"]
        .iter()
        .map(|v| v.content.is_some())
        .collect();
    assert_eq!(notes, [true, true, false, true]);
    // The rename tombstoned the old path.
    assert!(paths["src/lib.rs"].last().unwrap().content.is_none());
    assert_eq!(marks.keys().collect::<Vec<_>>(), ["before-migration"]);
}
