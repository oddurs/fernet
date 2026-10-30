//! Zip and tar archives, read-only: the archive's own tree. Files are raw
//! bytes ([`ScalarType::Bytes`]).

use std::io::{Cursor, Read};

use crate::{Builder, Caps, Driver, DriverError, Edit, NodeId, NodeKind, ScalarType, Tree};

/// Archives larger than this, uncompressed, aren't opened (zip bombs).
pub const MAX_UNPACKED: u64 = 256 << 20;

/// The zip driver.
#[derive(Clone, Copy, Debug, Default)]
pub struct Zip;

/// The tar driver (uncompressed tar).
#[derive(Clone, Copy, Debug, Default)]
pub struct Tar;

fn bad(message: impl Into<String>) -> DriverError {
    DriverError::Syntax {
        message: message.into(),
        offset: 0,
        line: 1,
        column: 1,
    }
}

/// Builds a directory tree from `(path, content)` entries; `None` content is
/// a directory.
fn build(entries: Vec<(String, Option<Vec<u8>>)>) -> Tree {
    let mut b = Builder::default();
    let root = b.push(NodeKind::Object, 0..0, String::new());
    for (path, content) in entries {
        let parts: Vec<&str> = path
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        let Some((last, dirs)) = parts.split_last() else {
            continue;
        };
        let mut at = root;
        for d in dirs {
            at = dir(&mut b, at, d);
        }
        match content {
            None => {
                dir(&mut b, at, last);
            }
            Some(bytes) => {
                let id = b.push(
                    NodeKind::Scalar(ScalarType::Bytes),
                    0..0,
                    format!("{} bytes", bytes.len()),
                );
                b.node_mut(id).raw = bytes;
                b.add_child(at, (*last).to_owned(), id);
            }
        }
    }
    b.finish(root)
}

/// The directory `name` under `parent`, created if needed.
fn dir(b: &mut Builder, parent: NodeId, name: &str) -> NodeId {
    if let Some(id) = b
        .child_of(parent, name)
        .filter(|&id| b.node_mut(id).kind == NodeKind::Object)
    {
        return id;
    }
    let id = b.push(NodeKind::Object, 0..0, String::new());
    b.add_child(parent, name.to_owned(), id);
    id
}

impl Driver for Zip {
    fn name(&self) -> &'static str {
        "zip"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["zip", "jar"]
    }

    fn sniff(&self, head: &[u8]) -> bool {
        head.starts_with(b"PK\x03\x04")
    }

    fn parse(&self, src: &[u8]) -> Result<Tree, DriverError> {
        let mut z = zip::ZipArchive::new(Cursor::new(src))
            .map_err(|e| bad(e.to_string().to_lowercase()))?;
        let mut total = 0u64;
        let mut entries = Vec::with_capacity(z.len());
        for i in 0..z.len() {
            let mut f = z
                .by_index(i)
                .map_err(|e| bad(e.to_string().to_lowercase()))?;
            let name = String::from_utf8_lossy(f.name_raw()).into_owned();
            if f.is_dir() {
                entries.push((name, None));
                continue;
            }
            total = total.saturating_add(f.size());
            if total > MAX_UNPACKED {
                return Err(DriverError::Unsupported("archive too large to open"));
            }
            let mut bytes = Vec::new();
            (&mut f)
                .take(MAX_UNPACKED)
                .read_to_end(&mut bytes)
                .map_err(|e| bad(e.to_string().to_lowercase()))?;
            entries.push((name, Some(bytes)));
        }
        Ok(build(entries))
    }

    fn splice(&self, _: &[u8], _: &[String], _: Edit) -> Result<Vec<u8>, DriverError> {
        Err(DriverError::Unsupported(
            "archives are read-only through fernet",
        ))
    }

    fn capabilities(&self) -> Caps {
        Caps::READ_ONLY
    }
}

impl Driver for Tar {
    fn name(&self) -> &'static str {
        "tar"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["tar"]
    }

    fn sniff(&self, head: &[u8]) -> bool {
        head.get(257..262) == Some(b"ustar")
    }

    fn parse(&self, src: &[u8]) -> Result<Tree, DriverError> {
        let mut a = tar::Archive::new(Cursor::new(src));
        let mut entries = Vec::new();
        let mut total = 0u64;
        for e in a.entries().map_err(|e| bad(e.to_string().to_lowercase()))? {
            let mut e = e.map_err(|e| bad(e.to_string().to_lowercase()))?;
            let path = e
                .path()
                .map_err(|e| bad(e.to_string().to_lowercase()))?
                .to_string_lossy()
                .into_owned();
            match e.header().entry_type() {
                tar::EntryType::Directory => entries.push((path, None)),
                tar::EntryType::Regular | tar::EntryType::Continuous => {
                    total = total.saturating_add(e.size());
                    if total > MAX_UNPACKED {
                        return Err(DriverError::Unsupported("archive too large to open"));
                    }
                    let mut bytes = Vec::new();
                    e.read_to_end(&mut bytes)
                        .map_err(|e| bad(e.to_string().to_lowercase()))?;
                    entries.push((path, Some(bytes)));
                }
                // Links, devices and metadata entries aren't files.
                _ => {}
            }
        }
        Ok(build(entries))
    }

    fn splice(&self, _: &[u8], _: &[String], _: Edit) -> Result<Vec<u8>, DriverError> {
        Err(DriverError::Unsupported(
            "archives are read-only through fernet",
        ))
    }

    fn capabilities(&self) -> Caps {
        Caps::READ_ONLY
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn zip_tree() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            w.add_directory("docs/", o).unwrap();
            w.start_file("docs/a.txt", o).unwrap();
            w.write_all(b"hello").unwrap();
            w.start_file("bin/x.dat", o).unwrap();
            w.write_all(&[0, 159, 146, 150]).unwrap();
            w.finish().unwrap();
        }
        let t = Zip.parse(buf.get_ref()).unwrap();
        let get = |p: &[&str]| {
            t.get(&p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .unwrap()
        };
        assert_eq!(t.render(get(&["docs", "a.txt"])).unwrap(), b"hello");
        assert_eq!(
            t.render(get(&["bin", "x.dat"])).unwrap(),
            [0, 159, 146, 150]
        );
        assert!(Zip.parse(b"PK\x03\x04garbage").is_err());
    }

    #[test]
    fn tar_tree() {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_cksum();
        b.append_data(&mut h, "src/main.rs", &b"fn\n"[..]).unwrap();
        let bytes = b.into_inner().unwrap();
        let t = Tar.parse(&bytes).unwrap();
        let id = t.get(&["src".into(), "main.rs".into()]).unwrap();
        assert_eq!(t.render(id).unwrap(), b"fn\n");
    }
}
