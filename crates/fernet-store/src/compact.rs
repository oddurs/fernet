//! Compaction: thinning old versions by retention policy (brief 4.4).
//!
//! The only process that removes history (invariant 2). It rewrites the kept
//! records into new segments, swaps the manifest, then deletes the retired
//! segments and the blobs nothing references any more. Every step leaves a
//! valid store: before the manifest swap the old segments are live and the new
//! ones are unlisted (ignored); after it, the new ones are live and every blob
//! they name still exists.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use crate::record::{Kind, Record};
use crate::{
    Error, Hash, Result, Store, VersionId, config_u64, segment, write_atomic, write_manifest,
};

const HOUR: i64 = 3_600 * 1_000_000_000;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;

/// Retention layers. Versions younger than `snow` are all kept; then the
/// latest per hour until `firn`, the latest per day until `ice`, and the
/// latest per week forever. The newest version of every path and every
/// version a mark points at are always kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Keep everything younger than this (nanoseconds). Default 24 h.
    pub snow: i64,
    /// Latest per hour younger than this. Default 30 days.
    pub firn: i64,
    /// Latest per day younger than this; latest per week after. Default 1 year.
    pub ice: i64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            snow: DAY,
            firn: 30 * DAY,
            ice: 365 * DAY,
        }
    }
}

impl Policy {
    /// The policy in `config.toml` (`[retention]` `snow_hours`, `firn_days`,
    /// `ice_days`), defaults for anything missing.
    #[must_use]
    pub fn from_config(dir: &Path) -> Self {
        let d = Self::default();
        let n = |k: &str, unit: i64, default: i64| {
            config_u64(dir, k)
                .and_then(|v| i64::try_from(v).ok())
                .map_or(default, |v| v.saturating_mul(unit))
        };
        Self {
            snow: n("snow_hours", HOUR, d.snow),
            firn: n("firn_days", DAY, d.firn),
            ice: n("ice_days", DAY, d.ice),
        }
    }

    /// The bucket a version of this age falls in; `None` means keep it.
    fn bucket(&self, age: i64, time: i64) -> Option<(u8, i64)> {
        if age < self.snow {
            None
        } else if age < self.firn {
            Some((1, time.div_euclid(HOUR)))
        } else if age < self.ice {
            Some((2, time.div_euclid(DAY)))
        } else {
            Some((3, time.div_euclid(WEEK)))
        }
    }
}

/// What a compaction did, or would do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Records before.
    pub records_before: u64,
    /// Records kept.
    pub records_after: u64,
    /// Blobs no kept record names.
    pub blobs_removed: u64,
    /// Bytes freed in blobs and log.
    pub bytes_freed: u64,
}

/// The paths whose history a record is part of.
fn paths_of(r: &Record) -> Vec<&str> {
    match r.kind {
        Kind::Write | Kind::Delete => vec![r.path.as_str()],
        Kind::Rename => vec![r.path.as_str(), r.extra.as_str()],
        Kind::Mark | Kind::Unmark => Vec::new(),
    }
}

/// Which records (by position in log order) survive `policy` at `now`.
#[must_use]
pub fn plan(records: &[(VersionId, Record)], now: i64, policy: &Policy) -> BTreeSet<usize> {
    // Live marks: the latest mark record per name, unless an unmark follows it.
    let mut marks: BTreeMap<&str, Option<(usize, i64)>> = BTreeMap::new();
    for (i, (_, r)) in records.iter().enumerate() {
        match r.kind {
            Kind::Mark => {
                marks.insert(&r.extra, Some((i, r.time)));
            }
            Kind::Unmark => {
                marks.insert(&r.extra, None);
            }
            _ => {}
        }
    }
    let live: Vec<(usize, i64)> = marks.values().flatten().copied().collect();
    let mut keep: BTreeSet<usize> = live.iter().map(|&(i, _)| i).collect();

    // Each path's versions, in (time, order).
    let mut by_path: HashMap<&str, Vec<(i64, usize)>> = HashMap::new();
    for (i, (_, r)) in records.iter().enumerate() {
        for p in paths_of(r) {
            by_path.entry(p).or_default().push((r.time, i));
        }
    }
    for versions in by_path.values_mut() {
        versions.sort_unstable();
        // The newest version, and whatever each mark points at.
        if let Some(&(_, i)) = versions.last() {
            keep.insert(i);
        }
        for &(_, t) in &live {
            if let Some(&(_, i)) = versions.iter().rev().find(|(vt, _)| *vt <= t) {
                keep.insert(i);
            }
        }
        // The latest version in each bucket; everything in the snow layer.
        let mut latest: HashMap<(u8, i64), usize> = HashMap::new();
        for &(t, i) in versions.iter() {
            match policy.bucket(now.saturating_sub(t), t) {
                None => {
                    keep.insert(i);
                }
                Some(b) => {
                    latest.insert(b, i);
                }
            }
        }
        keep.extend(latest.into_values());
    }
    keep
}

/// Writes `kept` into segments `first`, `first + 1`, … rolled at `roll_at`.
/// Returns their ids and total size.
fn write_segments(
    log: &Path,
    kept: &[&Record],
    first: u64,
    roll_at: u64,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<(Vec<u64>, u64)> {
    let mut id = first;
    let mut ids = Vec::new();
    let mut written = 0u64;
    let mut buf = segment::header(id).to_vec();
    for r in kept {
        let bytes = r.encode()?;
        if buf.len() > segment::HEADER_LEN && (buf.len() + bytes.len()) as u64 > roll_at {
            check()?;
            written += buf.len() as u64;
            write_atomic(&log.join(segment::file_name(id)), &buf)?;
            ids.push(id);
            id += 1;
            buf = segment::header(id).to_vec();
        }
        buf.extend_from_slice(&bytes);
    }
    check()?;
    written += buf.len() as u64;
    write_atomic(&log.join(segment::file_name(id)), &buf)?;
    ids.push(id);
    Ok((ids, written))
}

/// Where a compaction stops, for crash testing.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrashAfter(pub usize);

impl Store {
    /// Applies the retention policy from `config.toml` at instant `now`. With
    /// `dry_run`, only reports.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] without the writer lock; I/O and index errors.
    pub fn compact(&mut self, now: i64, dry_run: bool) -> Result<Report> {
        let policy = Policy::from_config(&self.dir);
        self.compact_with(now, &policy, dry_run, None)
    }

    /// [`Store::compact`] with an explicit policy, optionally stopping after
    /// `crash` steps (the test hook for crash safety).
    ///
    /// # Errors
    ///
    /// As [`Store::compact`]; [`Error::Layout`] when stopped by `crash`.
    #[doc(hidden)]
    pub fn compact_with(
        &mut self,
        now: i64,
        policy: &Policy,
        dry_run: bool,
        crash: Option<CrashAfter>,
    ) -> Result<Report> {
        if self.active.is_none() {
            return Err(Error::ReadOnly);
        }
        self.flush()?;
        let records = self.records()?;
        let keep = plan(&records, now, policy);
        let kept: Vec<&Record> = records
            .iter()
            .enumerate()
            .filter(|(i, _)| keep.contains(i))
            .map(|(_, (_, r))| r)
            .collect();
        let referenced: BTreeSet<Hash> = kept
            .iter()
            .filter(|r| matches!(r.kind, Kind::Write | Kind::Rename))
            .map(|r| r.hash)
            .collect();
        let doomed: Vec<Hash> = self
            .objects
            .all()?
            .into_iter()
            .filter(|h| !referenced.contains(h))
            .collect();
        let blob_bytes: u64 = doomed
            .iter()
            .map(|h| fs::metadata(self.objects.path(h)).map_or(0, |m| m.len()))
            .sum();
        let mut report = Report {
            records_before: records.len() as u64,
            records_after: kept.len() as u64,
            blobs_removed: doomed.len() as u64,
            bytes_freed: blob_bytes,
        };
        if dry_run || (kept.len() == records.len() && doomed.is_empty()) {
            return Ok(report);
        }

        let mut step = 0;
        let mut check = || -> Result<()> {
            step += 1;
            match crash {
                Some(CrashAfter(n)) if step > n => {
                    Err(Error::Layout("compaction stopped (crash test)"))
                }
                _ => Ok(()),
            }
        };

        // 1. Write the kept records into fresh segments, each fsynced. They
        //    stay unlisted (ignored) until the manifest names them.
        let log = self.dir.join("log");
        let old: Vec<u64> = self.segments.clone();
        let first = old.last().copied().unwrap_or(0) + 1;
        let (new_ids, written) = write_segments(&log, &kept, first, self.roll_at, &mut check)?;

        // 2. The switch: the manifest names only the new segments.
        check()?;
        write_manifest(&self.dir, &new_ids)?;
        self.segments.clone_from(&new_ids);
        let last = *new_ids.last().ok_or(Error::Layout("no segments"))?;
        let path = log.join(segment::file_name(last));
        let file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| Error::io(&path, e))?;
        let len = file.metadata().map_err(|e| Error::io(&path, e))?.len();
        self.active = Some(crate::Active {
            file,
            id: last,
            len,
        });

        // 3. Retire the old segments.
        for id in old {
            check()?;
            let p = log.join(segment::file_name(id));
            report.bytes_freed += fs::metadata(&p).map_or(0, |m| m.len());
            fs::remove_file(&p).map_err(|e| Error::io(&p, e))?;
        }
        crate::sync_dir(&log)?;
        report.bytes_freed = report.bytes_freed.saturating_sub(written);

        // 4. Blobs nothing names any more.
        for h in &doomed {
            check()?;
            let p = self.objects.path(h);
            match fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io(&p, e)),
            }
        }

        // 5. The index points into retired segments: rebuild it.
        check()?;
        self.reindex()?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: Kind, time: i64, path: &str, extra: &str) -> (VersionId, Record) {
        (
            VersionId {
                segment: 1,
                offset: 0,
            },
            Record {
                kind,
                time,
                path: path.into(),
                hash: Hash([time.to_le_bytes()[0]; 32]),
                size: 1,
                extra: extra.into(),
            },
        )
    }

    #[test]
    fn layers() {
        let now = 400 * DAY;
        let p = Policy::default();
        let recs = vec![
            rec(Kind::Write, now - HOUR, "a", ""),            // 0 snow
            rec(Kind::Write, now - 2 * HOUR, "a", ""),        // 1 snow
            rec(Kind::Write, now - 2 * DAY, "a", ""),         // 2 firn, hour bucket X
            rec(Kind::Write, now - 2 * DAY + 60, "a", ""),    // 3 firn, same hour: newer wins
            rec(Kind::Write, now - 40 * DAY, "a", ""),        // 4 ice
            rec(Kind::Write, now - 40 * DAY + HOUR, "a", ""), // 5 ice, same day: newer wins
            rec(Kind::Write, now - 380 * DAY, "a", ""),       // 6 deep ice
            rec(Kind::Write, now - 380 * DAY + DAY, "a", ""), // 7 deep ice, same week (probably)
        ];
        let keep = plan(&recs, now, &p);
        assert!(keep.contains(&0) && keep.contains(&1));
        assert!(!keep.contains(&2) && keep.contains(&3));
        assert!(!keep.contains(&4) && keep.contains(&5));
        assert!(keep.contains(&7));
    }

    #[test]
    fn newest_and_marked_versions_survive() {
        let now = 400 * DAY;
        let recs = vec![
            rec(Kind::Write, 10 * DAY, "a", ""),
            rec(Kind::Write, 10 * DAY + 60, "a", ""),
            rec(Kind::Mark, 10 * DAY + 30, "", "m"),
            rec(Kind::Write, 10 * DAY + 90, "b", ""),
            rec(Kind::Mark, 1, "", "gone"),
            rec(Kind::Unmark, 2, "", "gone"),
        ];
        let keep = plan(&recs, now, &Policy::default());
        // The mark points at the first write of `a`; the newest of `a` and `b` stay.
        assert!(keep.contains(&0) && keep.contains(&1) && keep.contains(&2) && keep.contains(&3));
        // A removed mark goes, with its unmark.
        assert!(!keep.contains(&4) && !keep.contains(&5));
    }
}
