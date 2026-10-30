//! Invariant 3: bare names are never altered. Listing any real directory
//! returns exactly the real entries (minus `.fernet/` at the root), with real
//! sizes and types.

// Test helpers outside #[test] fns: failing loudly is the point.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;

use fernet_fs_core::{EntryKind, Fernet};
use proptest::prelude::*;

/// What `std::fs` says a directory holds.
fn std_listing(dir: &Path, root: bool) -> BTreeMap<OsString, (EntryKind, u64)> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| !(root && e.file_name() == ".fernet"))
        .map(|e| {
            let m = fs::symlink_metadata(e.path()).unwrap();
            let ft = m.file_type();
            let kind = if ft.is_symlink() {
                EntryKind::Symlink
            } else if ft.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            (e.file_name(), (kind, m.len()))
        })
        .collect()
}

/// Lists every real directory under the root through fernet and compares.
fn check_tree(root: &Path) {
    let f = Fernet::open(root).unwrap();
    let mut stack = vec![(root.to_owned(), Vec::<OsString>::new())];
    while let Some((dir, names)) = stack.pop() {
        let r = f
            .resolve_names(names.iter().map(OsString::as_os_str))
            .unwrap();
        let ours: BTreeMap<_, _> = f
            .list(&r)
            .unwrap()
            .into_iter()
            .map(|e| (e.name, (e.kind, e.size)))
            .collect();
        let want = std_listing(&dir, names.is_empty());
        assert_eq!(ours, want, "{}", dir.display());
        for (name, (kind, _)) in want {
            if kind == EntryKind::Dir {
                let mut next = names.clone();
                next.push(name.clone());
                stack.push((dir.join(name), next));
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Item {
    File(Vec<u8>),
    Dir(Vec<(String, Item)>),
    Link(String),
}

/// Names that look like lenses on purpose.
fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z]{1,8}(\\.(json|yaml|toml|md))?",
        "[a-z]{1,4}@(-2h|yesterday|~1|first|[a-z]{1,4}|)",
        "[a-z]{1,4}#",
        "[a-z ]{1,4}@[a-z]{1,4}#",
        "\\PC{1,8}",
    ]
    .prop_filter("valid name", |s| {
        !s.contains('/') && !s.contains('\0') && s != "." && s != ".."
    })
}

fn item() -> impl Strategy<Value = Item> {
    let leaf = prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..64).prop_map(Item::File),
        Just(Item::File(b"{\"a\": 1}".to_vec())),
        name().prop_map(Item::Link),
    ];
    leaf.prop_recursive(3, 24, 6, |inner| {
        proptest::collection::vec((name(), inner), 0..6).prop_map(Item::Dir)
    })
}

fn build(dir: &Path, items: &[(String, Item)]) {
    for (name, item) in items {
        let path = dir.join(name);
        if fs::symlink_metadata(&path).is_ok() {
            continue; // duplicate generated name
        }
        match item {
            Item::File(bytes) => fs::write(&path, bytes).unwrap(),
            Item::Dir(children) => {
                fs::create_dir(&path).unwrap();
                build(&path, children);
            }
            Item::Link(target) => std::os::unix::fs::symlink(target, &path).unwrap(),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn listings_equal_read_dir(items in proptest::collection::vec((name(), item()), 0..12)) {
        let tmp = tempfile::tempdir().unwrap();
        build(tmp.path(), &items);
        check_tree(tmp.path());
    }
}

#[test]
fn store_dir_hidden_only_at_root() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".fernet/log")).unwrap();
    fs::create_dir_all(tmp.path().join("sub/.fernet")).unwrap();
    fs::write(tmp.path().join("a.json"), "{}").unwrap();
    check_tree(tmp.path());
    let f = Fernet::open(tmp.path()).unwrap();
    let names: Vec<_> = f
        .list(&f.root_node())
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["a.json", "sub"]);
}

#[test]
fn non_utf8_names() {
    use std::os::unix::ffi::OsStringExt;
    let tmp = tempfile::tempdir().unwrap();
    let weird = OsString::from_vec(vec![b'a', 0xff, b'@', b'#']);
    fs::write(tmp.path().join(&weird), "x").unwrap();
    check_tree(tmp.path());
    let f = Fernet::open(tmp.path()).unwrap();
    let r = f.resolve_names([weird.as_os_str()]).unwrap();
    assert_eq!(f.read(&r).unwrap(), b"x");
}
