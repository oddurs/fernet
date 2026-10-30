#![no_main]

use fernet_fs_formats::Yaml;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fernet_fuzz::check_driver(&Yaml, data);
});
