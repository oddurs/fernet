# fernet store format

Status: **draft, unversioned** (milestone 0 stub). The store arrives in milestone
2; this document becomes complete and frozen at v1.0 (invariant 6).

## Layout

```
.fernet/
├── FORMAT          ascii: "fernet-store 1\n"
├── MANIFEST        list of live segment ids; replaced atomically
├── config.toml     retention policy, ignore extras, limits
├── log/            000001.seg, 000002.seg … rolled at 64 MiB
├── objects/        ab/cdef0123…  (BLAKE3 hex; zstd level 3)
├── index/          disposable
└── lock            advisory lock; one writer per store
```

## Segment header

`b"FSEG"` · format version `u16` · segment id `u64` (little-endian).

## Log record

All integers little-endian.

| Field | Type | Notes |
| --- | --- | --- |
| magic | `[u8; 4]` | `b"FREC"` |
| length | `u32` | bytes after this field, including crc |
| kind | `u8` | 1 write · 2 delete · 3 rename · 4 mark · 5 unmark |
| flags | `u8` | reserved, 0 |
| time | `i64` | unix nanoseconds, UTC |
| path_len | `u16` | |
| path | `[u8]` | UTF-8, relative to root, `/`-separated |
| hash | `[u8; 32]` | BLAKE3 of uncompressed content; zero for delete/mark |
| size | `u64` | uncompressed size; 0 for delete/mark |
| extra_len | `u16` | |
| extra | `[u8]` | rename: old path · mark: mark name · else empty |
| crc | `u32` | CRC32C over kind..extra |
