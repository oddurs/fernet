#![no_main]

use fernet_fs_formats::{Driver, Edit, Tar};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some(tree) = fernet_fuzz::check_driver(&Tar, data) else { return };
    // Writable drivers: rewriting the first value with its own text is a no-op.
    if Tar.capabilities().write_scalar {
        if let Some((ptr, text)) = fernet_fuzz::first_leaf(&tree) {
            if let Ok(out) = Tar.splice(data, &ptr, Edit::SetScalar(format!("{text}\n"))) {
                let again = Tar.parse(&out).expect("a splice result reparses");
                let id = again.get(&ptr).expect("the edited value is still there");
                assert_eq!(again.node(id).text, text);
            }
        }
    }
});
