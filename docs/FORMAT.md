# fernet store format

Status: **stable. Format version 1, frozen at fernet 1.0** (invariant 6).

This document describes everything a reader needs to recover every version from
a store. `tools/format-reader/` is an independent reader written from this
document alone; its test recovers every version of the committed fixture store
`tests/fixtures/store-v1`.

**Compatibility promise.** Every later fernet reads version 1 stores. A change
to anything here needs a new `FORMAT` line (`fernet-store 2`), and fernet keeps
reading `fernet-store 1`. Additions that old readers can safely ignore (new
`config.toml` keys, new files that aren't listed here) don't change the version.

All integers are little-endian. All text is UTF-8. "Fsync" means a durable
flush of a file's contents (`fsync`/`F_FULLFSYNC`); "fsync the directory" means
the same on the containing directory, so that renames and creations in it are
durable.

## 1. Layout

The store lives in `.fernet/` at the root of the backing directory:

```
.fernet/
├── FORMAT          ascii: "fernet-store 1\n"
├── MANIFEST        live segment ids
├── config.toml     settings (section 7)
├── log/            000001.seg, 000002.seg, …
├── objects/        ab/cdef0123…
├── index/          disposable
└── lock            advisory lock
```

Paths in this document are relative to `.fernet/` unless they start with it.
A directory without `FORMAT` is not a store. `FORMAT` is written last on
creation, so a half-created store is never opened. A reader that sees any other
`FORMAT` content must refuse the store.

## 2. Manifest

`MANIFEST` is text: the line `fernet-manifest 1`, then one decimal segment id
per line, strictly ascending, each line ending in `\n`. At least one id is
listed. The last id is the **active** segment; all others are **sealed**.

`MANIFEST` is only ever replaced atomically: write `MANIFEST.tmp`, fsync it,
rename it over `MANIFEST`, fsync the directory. Segment files not listed in the
manifest are not part of the store and must be ignored.

## 3. Segments

Segment `N` is the file `log/` + `N` in decimal, zero-padded to at least six
digits, + `.seg` (`000001.seg`, `1234567.seg`). It holds a 14-byte header
followed by zero or more records, back to back:

| Offset | Field | Type | Value |
| --- | --- | --- | --- |
| 0 | magic | `[u8; 4]` | `b"FSEG"` |
| 4 | version | `u16` | `1` |
| 6 | id | `u64` | `N`, matching the file name |

Records are only ever appended. A new segment is started when appending a
record would take the active segment past `segment_bytes` (default 64 MiB):
create the new file with its header, fsync it, fsync the directory, then
replace the manifest with the new id appended.

## 4. Records

| Offset | Field | Type | Notes |
| --- | --- | --- | --- |
| 0 | magic | `[u8; 4]` | `b"FREC"` |
| 4 | length | `u32` | bytes after this field, including crc |
| 8 | kind | `u8` | 1 write · 2 delete · 3 rename · 4 mark · 5 unmark |
| 9 | flags | `u8` | reserved, must be 0 |
| 10 | time | `i64` | unix nanoseconds, UTC |
| 18 | path_len | `u16` | |
| 20 | path | `[u8; path_len]` | relative to the root, `/`-separated, no leading `/`; empty for marks |
| … | hash | `[u8; 32]` | BLAKE3 of the uncompressed content; zero for delete, mark, unmark |
| … | size | `u64` | uncompressed content size; 0 for delete, mark, unmark |
| … | extra_len | `u16` | |
| … | extra | `[u8; extra_len]` | rename: the old path · mark/unmark: the mark name · else empty |
| … | crc | `u32` | CRC32C (Castagnoli) over the bytes from kind through extra |

So `length = 1 + 1 + 8 + 2 + path_len + 32 + 8 + 2 + extra_len + 4`, which is
at least 58. A record is valid only if its magic, length, crc, kind and flags
are all as above and both text fields are UTF-8.

### Meaning

Records are read in log order: segments in manifest order, records in file
order. A record's **ordinal** is its position in that order, starting at 0.

- **write** (1): from `time` on, `path` has content `hash` of length `size`.
- **delete** (2): from `time` on, `path` doesn't exist (a tombstone).
- **rename** (3): from `time` on, `path` has content `hash` and the old path
  in `extra` doesn't exist.
- **mark** (4): the mark named `extra` points at `time`. Names match
  `[a-z][a-z0-9-]{0,63}`.
- **unmark** (5): the mark named `extra` no longer exists.

The state of a path at instant `T` is given by its latest record (write,
delete, or rename naming it as `path` or as `extra`) with `time ≤ T`, ties
broken by the larger ordinal. Directories are implicit: a directory exists at
`T` if any path below it exists at `T`.

Times are not required to be monotonic in log order. Readers must order by
`(time, ordinal)`.

## 5. Objects

Each distinct content is stored once, at `objects/` + the first two hex digits
of its BLAKE3 hash + `/` + the remaining 62 (lowercase). The file holds one
zstd frame (written at level 3) whose decompressed bytes hash to the name.

Blobs are written before any record that names them: write a temp file in the
same directory, fsync it, rename it into place, fsync the directory.

## 6. Durability and recovery

- Appended records are fsynced in groups, at most `group_commit_ms` apart
  (default 50 ms), and always when a writer flushes or closes.
- Because of that, a crash can tear the **final** record of the active segment.
  On open, a writer scans the active segment. If the bytes after the last valid
  record are a record that runs past the end of the file, or one that ends
  exactly at the end of the file but fails its crc, the file is truncated to the
  end of the last valid record and fsynced.
- Any other invalid bytes, in any segment, are corruption. They are reported
  and never skipped. A writer refuses to open a store whose active segment is
  corrupt before its tail.

## 7. `config.toml`

TOML. Settings fernet reads (anything else is ignored):

| Table | Key | Default | Meaning |
| --- | --- | --- | --- |
| `[log]` | `group_commit_ms` | 50 | maximum fsync interval for records |
| `[log]` | `segment_bytes` | 67108864 | roll segments at this size |
| `[capture]` | `debounce_ms` | 2000 | a path is captured once quiet this long |
| `[retention]` | `snow_hours` | 24 | compaction keeps every version younger than this |
| `[retention]` | `firn_days` | 30 | then the latest per hour until this age |
| `[retention]` | `ice_days` | 365 | then the latest per day until this age; the latest per week after |
| `[limits]` | `max_file_size` | 52428800 | files larger than this aren't captured |
| `[ignore]` | `patterns` | `[]` | extra gitignore-style patterns |

## 8. Compaction

Compaction is the only process that removes records (invariant 2). It is a
rewrite, never an edit:

1. Choose the records to keep. For each path, keep the newest record, the latest
   record at or before each live mark's time, every record younger than
   `snow_hours`, and the latest record in each hour, day or week bucket (UTC,
   by age as in section 7). Keep each live mark's latest `mark` record. Drop
   `unmark` records and the marks they removed. A `rename` is kept if either of
   its paths keeps it.
2. Write the kept records, in their original order, into new segments numbered
   after the highest live id, each written and fsynced as in section 3.
3. Replace `MANIFEST` with the new ids only. This is the commit point.
4. Delete the retired segment files, then any blob no kept record names.

A reader that finds unlisted segments or unreferenced blobs ignores them. They
are what an interrupted compaction leaves behind.

## 9. Index

`index/` holds derived lookup tables and can be deleted at any time
(invariant 5). It isn't part of the format; readers should ignore it and replay
the log.

## 10. Lock

`lock` is an empty file. A writer holds an exclusive advisory lock on it
(`flock`) for as long as it may append. Readers don't need it.
