//! `fernet verify`: check the log, blobs and manifest without changing them.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::record::{Bad, Kind};
use crate::segment::{self, Problem};
use crate::{DIR, Error, FORMAT_LINE, Hash, Objects, Result, read_manifest};

/// What `verify` found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Segments checked.
    pub segments: u64,
    /// Valid records.
    pub records: u64,
    /// Blobs checked.
    pub blobs: u64,
    /// Problems, one line each, lowercase.
    pub problems: Vec<String>,
    /// Things that are fine but worth knowing (a torn final record that the
    /// next writer will truncate).
    pub notes: Vec<String>,
}

impl Report {
    /// True if nothing is wrong.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Checks the store under `root`. Never modifies it.
///
/// # Errors
///
/// [`Error::NotAStore`] and unreadable manifests; everything else is reported.
pub fn verify(root: &Path) -> Result<Report> {
    let dir = root.join(DIR);
    let mut report = Report::default();
    match fs::read_to_string(dir.join("FORMAT")) {
        Ok(s) if s == FORMAT_LINE => {}
        Ok(s) => report
            .problems
            .push(format!("format file says {:?}", s.trim())),
        Err(_) => return Err(Error::NotAStore(root.to_owned())),
    }
    let segments = read_manifest(&dir)?;
    let objects = Objects::new(dir.join("objects"));
    let mut referenced = BTreeSet::new();
    for (i, &id) in segments.iter().enumerate() {
        let path = dir.join("log").join(segment::file_name(id));
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                report.problems.push(format!("segment {id}: {e}"));
                continue;
            }
        };
        report.segments += 1;
        let scan = segment::scan(&bytes, id);
        report.records += scan.records.len() as u64;
        let active = i + 1 == segments.len();
        match scan.problem {
            None => {}
            Some(Problem::Header) => report.problems.push(format!("segment {id}: bad header")),
            Some(Problem::Record {
                offset,
                at_tail: true,
                ..
            }) if active => report.notes.push(format!(
                "segment {id}: torn final record at byte {offset}; the next writer truncates it"
            )),
            Some(Problem::Record { offset, bad, .. }) => {
                let why = match bad {
                    Bad::Truncated => "truncated record",
                    Bad::Magic => "bad record magic",
                    Bad::Length => "impossible record length",
                    Bad::Crc => "checksum mismatch",
                    Bad::Field => "invalid record field",
                };
                report
                    .problems
                    .push(format!("segment {id}: {why} at byte {offset}"));
            }
        }
        for (_, r) in scan.records {
            if matches!(r.kind, Kind::Write | Kind::Rename) {
                referenced.insert((r.hash, r.size));
            }
        }
    }
    let mut checked: BTreeSet<Hash> = BTreeSet::new();
    for (hash, size) in referenced {
        if !checked.insert(hash) {
            continue;
        }
        report.blobs += 1;
        if objects.get(&hash, size).is_err() {
            report.problems.push(format!(
                "blob {} is missing or damaged",
                &hash.to_hex()[..12]
            ));
        }
    }
    Ok(report)
}
