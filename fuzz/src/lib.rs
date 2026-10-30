//! Shared checks for the driver fuzz targets.

use fernet_fs_formats::{Driver, NodeKind, Tree};

/// Parses `data` and checks every structural guarantee the tree makes.
pub fn check_driver(driver: &dyn Driver, data: &[u8]) -> Option<Tree> {
    let tree = driver.parse(data).ok()?;
    // Text formats are UTF-8; archives and databases needn't be.
    let text = std::str::from_utf8(data).ok();
    assert!(tree.root.0 < tree.nodes.len());
    for (i, n) in tree.nodes.iter().enumerate() {
        for r in [Some(&n.span), Some(&n.value_span), n.key_span.as_ref()].into_iter().flatten() {
            assert!(r.start <= r.end && r.end <= data.len(), "node {i}: {r:?}");
            if let Some(text) = text {
                assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
            }
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

/// The pointer and text of the first scalar in document order.
pub fn first_leaf(tree: &Tree) -> Option<(Vec<String>, String)> {
    fn walk(t: &Tree, id: fernet_fs_formats::NodeId, p: &mut Vec<String>) -> Option<(Vec<String>, String)> {
        let n = t.node(id);
        if let NodeKind::Scalar(_) = n.kind {
            return Some((p.clone(), n.text.clone()));
        }
        for (k, c) in &n.children {
            p.push(k.clone());
            if let Some(found) = walk(t, *c, p) {
                return Some(found);
            }
            p.pop();
        }
        None
    }
    walk(tree, tree.root, &mut Vec::new())
}
