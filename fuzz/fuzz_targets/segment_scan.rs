#![no_main]

use fernet_fs_store::{record, segment};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Raw record decoding: anything that decodes re-encodes to the same bytes.
    if let Ok((r, n)) = record::decode(data) {
        assert_eq!(r.encode().unwrap(), &data[..n]);
    }
    // Segment scanning: records found are in bounds, contiguous and canonical.
    let mut seg = segment::header(7).to_vec();
    seg.extend_from_slice(data);
    let scan = segment::scan(&seg, 7);
    let mut at = segment::HEADER_LEN as u64;
    for (offset, r) in &scan.records {
        assert_eq!(*offset, at);
        at += r.encode().unwrap().len() as u64;
    }
    assert_eq!(scan.valid_end, at);
    assert!(scan.valid_end <= seg.len() as u64);
    assert_eq!(scan.problem.is_none(), scan.valid_end == seg.len() as u64);
});
