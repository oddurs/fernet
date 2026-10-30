//! Milestone 6 benchmarks, as tests. Ignored by default; CI runs them in
//! release mode: `cargo test --release -p fernet-fs --test bench -- --ignored`.

#![allow(clippy::unwrap_used, clippy::panic, clippy::cast_precision_loss)]

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use fernet_fs_core::Fernet;
use fernet_fs_path::FernetPath;

fn best_of(n: usize, mut f: impl FnMut()) -> Duration {
    (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap()
}

/// A `#` read of one key in a 1 MiB JSON file, warm, under 5 ms.
#[test]
#[ignore = "benchmark: run with --release --ignored"]
fn hash_read_in_a_1mib_json_is_under_5ms() {
    let tmp = tempfile::tempdir().unwrap();
    let mut json = String::from("{\n");
    let mut i = 0;
    while json.len() < 1 << 20 {
        let _ = writeln!(
            json,
            "  \"key{i}\": {{\"name\": \"item {i}\", \"tags\": [1, 2, 3], \"nested\": {{\"value\": {i}}}}},"
        );
        i += 1;
    }
    json.push_str("  \"last\": {\"nested\": {\"value\": \"found\"}}\n}\n");
    std::fs::write(tmp.path().join("big.json"), &json).unwrap();
    let f = Fernet::open(tmp.path()).unwrap();
    let path = FernetPath::parse("big.json#/last/nested/value").unwrap();
    let read = || {
        let r = f.resolve(&path).unwrap();
        assert_eq!(f.read(&r).unwrap(), b"found\n");
    };
    read(); // cold: parse and cache
    let warm = best_of(50, read);
    println!("# read, 1 MiB json, warm: {warm:?} ({} keys)", i + 1);
    if cfg!(debug_assertions) {
        println!("debug build: not asserting");
    } else {
        assert!(warm < Duration::from_millis(5), "{warm:?}");
    }
}

/// `cat` of a real file through the mount within 10% of the backing
/// filesystem, warm.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "benchmark: run with --release --ignored"]
fn cat_through_the_mount_is_within_10_percent() {
    if !Path::new("/dev/fuse").exists() {
        assert!(
            std::env::var_os("FERNET_REQUIRE_FUSE").is_none(),
            "FUSE required"
        );
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (backing, mnt) = (tmp.path().join("b"), tmp.path().join("m"));
    std::fs::create_dir_all(&backing).unwrap();
    std::fs::create_dir_all(&mnt).unwrap();
    let data: Vec<u8> = (0..64u32 << 20)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    std::fs::write(backing.join("big.bin"), &data).unwrap();
    let bin = assert_cmd::cargo::cargo_bin("fernet");
    assert!(
        Command::new(&bin)
            .arg("mount")
            .arg(&backing)
            .arg(&mnt)
            .args(["--daemon", "--read-only"])
            .status()
            .unwrap()
            .success()
    );
    let read = |p: &Path| assert_eq!(std::fs::read(p).unwrap().len(), data.len());
    read(&mnt.join("big.bin"));
    read(&backing.join("big.bin"));
    let direct = best_of(15, || read(&backing.join("big.bin")));
    let mounted = best_of(15, || read(&mnt.join("big.bin")));
    let _ = Command::new("fusermount3").arg("-u").arg(&mnt).status();
    let ratio = mounted.as_secs_f64() / direct.as_secs_f64();
    println!("cat 64 MiB, warm: backing {direct:?}, mount {mounted:?}, ratio {ratio:.2}");
    if !cfg!(debug_assertions) {
        assert!(ratio <= 1.10, "ratio {ratio:.2}");
    }
}
