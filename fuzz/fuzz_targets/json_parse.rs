#![no_main]

use fernet_fs_formats::Json;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let ours = fernet_fuzz::check_driver(&Json, data);
    // Differential: anything strict JSON accepts, we accept, with the same
    // number of leaves (modulo duplicate keys, which collapse in both).
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) {
        let tree = ours.expect("serde_json accepted input the json driver rejected");
        fn leaves(v: &serde_json::Value) -> usize {
            match v {
                serde_json::Value::Object(m) => m.values().map(leaves).sum(),
                serde_json::Value::Array(a) => a.iter().map(leaves).sum(),
                _ => 1,
            }
        }
        fn ours_leaves(t: &fernet_fs_formats::Tree, id: fernet_fs_formats::NodeId) -> usize {
            let n = t.node(id);
            match n.kind {
                fernet_fs_formats::NodeKind::Scalar(_) => 1,
                _ => n.children.iter().map(|(_, c)| ours_leaves(t, *c)).sum(),
            }
        }
        assert_eq!(leaves(&v), ours_leaves(&tree, tree.root));
    }
});
