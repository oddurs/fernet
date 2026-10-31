//! Content-addressed blobs: `objects/ab/cdef…`, BLAKE3 of the uncompressed
//! bytes, stored zstd-compressed (level 3).

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::{Error, Hash, Result, sync_dir};

/// zstd level for new blobs.
pub(crate) const LEVEL: i32 = 3;

/// The objects directory.
pub(crate) struct Objects {
    dir: PathBuf,
}

impl Objects {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(crate) fn path(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    pub(crate) fn contains(&self, hash: &Hash) -> bool {
        self.path(hash).is_file()
    }

    /// Stores `bytes` if not already present. Returns the hash. The blob is
    /// durable when this returns: temp file, fsync, rename, fsync directory.
    pub(crate) fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        let path = self.path(&hash);
        if path.is_file() {
            return Ok(hash);
        }
        let parent = path
            .parent()
            .ok_or(Error::Layout("object path has no parent"))?;
        if !parent.is_dir() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            // The new prefix directory's entry must be durable too.
            sync_dir(&self.dir)?;
        }
        let compressed = zstd::bulk::compress(bytes, LEVEL).map_err(|e| Error::io(&path, e))?;
        let tmp = parent.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            &hash.to_hex()[2..18]
        ));
        {
            let mut f = fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
            f.write_all(&compressed).map_err(|e| Error::io(&tmp, e))?;
            f.sync_all().map_err(|e| Error::io(&tmp, e))?;
        }
        fs::rename(&tmp, &path).map_err(|e| Error::io(&path, e))?;
        sync_dir(parent)?;
        Ok(hash)
    }

    /// Reads and decompresses a blob, checking its hash.
    pub(crate) fn get(&self, hash: &Hash, size: u64) -> Result<Vec<u8>> {
        let path = self.path(hash);
        let compressed = fs::read(&path).map_err(|e| Error::io(&path, e))?;
        // Streamed and bounded: a damaged record's size can't force a huge
        // allocation.
        let mut bytes = Vec::new();
        zstd::stream::read::Decoder::new(&compressed[..])
            .and_then(|d| {
                std::io::Read::read_to_end(
                    &mut std::io::Read::take(d, size.saturating_add(1)),
                    &mut bytes,
                )
            })
            .map_err(|_| Error::BadBlob { hash: *hash })?;
        if bytes.len() as u64 != size || Hash::of(&bytes) != *hash {
            return Err(Error::BadBlob { hash: *hash });
        }
        Ok(bytes)
    }

    /// Every stored blob hash.
    pub(crate) fn all(&self) -> Result<Vec<Hash>> {
        let mut out = Vec::new();
        let Ok(dirs) = fs::read_dir(&self.dir) else {
            return Ok(out);
        };
        for d in dirs {
            let d = d.map_err(|e| Error::io(&self.dir, e))?;
            let prefix = d.file_name().to_string_lossy().into_owned();
            if prefix.len() != 2 || !d.path().is_dir() {
                continue;
            }
            for f in fs::read_dir(d.path()).map_err(|e| Error::io(&d.path(), e))? {
                let f = f.map_err(|e| Error::io(&d.path(), e))?;
                if let Some(h) =
                    Hash::from_hex(&format!("{prefix}{}", f.file_name().to_string_lossy()))
                {
                    out.push(h);
                }
            }
        }
        Ok(out)
    }
}
