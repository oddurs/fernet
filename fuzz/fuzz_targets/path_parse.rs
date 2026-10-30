#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Milestone 0 placeholder: exercises the harness. Milestone 1 replaces this
    // with the parse/display bijection check.
    let _ = std::str::from_utf8(data);
});
