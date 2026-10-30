//! Shared checks for the driver fuzz targets.

use fernet_fs_formats::{Driver, NodeKind, Tree};

/// Parses `data` and checks every structural guarantee the tree makes.
pub fn check_driver(driver: &dyn Driver, data: &[u8]) -> Option<Tree> {
    let tree = driver.parse(data).ok()?;
    let text = std::str::from_utf8(data).expect("a parsed tree implies utf-8");
    assert!(tree.root.0 < tree.nodes.len());
    for (i, n) in tree.nodes.iter().enumerate() {
        for r in [Some(&n.span), Some(&n.value_span), n.key_span.as_ref()].into_iter().flatten() {
            assert!(r.start <= r.end && r.end <= data.len(), "node {i}: {r:?}");
            assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
        }
        for (_, c) in &n.children {
            assert!(c.0 < tree.nodes.len());
        }
        let mut keys: Vec<_> = n.children.iter().map(|(k, _)| k).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), n.children.len(), "duplicate keys under node {i}");
        if n.kind == NodeKind::Array {
            for (j, (k, _)) in n.children.iter().enumerate() {
                assert_eq!(*k, j.to_string());
            }
        }
        if let NodeKind::Scalar(_) = n.kind {
            assert!(n.children.is_empty());
            let _ = tree.render(fernet_fs_formats::NodeId(i));
        }
    }
    Some(tree)
}
