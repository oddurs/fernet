//! Every leaf of the golden corpus, reached through the resolver with a real
//! fernet path, reads back what the driver reports.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use fernet_fs_core::{Fernet, Resolved};
use fernet_fs_formats::{NodeId, NodeKind, Registry, Tree};
use fernet_fs_path::{FernetPath, encode_token};

fn walk(tree: &Tree, id: NodeId, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
    let node = tree.node(id);
    match tree.render(id) {
        Some(bytes) => out.push((prefix.to_owned(), bytes)),
        None => {
            for (key, child) in &node.children {
                walk(
                    tree,
                    *child,
                    &format!("{prefix}/{}", encode_token(key).unwrap()),
                    out,
                );
            }
        }
    }
}

#[test]
fn golden_leaves_through_the_resolver() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/fernet-formats/tests/golden");
    let f = Fernet::open(&dir).unwrap();
    let registry = Registry::default();
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let bytes = std::fs::read(&path).unwrap();
        let Some(driver) = registry.detect(&path, &bytes) else {
            continue;
        };
        let tree = driver.parse(&bytes).unwrap();
        let mut leaves = Vec::new();
        walk(&tree, tree.root, &format!("{name}#"), &mut leaves);
        for (p, want) in leaves {
            let parsed = FernetPath::parse(&p).unwrap();
            assert_eq!(parsed.to_string(), p);
            let r = f.resolve(&parsed).unwrap_or_else(|e| panic!("{p}: {e}"));
            assert!(
                matches!(
                    r,
                    Resolved::Inside {
                        node: NodeKind::Scalar(_),
                        ..
                    }
                ),
                "{p}"
            );
            assert_eq!(f.read(&r).unwrap(), want, "{p}");
            checked += 1;
        }
    }
    assert!(checked > 200);
}
