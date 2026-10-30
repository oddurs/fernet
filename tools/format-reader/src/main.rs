//! A minimal, independent reader for fernet stores, written only from
//! `docs/FORMAT.md` (invariant 6). Usage:
//!
//! ```text
//! fernet-format-reader <root>             list every record
//! fernet-format-reader <root> <out-dir>   also write every version's bytes
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// One record (FORMAT.md section 4), plus its position in log order.
#[derive(Debug, Clone)]
pub struct Record {
    pub ordinal: u64,
    pub kind: u8,
    pub time: i64,
    pub path: String,
    pub hash: [u8; 32],
    pub size: u64,
    pub extra: String,
}

fn u16_at(b: &[u8], i: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(
        b.get(i..i + 2)
            .ok_or("short")?
            .try_into()
            .map_err(|_| "short")?,
    ))
}
fn u32_at(b: &[u8], i: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        b.get(i..i + 4)
            .ok_or("short")?
            .try_into()
            .map_err(|_| "short")?,
    ))
}
fn u64_at(b: &[u8], i: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        b.get(i..i + 8)
            .ok_or("short")?
            .try_into()
            .map_err(|_| "short")?,
    ))
}

/// Reads every record of the store under `root`, in log order (sections 1-4).
pub fn read_log(root: &Path) -> Result<Vec<Record>, String> {
    let dir = root.join(".fernet");
    let format = fs::read_to_string(dir.join("FORMAT")).map_err(|e| format!("FORMAT: {e}"))?;
    if format != "fernet-store 1\n" {
        return Err(format!("unsupported store: {format:?}"));
    }
    let manifest =
        fs::read_to_string(dir.join("MANIFEST")).map_err(|e| format!("MANIFEST: {e}"))?;
    let mut lines = manifest.lines();
    if lines.next() != Some("fernet-manifest 1") {
        return Err("bad manifest header".into());
    }
    let mut records = Vec::new();
    for line in lines {
        let id: u64 = line
            .parse()
            .map_err(|_| format!("bad manifest line {line:?}"))?;
        let seg = fs::read(dir.join("log").join(format!("{id:06}.seg")))
            .map_err(|e| format!("segment {id}: {e}"))?;
        // Header: "FSEG", version 1 (u16), id (u64).
        if seg.get(..4) != Some(b"FSEG") || u16_at(&seg, 4)? != 1 || u64_at(&seg, 6)? != id {
            return Err(format!("segment {id}: bad header"));
        }
        let mut at = 14;
        while at < seg.len() {
            if seg.get(at..at + 4) != Some(b"FREC") {
                return Err(format!("segment {id} byte {at}: bad magic"));
            }
            let length = u32_at(&seg, at + 4)? as usize;
            let body = seg
                .get(at + 8..at + 8 + length)
                .ok_or_else(|| format!("segment {id} byte {at}: torn record"))?;
            let (payload, crc) = body.split_at(length - 4);
            if crc32c::crc32c(payload) != u32_at(crc, 0)? {
                return Err(format!("segment {id} byte {at}: crc mismatch"));
            }
            let path_len = u16_at(payload, 10)? as usize;
            let p = 12 + path_len;
            let extra_len = u16_at(payload, p + 40)? as usize;
            let text = |r: std::ops::Range<usize>| -> Result<String, String> {
                String::from_utf8(payload.get(r).ok_or("short")?.to_vec())
                    .map_err(|_| "not utf-8".to_string())
            };
            records.push(Record {
                ordinal: records.len() as u64,
                kind: payload[0],
                time: u64_at(payload, 2)? as i64,
                path: text(12..p)?,
                hash: payload
                    .get(p..p + 32)
                    .ok_or("short")?
                    .try_into()
                    .map_err(|_| "short")?,
                size: u64_at(payload, p + 32)?,
                extra: text(p + 42..p + 42 + extra_len)?,
            });
            at += 8 + length;
        }
    }
    Ok(records)
}

fn hex(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads and checks a blob (section 5).
pub fn read_blob(root: &Path, hash: &[u8; 32], size: u64) -> Result<Vec<u8>, String> {
    let h = hex(hash);
    let path = root.join(".fernet/objects").join(&h[..2]).join(&h[2..]);
    let compressed = fs::read(&path).map_err(|e| format!("blob {h}: {e}"))?;
    let bytes = zstd::decode_all(&compressed[..]).map_err(|e| format!("blob {h}: {e}"))?;
    if bytes.len() as u64 != size || blake3::hash(&bytes).as_bytes() != hash {
        return Err(format!("blob {h}: content doesn't match its name"));
    }
    Ok(bytes)
}

/// One version of one path: `None` content is a tombstone (section 4, "Meaning").
#[derive(Debug, Clone)]
pub struct Version {
    pub time: i64,
    pub ordinal: u64,
    pub content: Option<([u8; 32], u64)>,
}

/// Every path's versions, ordered by (time, ordinal), and the live marks.
pub fn history(records: &[Record]) -> (BTreeMap<String, Vec<Version>>, BTreeMap<String, i64>) {
    let mut paths: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    let mut marks = BTreeMap::new();
    for r in records {
        let mut add = |p: &str, content| {
            paths.entry(p.to_owned()).or_default().push(Version {
                time: r.time,
                ordinal: r.ordinal,
                content,
            })
        };
        match r.kind {
            1 => add(&r.path, Some((r.hash, r.size))),
            2 => add(&r.path, None),
            3 => {
                add(&r.path, Some((r.hash, r.size)));
                add(&r.extra, None);
            }
            4 => {
                marks.insert(r.extra.clone(), r.time);
            }
            5 => {
                marks.remove(&r.extra);
            }
            k => eprintln!("record {}: unknown kind {k}, skipped", r.ordinal),
        }
    }
    for v in paths.values_mut() {
        v.sort_by_key(|v| (v.time, v.ordinal));
    }
    (paths, marks)
}

fn main() {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let Some(root) = args.first() else {
        eprintln!("usage: fernet-format-reader <root> [out-dir]");
        std::process::exit(2);
    };
    let records = read_log(root).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });
    let (paths, marks) = history(&records);
    for (path, versions) in &paths {
        for v in versions {
            match v.content {
                Some((hash, size)) => {
                    println!("{}\t{path}\t{size}\t{}", v.time, hex(&hash));
                    if let Some(out) = args.get(1) {
                        let bytes = read_blob(root, &hash, size).unwrap_or_else(|e| {
                            eprintln!("error: {e}");
                            std::process::exit(1);
                        });
                        let file = out.join(format!("{path}@{}", v.time));
                        fs::create_dir_all(file.parent().unwrap_or(out)).ok();
                        fs::write(&file, bytes).ok();
                    }
                }
                None => println!("{}\t{path}\tdeleted", v.time),
            }
        }
    }
    for (name, time) in marks {
        println!("{time}\tmark\t{name}");
    }
}
