#![no_main]

use fernet_fs_path::{Component, FernetPath, decode_token, encode_token};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };

    // Every segment is a component, and printing is its exact inverse.
    for seg in s.split('/') {
        let c = Component::parse(seg);
        assert_eq!(c.to_string(), seg);
        if !seg.is_empty() {
            assert!(c.is_canonical());
        }
        let _ = c.pointer_token();
    }

    // Whole paths: bijective up to dropping empty segments.
    if let Ok(p) = FernetPath::parse(s) {
        let normalized: Vec<&str> = s.split('/').filter(|x| !x.is_empty()).collect();
        assert_eq!(p.to_string(), normalized.join("/"));
        assert_eq!(FernetPath::parse(&p.to_string()).as_ref(), Ok(&p));
        let view = p.lens_view();
        if let (Some(t), Some(o)) = (view.time, view.open) {
            assert!(t <= o);
        }
        let _ = view.pointer();
    }

    // Keys survive encode/decode.
    if let Some(token) = encode_token(s) {
        assert!(!token.contains('/'));
        assert_eq!(decode_token(&token).as_deref(), Some(s));
    }
});
