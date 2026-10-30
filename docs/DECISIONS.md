# Decisions

Numbered, append-only. Each entry: context, decision, alternatives rejected,
consequences. A later entry may supersede an earlier one; the earlier one stays.

## 1. Package names on crates.io

**Context.** `fernet` on crates.io is the Fernet encryption-token crate. The other
names we want are free, but `fernet-core` and friends read as part of that family.

**Decision.** Every package is published under a `fernet-fs-` prefix:
`fernet-fs` (the CLI package; its binary is still named `fernet`),
`fernet-fs-path`, `fernet-fs-formats`, `fernet-fs-store`, `fernet-fs-core`,
`fernet-fs-fuse`. Directories keep the short names from the brief
(`crates/fernet-path`, …). Rust import paths are `fernet_fs_path` etc.

**Rejected.** Renaming only the binary package: leaves the library names
confusable with the token crate. A different project name: the name is the brand.

**Consequences.** Import paths are a little longer. Directory and package names
differ; the workspace `Cargo.toml` maps them.

## 2. License

**Decision.** `MIT OR Apache-2.0`, the Rust ecosystem default. `deny.toml` allows
only permissive licenses for dependencies.

## 3. Toolchain pin vs MSRV

**Context.** The brief asks for an MSRV "pinned in `rust-toolchain.toml`".
Pinning the toolchain to the MSRV would mean developing and linting on the oldest
compiler.

**Decision.** `rust-toolchain.toml` pins a recent stable (1.97.0) so clippy and
rustfmt are reproducible. The MSRV is `rust-version` in the workspace
`Cargo.toml`, initially 1.85 (the edition 2024 floor), and a CI job checks it.
Resolver 3 makes dependency resolution respect it.

**Rejected.** Pinning the toolchain to the MSRV; floating `stable` (new lints
would break CI unannounced).

**Consequences.** Toolchain bumps and MSRV bumps are deliberate commits; each MSRV
bump gets an entry here.

## 4. `tests/` is an unpublished crate

**Context.** Cargo has no workspace-level test directory; a virtual manifest can't
own integration tests.

**Decision.** `tests/` is the package `fernet-tests` (`publish = false`). Its
`src/lib.rs` holds shared helpers and `tests/tests/*.rs` holds the cross-crate
tests. `cargo test --workspace` runs them.

**Consequences.** Keeps the layout in the brief.

## 5. Fuzz crate outside the workspace

**Decision.** `fuzz/` is its own workspace (empty `[workspace]` table) with its own
lockfile, because cargo-fuzz needs nightly and sanitizer flags. CI fuzzes each
target for 60 s in a matrix, so adding targets doesn't shrink anyone's budget.

## 6. Lint policy

**Decision.** `clippy::pedantic` is denied workspace-wide, as are `unwrap_used`,
`expect_used`, `panic`, `dbg_macro` and `todo` (tests exempt via `clippy.toml`).
`missing_docs` is denied. `fernet-fs-fuse` repeats the lint table with
`unsafe_code = "deny"` instead of `forbid`, plus
`clippy::undocumented_unsafe_blocks`. Every local `#[allow]` carries a
justification comment.

## 7. The grammar is total and bijective

**Context.** "Real names win" means the resolver must check the backing directory
for the component's exact text before it reads any lens. The parser can't know
which names are real, so it must not reject or normalise anything.

**Decision.** `Component::parse` never fails. A lens is recognised only in
canonical spelling; anything else is a plain name. Printing is the exact inverse
of parsing for every segment. Segments split at the **last** `@` (mark names and
specs never contain `@`). The only path-level errors are empty, absolute, and NUL.

**Rejected.** Rejecting malformed specs (`x@-02h` would become unreachable even
when it's a real file). Normalising specs (the resolver would look up the wrong
real name).

**Consequences.** A typo in a spec reads as a missing file rather than a syntax
error, so the CLI has to explain likely typos in its not-found messages.
Property tests check both `parse(display(p)) == p` and `display(parse(s)) == s`.

## 8. `TimeSpec::Relative { amount, unit }` instead of `Relative(Duration)`

**Context.** Brief 4.2 has `Relative(Duration)`. A `Duration` can't tell
`@-120m` from `@-2h`, so it couldn't print back what was written (entry 7).

**Decision.** Store the amount and unit; `TimeSpec::duration()` converts. Amounts
whose duration overflows `u64` seconds are not specs. `AbsoluteTime` likewise keeps
the written precision, and `-00:00` stays distinct from `+00:00`.

## 9. Pointer-token extension for unnameable keys

**Context.** The kernel never delivers `""`, `.` or `..` as names, but JSON allows
them as keys.

**Decision.** Whole-token forms `~e`, `~.` and `~..`. RFC 6901 forbids `~`
followed by anything but `0` or `1`, so this can't clash with a standard pointer.
Keys with NUL or an encoded length over 255 bytes are left out of listings.

## 10. Lens placement: one of each, time first

**Context.** Brief 2.1 says "one component may carry lenses", but section 1 shows
`src@2026-10-01/` with paths beneath it.

**Decision.** At most one time lens and one open lens apply, the time lens on the
open component or before it. Later lens-shaped segments are literal.

## 11. Additions to the driver types in brief 4.3

**Decision.**
- `ScalarType::Datetime`, for TOML's first-class date-times. Folding them into
  `String` would let a type-preserving write turn a date into a string later.
- `Node::key_span`, so listings, messages and inserts know where key text sits.
- `Node::text`, the scalar as fernet shows it, decoded once at parse time.
  Strings are unescaped; every other scalar is its source text (`0x1F`, `1e3`
  and `1979-05-27 07:32:00Z` read back as written).
- `Driver::extensions` and `Driver::sniff`, with `detect` as a provided method.
  `Registry` uses them separately, so one driver's sniff never overrides another
  driver's extension. Sniffing only runs for unclaimed extensions, is skipped
  for heads containing NUL, and runs TOML before JSON (`[[package]]`).
- Duplicate keys keep their first position and take the last value, matching
  `serde_json`.

## 12. TOML via `toml_edit`

**Context.** Brief 4.3 asks whether `toml_edit` exposes reliable byte spans.

**Decision.** It does: parsing into `toml_edit::Document` (before `despan`)
gives exact byte ranges for keys, values, inline-table entries and table headers,
including dotted keys and arrays of tables. Verified by a spike and kept as unit
and golden tests. It doesn't accept a BOM, so the driver parses after it and
shifts spans.

**Consequences.** A table's `span` is its header line only: TOML tables can be
split across a file (`[a]` … `[c]` … `[a.b]`), so "the whole node" isn't one
range. Removal (M4) will need a list of ranges. Implicit tables (`a` in `[a.b]`)
use their key's span.

## 13. YAML via `saphyr-parser` events

**Context.** Brief 4.3 suggests the saphyr family.

**Decision.** `saphyr-parser` 0.3's event stream gives start and end markers
for every scalar. Two adjustments: markers count **characters**, so the driver
maps them to byte offsets; and a scalar's span includes its `&anchor`/`!tag`
properties and, for block scalars, starts at the content. The driver strips
properties and widens block scalars to start at their `|`/`>` header, so every
value span is a self-contained YAML value.

v0.1 read semantics: the first document only; aliases resolve to the anchored
node (recursive aliases are an error); `<<` merge keys are ordinary keys;
mapping, sequence and alias keys are left out; untagged plain scalars follow the
YAML 1.2 core schema; quoted, block and custom-tagged scalars are strings. An
empty value (`a:`) is a null that reads as an empty line.

**Rejected.** `yaml-rust2` (same lineage, older span support). Writing our own
YAML parser.

## 14. Reference parsers for golden tests (dev-only)

**Decision.** `serde_json` (with `arbitrary_precision` and `preserve_order`),
`toml`, and `serde_norway` for YAML. `serde_yaml` itself is deprecated, and
`saphyr` shares our parser's lineage, so it can't be an independent reference.
The JSONC golden files go through a small comment and trailing-comma stripper
written in the test.

## 15. The JSON driver accepts JSONC everywhere

**Decision.** `//` and `/* */` comments and trailing commas are accepted in any
JSON file, not just `.jsonc`. Strict JSON is a subset, so nothing valid is
rejected; tsconfig and VS Code settings need both. Nesting is limited to 512
levels in every driver.

## 16. The resolver is a per-component step function

**Context.** Invariant 7 says the CLI and the mount resolve identically. The
kernel looks paths up one component at a time, and "real names win" has to be
checked per component.

**Decision.** `Fernet::lookup(parent, name)` is the only resolution primitive.
`Fernet::resolve(&FernetPath)` folds it over the path's components printed back
to their exact text (entry 7), and `resolve_names` does the same for raw
`OsStr` names, so non-UTF-8 real names work. The mount (M3) will call `lookup`
directly.

**Consequences.** `Inside` nodes carry a decoded pointer, not a node id, so each
step re-walks from the file's root through a cached tree. That's cheap (pointers
are short) and stays correct if the file changes between lookups.

## 17. A tree cache from milestone 1

**Decision.** Parsed trees are cached by `(path, mtime, size)` in a bounded LRU
(64 MiB of source by default). Brief 4.5 places it in core without a milestone;
without it, resolving `a.json#/x/y/z` would parse the file once per component.
Blob-hash keys arrive with the store.

## 18. CLI root and paths

**Decision.** The root is `--root`, else the nearest ancestor of the current
directory holding `.fernet/`, else the current directory. Path arguments are
relative to the current directory or absolute; `.` and `..` apply lexically and
the result must lie inside the root (exit 2 otherwise). `.fernet` is hidden only
at the root: a nested `.fernet` directory is ordinary user data.

## 19. `#` on files no driver handles is not found

**Context.** Brief §8 Q5.

**Decision.** `#` on an unknown or binary format resolves to nothing (`ENOENT`,
exit 1). A raw byte-range view can come later as an explicit driver without
breaking anything.

## 20. Error mapping

**Decision.** One `fernet_fs_core::Error` enum. The mount maps it with
`Error::errno()`; the CLI maps it to the exit codes in brief 4.7: not found → 1;
directory/file mix-ups, unknown subcommands and paths outside the root → 2; a
file its driver can't parse → 3; a time lens without a store → 5; other I/O → 10.
Messages are lowercase, name the path relative to the root, and come with a
`try:` line where there's an obvious next step.

## 21. Extra dependencies in milestone 1

- `thiserror` (libraries), `anyhow` and `clap` (binary): named in the brief.
- `tracing` and `tracing-subscriber` (`env-filter`, `fmt` only): logging is
  `tracing`, off by default, `FERNET_LOG` to enable.
- `toml_edit` and `saphyr-parser`: entries 12 and 13.
- Dev-only: `proptest`, `tempfile`, `assert_cmd`, `serde`, `serde_json`, `toml`,
  `serde_norway`. The fuzz crate adds `libfuzzer-sys`.
- Not used, despite PLAN.md: `arbitrary` and a structured path fuzz target.
  The proptest generators already cover structured paths; the string target
  checks the bijection on arbitrary input.

## 22. MSRV 1.90

**Context.** `redb` 4 needs Rust 1.90.

**Decision.** Raise the MSRV from 1.85 to 1.90. That also brings
`std::fs::File::try_lock` (1.89) for the store lock, so no locking crate is
needed, and let-chains.

## 23. Two index backends; readers never block on the writer

**Context.** `redb` holds an exclusive lock on its file. While `fernet watch`
(or, later, the mount) holds the store, a short-lived `fernet cat x@-1h` couldn't
open the index.

**Decision.** The process holding the store lock uses the persistent `redb`
index in `.fernet/index/fernet.redb`. A reader that can't get the lock replays
the log into an in-memory index instead. Both backends sit behind one trait,
with one set of queries, and the reindex property test checks that they agree.
Index commits use `Durability::None` and are made durable on flush: a lost index
commit is replayed from the log on the next open. An index that is missing, from
another format, or positioned beyond the end of the log is rebuilt.

**Consequences.** Read-only opens cost a full log replay. That's fine at v0.2
sizes; a read-only snapshot of the index can come later if needed.

## 24. Record semantics the brief left open

- **Ordering.** State at instant `T` is the latest record with `time ≤ T`, ties
  broken by log order. Times needn't be monotonic in the log (clocks move).
- **Rename** writes the new path and tombstones the old one, both at the record's
  time.
- **Marks** are store-wide and unique by name. Re-marking an existing name is an
  error (`--rm` it first). Mark names follow the grammar's rules.
- **Directories** are implicit: there are no directory records, so empty
  directories have no history.
- **Non-UTF-8 paths** can't be recorded (the format says UTF-8) and are not
  captured; the present tree still lists and reads them.
- **Identical content** is never stored twice: a snapshot whose bytes hash the
  same as the path's latest version records nothing.

## 25. Time lenses on directories

**Decision.** Instants (relative, named days, absolute, marks) work on
directories. `@first` on a directory is the earliest instant anything in it
existed. `@~N` and `@` (listing) count versions of one file, so on directories
they resolve to nothing. Version-listing names that collide within a second get
`.1`, `.2` suffixes (PLAN.md Q10).

## 26. `verify` never modifies the store

**Decision.** `fernet verify` only reads. A torn final record in the active
segment is a note, not a problem: the next writer truncates it. Everything else
(bad headers, bad records anywhere else, missing or damaged blobs) is a problem,
and the exit code is 5.

## 27. Settings read with a line scan

**Decision.** The store reads its few integer settings and the ignore pattern
list from `config.toml` with a line scan rather than a TOML parser, to keep
`fernet-fs-store` free of the `toml` crates. The file is written by fernet with
one setting per line. Malformed values fall back to defaults. If `config.toml`
grows nested structure, switch to `toml_edit`, which is already in the tree.

## 28. The watcher

**Decision.** `fernet watch` takes the writer lock, captures the whole tree once
(recording tombstones for files that vanished while nothing was watching), then
follows `notify` events, debounced until the tree has been quiet for 200 ms.
Records are flushed after every batch. `notify` is built with only its macOS
FSEvents backend feature (Linux inotify needs no feature).

## 29. Extra dependencies in milestone 2

- `blake3`, `zstd`, `crc32c`, `redb`, `jiff`, `notify`, `ignore`: named in the brief.
- `similar` for `fernet diff`'s unified diffs: small, maintained, no dependencies.
- Dev-only: `predicates` (CLI assertions); `jiff` with `tzdb-bundle-always` so
  time-zone tests don't depend on the machine's tzdb.

## 30. The mount maps resolved nodes to inodes, nothing more

**Decision.** `fernet-fs-fuse` keeps an inode table keyed by `Resolved` (which
is `Hash` for this purpose). `lookup` calls `Fernet::lookup` once per kernel
lookup, so invariant 7 holds by construction. Real files and directories keep
their inode for the life of the mount; virtual nodes are dropped once the
kernel's lookup count reaches zero. `readdir` returns the listing captured at
`opendir`, giving each entry a stable inode without taking a lookup reference.

**Consequences.** No `unsafe` was needed: `fuser` 0.18 is safe to use, and
attributes come from `std` metadata. `statfs` reports zeros, which `df` shows as
an empty filesystem; real numbers need `statvfs` (a dependency or `unsafe`) and
can come with the writable mount.

## 31. Real files pass through; virtual files render at open

**Decision.** Reads of real files go straight to the backing file with
`read_at`. Past files and values inside files are rendered once at `open` and
served from that buffer, so a reader never sees a torn value even if the source
changes mid-read. History is immutable, so past nodes get a 60 s kernel cache
and `FOPEN_KEEP_CACHE`; present-day nodes get 1 s.

## 32. Milestone 3 mounts with `ro`

**Decision.** The milestone 3 mount uses the `ro` mount option, so the kernel
returns `EROFS` for every write. `default_permissions` makes the kernel check
access against the modes we report: real modes for real files, `0444`/`0555` for
everything virtual. The mount opens the store in read mode. That takes the
writer lock when it's free (one writer per store), so `fernet watch` and a
mount don't run at the same time. In v0.4 the mount itself becomes the writer.

## 33. `fernet mount` signals and `--daemon`

**Decision.** SIGINT, SIGTERM and SIGHUP unmount, via the `ctrlc` crate (with
its `termination` feature) rather than hand-written `unsafe` signal handling. If
the filesystem is unmounted from outside (`fusermount3 -u`, `umount`), the
command notices and exits. `--daemon` re-runs `fernet mount` in the foreground
as a child in its own process group, with null stdio, and returns once the mount
appears (the mountpoint's device changes). That gives a detached mount without
`fork`. A mountpoint inside the backing directory is refused.

## 34. A tree capture is one moment

**Context.** `src@first` showed only the first file `snapshot_tree` captured,
because each file got its own timestamp.

**Decision.** `snapshot_tree` stamps every record of one capture (writes and
tombstones) with the same time. `fernet init` and the watcher's first scan are
therefore single moments in history. Single-file snapshots still use the clock
at capture.

## 35. Mount tests in CI

**Decision.** The Linux test job installs `fuse3` and `ripgrep` and sets
`FERNET_REQUIRE_FUSE=1`, which turns the mount tests' "FUSE unavailable" skip
into a failure. Locally, without FUSE, they skip. The tests mount through the
real `fernet mount --daemon` binary and unmount with `fusermount3 -u`.

## 36. Type-preserving writes, per format

**Decision.** `SetScalar` strips exactly one trailing `\n`, then:
- **Strings** accept any text, re-quoted in the source's style when the text
  allows it: JSON is always double-quoted; TOML literal strings stay literal
  unless the text holds a quote or newline, multi-line strings stay multi-line,
  and everything else becomes a basic string; YAML plain stays plain only when
  the text reads back as the same string (one line, no indicators, not `true` or
  `123`), single-quoted stays single-quoted, and anything else becomes
  double-quoted with escapes for non-printable characters.
- **Other types** must parse as the same type in that format's grammar: TOML's
  integers (with `_`, `0x`, `0o`, `0b`), floats and date-times are validated by
  `toml_edit`; YAML uses the 1.2 core schema. JSON has one number type, so
  integers and floats are interchangeable there. Elsewhere `2` into a float field
  is `EINVAL`.
- Every splice is checked by re-parsing: the result must parse and the value
  at the pointer must be exactly the new text with the same type. Otherwise the
  write fails and nothing changes.

## 37. Inserts and removes

**Decision.**
- **JSON** inserts are byte splices that copy the last sibling's layout: same
  line or one per line, its indentation, its `:` spacing, and a trailing comma if
  it has one. Arrays only grow at the end (the new name must be the next index).
  New text that is a JSON number, `true`, `false` or `null` keeps that type;
  anything else is a string. Removes take one adjoining comma; removing the only
  member leaves `{}`/`[]`.
- **TOML** inserts and removes go through `toml_edit`'s format-preserving
  document, which keeps every untouched item byte-for-byte. Two small layout
  fixes keep arrays and inline tables tidy. `mkdir` in a table creates a
  `[parent.key]` table; in an inline table, `{}`.
- **YAML** is scalars-only, as the brief says. Values behind aliases or carrying
  anchors or tags, and block scalars, are read-only (`EOPNOTSUPP`).

## 38. Writes through `#`: atomic, locked, checked, captured

**Decision.** `Fernet::write_value`, `insert` and `remove` take a per-file lock,
read the file, splice it, write a temp file beside it (`.name.fernet-PID.tmp`,
with the original's permissions), fsync it, re-check the original's mtime, size
and hash, rename over it, and fsync the directory. If the file changed in
between, the edit is retried once against the new content, then fails with
`EAGAIN`. After the rename the file is snapshotted if this process holds the
store. A write succeeds without a store: history is a bonus, the file is the
truth (invariant 1). `fernet write` takes the writer lock when it's free and
otherwise leaves capture to the process that holds it.

## 39. The writable mount

**Decision.**
- From v0.4, `fernet mount` is writable by default and is the store's writer;
  `--read-only` keeps the v0.3 behaviour.
- Real files pass through: `open` (with `O_TRUNC`), `write_at`, `create`,
  `mkdir`, `unlink`, `rmdir`, `rename` (including `RENAME_NOREPLACE`), `symlink`,
  and `setattr` (size, mode, times). Hard links and changing ownership aren't
  supported.
- `#` values are edited in a per-handle buffer and applied on `flush` once
  something was written, else on `release`. Shells `dup2` a redirection and
  close the original fd before writing, so applying on the first flush would
  write an empty value. A refused write (`EINVAL` for a wrong type) is returned
  from `flush`/`fsync`/`close`. Shells ignore close errors on redirections; the
  file is unchanged either way. New keys are inserted on first flush after a
  write, with their type inferred from the text. `create` fails up front with
  `EOPNOTSUPP` when the format can't add keys (YAML).
- The mount asks the kernel for `FUSE_ATOMIC_O_TRUNC`, so `echo x > file#/key`
  is one open-write-close on an empty buffer rather than a separate truncate.
- Anything under `@`, and `.fernet` at the root, refuse changes (`EROFS`,
  `EACCES`).
- Renames rekey the inode table so open handles and kernel inodes follow the
  file.

## 40. Capture while mounted: one debouncer, trailing edge

**Decision.** Every change (a written handle released, a truncate, a rename's
source and target, an unlink, and every event from a `notify` watcher on the
backing directory) touches its path in one debouncer. A path is captured once
it has been quiet for `debounce_ms` (default 2000, in `config.toml`). Editors'
atomic saves (vim renames the original to `file~` and writes a new file;
JetBrains writes `file___jb_tmp___` and renames it over) are a burst on one path
and land as exactly one version. Temp-file patterns are in the default ignore
list, so they never appear in history. A change seen both through the mount and
by the watcher can't be counted twice: they coalesce, and identical content is
never recorded twice anyway. Unmounting captures everything still pending.

## 41. `fernet mount` mustn't hold the stderr lock

**Context.** The foreground mount held `stderr().lock()` for its lifetime;
the first log line from a filesystem thread deadlocked the mount.

**Decision.** The mount prints its banner with `eprintln!` and holds no lock.
Tracing calls in the FUSE layer are cheap and safe to keep.

## 42. Compaction

**Decision.** `Store::compact` plans with a pure function (`compact::plan`:
records + now + policy → kept positions), then rewrites: new segments
(unlisted, so ignored until committed), an atomic manifest swap (the commit
point), retired segments deleted, unreferenced blobs deleted, index rebuilt.
Buckets are UTC hours, days and weeks counted from the epoch. A `rename` is kept
if either side keeps it, which can keep slightly more history than the policy
asks for, never less. Compaction needs the writer lock. While a mount or watcher
runs, `fernet compact` reports the store as busy. A test hook (`CrashAfter(n)`)
stops after any step. The crash test runs every stopping point and checks that
the reopened store verifies and holds exactly the old history or exactly the
planned one.
