# fernet — plan for milestones 0 and 1

Status: **awaiting approval**. Nothing outside this file has been written yet.

This plan covers M0 (scaffolding) and M1 (v0.1, read the depths). Section 5 lists
questions. Each has a recommended default, so approving the plan as-is approves
those defaults. Questions marked **blocking** change M0/M1 code. The others can be
answered later.

---

## 1. Approach in one paragraph

Invariant tests and property tests come first, then the code under them. The
grammar crate is a pure, total, bijective parser: every string parses, and
printing the result gives the same string back. Bijective printing is what lets
"real names win" work. The resolver can always recover the exact text the user
typed and check it against the backing directory before it applies a lens. The
resolver is a **per-component step function** (`lookup(parent, name)`). The kernel
already resolves paths that way, so the CLI's whole-path resolve is just a fold
over that same step. That gives invariant 7 by construction. Format drivers each
expose a span-tracking tree. JSON gets a hand-written parser. TOML and YAML each
start with a time-boxed spike to check whether an existing library gives exact
byte spans, with a recorded fallback if it doesn't.

---

## 2. Milestone 0: scaffolding

### 2.1 Files

```
Cargo.toml                 workspace; edition 2024; resolver 3; [workspace.lints]; shared deps
rust-toolchain.toml        pinned stable toolchain (see Q7)
rustfmt.toml               edition 2024, max_width 100, imports_granularity=Crate (stable opts only)
clippy.toml                allow-unwrap-in-tests, allow-expect-in-tests
deny.toml                  licenses allowlist, bans (multiple-versions = warn), advisories, sources = crates.io only
.gitignore
LICENSE-MIT, LICENSE-APACHE (see Q2)
README.md                  tagline, what it is, grammar summary, zsh EXTENDED_GLOB hazard (2.6)
PLAN.md                    this file
docs/DECISIONS.md          numbered decision log, seeded with entries listed in §4
docs/GRAMMAR.md            stub: shape, real-names-win, spec table, escapes (filled for real in M1)
docs/FORMAT.md             stub: layout and record table copied from the brief, marked "draft, unversioned"
crates/fernet-path/        lib, #![forbid(unsafe_code)], no dependencies
crates/fernet-formats/     lib, stub Driver trait
crates/fernet-store/       lib, empty (crate doc only)
crates/fernet-core/        lib, stub
crates/fernet-fuse/        lib, empty; unsafe allowed, but none written yet
crates/fernet/             bin "fernet", clap skeleton printing --version
fuzz/                      cargo-fuzz project, excluded from the workspace; target `path_parse`
tests/                     the workspace-level integration test crate (see 2.3)
.github/workflows/ci.yml
```

### 2.2 Lints (`[workspace.lints]`, inherited by every crate with `lints.workspace = true`)

- `rust`: `unsafe_code = "forbid"` (overridden to `deny` in `fernet-fuse` so a local,
  commented `#[allow]` is possible), `missing_docs = "deny"`, `unreachable_pub = "warn"`.
- `clippy`: `pedantic = { level = "deny", priority = -1 }`, plus `unwrap_used`,
  `expect_used`, `panic`, `dbg_macro`, `todo` set to `deny` (tests exempt via
  `clippy.toml`). Every local `#[allow]` carries a one-line justification comment.
- `rustdoc`: `broken_intra_doc_links = "deny"`.

### 2.3 Where `tests/` lives

Cargo has no "workspace-level tests" directory. A virtual manifest can't own tests.
I'll make `tests/` a small unpublished crate (`fernet-tests`, `publish = false`)
whose `tests/*.rs` files depend on every other crate. This keeps the layout in the
brief, and `cargo test --workspace` picks it up. Recorded as a decision.

### 2.4 CI (`.github/workflows/ci.yml`)

| Job | Runner | Steps |
| --- | --- | --- |
| fmt | ubuntu | `cargo fmt --all --check` |
| clippy | ubuntu | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| test | ubuntu | `cargo test --workspace --locked` |
| msrv | ubuntu | `cargo +<MSRV> check --workspace --locked` (see Q7) |
| deny | ubuntu | `EmbarkStudios/cargo-deny-action` |
| docs | ubuntu | `RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps` |
| fuzz-smoke | ubuntu, matrix per target | nightly + `cargo-fuzz`; `cargo fuzz run <t> -- -max_total_time=60` |
| macos | macos-latest (aarch64) | `cargo build --workspace` and `cargo test --workspace --lib --bins` |

Notes: `Cargo.lock` is committed because the workspace ships a binary. Caching uses
`Swatinem/rust-cache`. The fuzz job runs 60 s **per target** in a matrix, so adding
a driver doesn't eat the others' budget. Linux FUSE packages (`libfuse3-dev`,
`fuse3`) aren't installed until M3.

### 2.5 M0 acceptance

- CI green on the workspace above (every crate compiles, stub tests pass, one
  fuzz target runs 60 s).
- `cargo deny check` passes.
- Docs stubs and README exist. DECISIONS.md has the M0 entries.

### 2.6 M0 commits

1. `chore: initialise workspace and crate skeletons`
2. `chore: add lint, fmt and deny configuration`
3. `ci: add github actions workflow`
4. `docs: add readme, decisions, grammar and format stubs`

---

## 3. Milestone 1: read the depths

### 3.1 `fernet-path`: grammar (no dependencies, no I/O)

#### Design rules (proposed; GRAMMAR.md becomes the normative text)

- **Total.** `FernetPath::parse(&str)` fails only on structural problems: empty
  input, a NUL byte, or an absolute path. Any single segment that isn't a valid lens
  is a plain name. `user@example.com` → name `user@example.com`, no lens. This has
  to hold, because a real file with that name must stay reachable (2.2), and the
  parser can't know which names are real.
- **Canonical lenses only.** A lens is recognised only in its canonical spelling.
  `@-2h` is a lens. `@-02h`, `@-2H` and `@+2h` are not: they're plain names. That
  makes `display(parse(s)) == s` for **every** string `s`, which is stronger than the
  brief's `parse(display(p)) == p`. It's also what makes real-names-win exact: core
  re-prints a component and gets back exactly what the user typed. (A
  `Relative(Duration)` would print `@-120m` as `@-2h`. See the `TimeSpec` deviation
  below.)
- **Splitting.** A segment is split by stripping one trailing `#` (sets `open`).
  Then the remainder is split at the **last** `@`. Mark names and specs never
  contain `@`, so the last `@` is always the right one. `a@b@-2h` → name `a@b`,
  spec `-2h`. An empty name (`@-2h`, `#`) means no lens, so the segment is a plain
  name.
- **Lens placement.** At most one time lens and at most one open lens per path. The
  time lens may sit on an earlier component than the open lens
  (`src@2026-10-01/config.json#/a`), but not a later one (`a.json#/x@-1h` is
  literal: tokens after `#` are pointer tokens). The brief says "one component may
  carry lenses" in 2.1 but shows `src@2026-10-01/` in section 1. I read that as "one
  of each" (Q3).
- **Pointer tokens.** Segments after the open component are pointer tokens. They're
  kept verbatim as components (so the path stays bijective) and decoded on demand
  by `Component::pointer_token()`: `~1` → `/`, `~0` → `~`, plus the extension in Q4.
  An invalid `~` escape makes the token undecodable. Core reports that as not
  found, with a hint.
- **Empty segments.** `a//b` and a trailing `/` are collapsed, matching the kernel.
  That means the JSON empty key `""` can't be reached without an escape (Q4).

#### Types

```rust
pub struct FernetPath { pub components: Vec<Component> }

pub struct Component {
    pub name: String,
    pub when: Option<TimeSpec>,
    pub open: bool,
}

pub enum TimeSpec {
    Relative { amount: u64, unit: Unit },   // deviation: keeps the written unit (see below)
    NamedDay(DayName),                      // Yesterday | Monday..Sunday
    Absolute(AbsoluteTime),
    Back(u32),                              // ~N, N >= 1, no leading zeros
    First,
    Mark(MarkName),                         // validated newtype: [a-z][a-z0-9-]{0,63}, not reserved
    Listing,
}
pub enum Unit { Seconds, Minutes, Hours, Days, Weeks }
pub enum DayName { Yesterday, Monday, Tuesday, Wednesday, Thursday, Friday, Saturday, Sunday }

pub struct AbsoluteTime {
    pub date: Date,                 // y (4 digits), m, d; calendar-validated incl. leap years
    pub time: Option<TimeOfDay>,    // hh:mm or hh:mm:ss; second 60 rejected
    pub offset: Option<Offset>,     // Z | +hh:mm | -hh:mm
}

pub enum ParseError { Empty, Nul, Absolute, /* … */ }

impl FernetPath {
    pub fn parse(s: &str) -> Result<Self, ParseError>;
    /// Syntactic reading assuming no real names interfere: real prefix,
    /// time-lens index, open index, decoded pointer.
    pub fn lens_view(&self) -> LensView<'_>;
}
impl Component {
    pub fn parse(segment: &str) -> Self;            // total; the primitive used by core per lookup
    pub fn pointer_token(&self) -> Option<String>;  // RFC 6901 decode of the raw segment
}
impl Display for FernetPath / Component / TimeSpec   // exact inverse of parse
```

**Deviation from 4.2:** `Relative(Duration)` becomes `Relative { amount, unit }`
with a `fn duration(&self) -> Option<Duration>` (`None` on overflow). A `Duration`
can't tell `@-120m` from `@-2h`, and bijection needs
that. The same reasoning is why `AbsoluteTime` keeps the written precision. Gets its
own DECISIONS entry.

Smaller rules I'll apply unless told otherwise: relative amounts have no leading
zeros (`-0s` is allowed and means now). `~0` isn't a lens (use the bare name). All
keywords are lowercase only. A lens on `.` or `..` isn't recognised.

#### Tests written first

- Unit: one table-driven test per spec form, accepted and rejected spellings
  (including all 7 weekdays, the reserved words, mark length 64 vs 65, Feb 29 on
  leap and non-leap years, offsets, the `name@spec#` order, last-`@` splitting,
  `#` not last, empty name, `~0`/`~1` decoding, invalid escapes).
- Property (`proptest`, dev-only):
  1. `parse(display(p)) == p` for generated well-formed `FernetPath`s (the brief's
     test).
  2. `display(parse(s)) == s` for arbitrary strings (the stronger bijection).
  3. Encode/decode round-trip for pointer tokens.
- Fuzz: `path_parse` (bijection plus no panics), `path_structured` (via `arbitrary`
  on a mirror type; dev-only dependency of the fuzz crate).

### 3.2 `fernet-formats`: drivers, read-only

#### Trait and tree

As in 4.3, with two changes:

- **`ScalarType::Datetime`** is added. TOML has first-class date-times. Folding them
  into `String` would let a later type-preserving write turn `2026-10-01` into the
  string `"…"`, and `Text` belongs to Markdown bodies (Q5).
- **`Node::key_span: Option<Range<usize>>`** is added so listings and later inserts
  know where the key text lives. `span` stays "whole node incl. key".

`splice` exists on the trait but returns `DriverError::Unsupported` in M1, and
`Caps` reports read-only for all three drivers. `DriverError` carries a byte
offset plus line and column for messages
(`fernet: config.json is not valid json at 3:14 (expected ',')`).

I'll also add a shared `render(&Tree, NodeId, src) -> Vec<u8>` helper. It turns a
scalar into its file text: the string value unescaped, or the source text verbatim
for non-strings (so `0x1F`, `1e3` and `.inf` read back as written), plus `\n`. That
way core and every driver agree on rendering.

#### JSON (+ JSONC): hand-written, `crates/fernet-formats/src/json/`

- Recursive-descent with an explicit depth limit (default 512) to avoid stack
  overflow in the fuzzer and on hostile input.
- Lenient by design: `//` and `/* */` comments, and trailing commas (tsconfig needs
  both). A leading UTF-8 BOM is skipped, and spans still index the original bytes.
- Duplicate keys: listed once and resolved to the **last** occurrence, matching
  `serde_json` and most consumers (Q6).
- Integer vs float is decided by syntax (`.`/`e` → float). Arbitrarily large
  integers are kept as text.
- Detect: `.json`, `.jsonc`, `.code-workspace` (JSON5 is out of scope); sniff
  `{`/`[` after whitespace, BOM and comments.

#### TOML: spike first, `toml_edit` or our own

Spike (≤ 1 day, kept as a test file either way): parse the golden corpus with
`toml_edit` 0.25 (`ImDocument`, before `despan`) and assert every key and value
span against hand-checked offsets. I'll cover dotted keys, inline tables, arrays of
tables, and a table split across non-adjacent headers.

- If the spans are exact, I use it. Keys, values and table headers give
  `key_span`/`value_span`.
- Known structural issue either way: a TOML table can be **non-contiguous**
  (`[a]` … `[c]` … `[a.b]`), so "whole node incl. key" isn't a single range. In M1,
  `span` for a table node is its header line. Removal is M4's problem, and I'll
  propose a `Vec<Range>` then. Recorded now so it doesn't surprise anyone.
- If spans are wrong or missing, I write our own TOML 1.0 reader in M1 and record
  that.

#### YAML: spike first, `saphyr-parser`

Spike: `saphyr-parser` 0.3 is event-based with `Marker`/`Span`. Things to verify:

1. Whether scalar events carry an **end** marker, not just a start.
2. Whether `Marker::index` is a char offset (I believe it is), which needs a
   char→byte table.
3. Block scalars, flow collections and multi-line plain scalars.

If the spans hold, I build the tree from events. Otherwise I evaluate
`yaml-rust2`, then report back before writing a YAML parser myself (that's a big
job and deserves your say).

Read semantics proposed for v0.1:

- Aliases resolve to the anchored node (read-only).
- `<<` merge keys are shown as a literal `<<` key, with no merging (Q6).
- Non-string scalar keys use their source text.
- Complex (mapping/sequence) keys are omitted from listings, and that's logged.
- Multi-document streams: see Q6.

#### Golden and span tests (written before the drivers)

- `crates/fernet-formats/tests/golden/` corpus, authored by me in the shape of real
  files: `package.json`, `Cargo.toml` (workspace and package), `docker-compose.yml`
  (anchors and merge keys), a GitHub workflow YAML (block scalars, `on:` key), a
  `tsconfig.json` with comments and trailing commas, plus a few nasties (unicode
  escapes, surrogate pairs, CRLF line endings, a BOM, deep nesting). Any file copied
  from a real project is MIT/Apache only, with attribution in `golden/SOURCES.md`.
- **Leaf test:** for every leaf reachable via `#`, the rendered value equals what
  the reference parser reports at the same pointer. Comparison is semantic: strings
  exact, numbers by parsed value, bools and nulls exact. Reference parsers
  (dev-dependencies only): `serde_json` (with `arbitrary_precision`), `toml`, and
  `serde_norway`. That's a maintained `serde_yaml`-compatible fork.
  `serde_yaml` itself is deprecated, and `saphyr` would be the same lineage as our
  parser, so it can't serve as an independent reference. For JSONC, the reference
  is `serde_json` over a copy with comments and trailing commas removed by a small
  stripper in test code.
- **Span test:** for every scalar, `unescape(src[value_span])` equals the reported
  value, and for every keyed node `src[key_span]` unescapes to the key.
- **Coverage test:** reference-parser leaf count == our leaf count, so nothing gets
  silently dropped.
- Fuzz: `json_parse`, `toml_parse`, `yaml_parse`. Each one checks for no panics,
  checks that all spans are in bounds and nested properly, and differential-checks
  against the reference parser whenever the reference accepts the input.

### 3.3 `fernet-core`: resolution without history

```rust
pub struct Fernet { root: PathBuf, store: Option<Store>, drivers: Registry }

impl Fernet {
    /// Opens `root`. A missing `.fernet/` is fine in M1 (no history).
    pub fn open(root: &Path) -> Result<Self>;
    /// The step function; the mount (M3) calls exactly this per kernel lookup.
    pub fn lookup(&self, parent: &Resolved, name: &OsStr) -> Result<Resolved>;
    /// Fold of `lookup` over the path; what the CLI uses.
    pub fn resolve(&self, path: &FernetPath) -> Result<Resolved>;
    pub fn read(&self, r: &Resolved) -> Result<Vec<u8>>;
    pub fn list(&self, r: &Resolved) -> Result<Vec<Entry>>;
}

pub struct Entry { pub name: OsString, pub kind: EntryKind, pub size: u64 }
pub enum EntryKind { File, Dir, Symlink, Other }
pub enum Source { Present(PathBuf) /* Blob(Hash) arrives in M2 */ }
```

`Resolved` is as in 4.5, with the variants M1 can produce: `RealFile`, `RealDir`,
`Inside`, `Missing`. The `@` variants exist but resolving one returns
`Error::NoHistory` ("no store here; try: fernet init"). Errors are a `thiserror`
enum, and each variant maps to an errno (for M3) and a CLI exit code.

- **Real names win:** `lookup` first checks `symlink_metadata(parent/name)`.
  It parses lenses only on a miss. Because `name` is an `OsStr`, non-UTF-8 real
  names keep working. Lenses need UTF-8.
- **Inside walk:** keys are matched as decoded tokens. Array entries are canonical
  decimal indices only (`01` misses). `list` on an `Inside` object or array returns
  **encoded** tokens (`a~1b`), sized by rendered text, in source order.
- **Containment:** `..` can't climb out of `root`. Symlinks are followed for reading
  (like `cat`) and reported as `Symlink` by `list`.
- **Detection:** extension first, then a 4 KiB sniff. Unknown or binary → `Missing`
  (ENOENT) per Q8.
- Tree cache: deferred to M3. The CLI parses once per invocation. The mount is where
  repeated lookups make a cache pay for itself.

Tests first:

- `tests/invariant_3_bare_names.rs`: proptest generates random directory trees
  (names with `@`, `#`, spaces, unicode, non-UTF-8 bytes, symlinks, empty files,
  directories) in a tempdir. `Fernet::list` must equal `std::fs::read_dir` (names,
  types and sizes), minus `.fernet/`.
- `tests/real_names_win.rs`: real `a@-2h`, `b#` and `user@example.com` entries are
  served as-is. When they're absent, the lens applies.
- `tests/inside_paths.rs`: the golden corpus read through `Fernet::resolve` and
  `read` matches the driver-level leaf test.

### 3.4 `fernet` CLI

- `fernet cat <path>` and `fernet ls <path>`, plus `--root <dir>` (default: nearest
  ancestor containing `.fernet/`, else the current directory; Q9).
- `ls` output: one name per line on a pipe. On a TTY, directories get a trailing `/`
  and are dimmed. Colour is only used on a TTY and respects `NO_COLOR`. Sorting: real
  directories sort by byte order of the name (like `ls`), keys keep source order.
- Errors follow the house style. Exit codes come from 4.7 via a single
  `fn exit_code(&Error) -> u8`.
- `anyhow` only here. Logging uses `tracing` with `FERNET_LOG`, off by default.
- Tests: `assert_cmd` + `tempfile` (dev-dependencies; DECISIONS entry), covering
  `fernet ls` == `read_dir` on the invariant-3 fixture, `cat` of every golden leaf,
  and exit codes for not-found and usage errors.

### 3.5 M1 acceptance (mapped to the brief)

| Criterion | Where |
| --- | --- |
| `parse(display(p)) == p` | `fernet-path` proptest 1 (plus the stronger bijection, proptest 2) |
| Fuzz clean for 10 min locally | `path_parse`, `path_structured`, `json_parse`, `toml_parse`, `yaml_parse`; I'll run each for 10 min in this container and paste the stats into the M1 report |
| Golden leaves equal reference parsers | `fernet-formats/tests/golden.rs` + `tests/inside_paths.rs` |
| `value_span` slice unescapes to value | `fernet-formats/tests/spans.rs` |
| `fernet ls` == `read_dir` | `tests/invariant_3_bare_names.rs` + CLI test |

### 3.6 M1 commit sketch

`test(path): …` → `feat(path): …` → `docs(grammar): …` → `test(formats): golden corpus` →
`feat(formats): json driver` → `spike(formats): toml_edit spans` → `feat(formats): toml driver` →
`spike(formats): saphyr spans` → `feat(formats): yaml driver` → `test(core): invariant 3` →
`feat(core): resolver` → `feat(cli): cat and ls` → `test(fuzz): …`

---

## 4. DECISIONS.md entries I expect to write in M0–M1

1. Crate names on crates.io (Q1).
2. `tests/` as an unpublished `fernet-tests` crate.
3. Grammar is total and bijective; canonical-only lenses; split at last `@`.
4. `TimeSpec::Relative { amount, unit }` instead of `Duration`.
5. Resolver is a per-component step function; `resolve` is a fold (invariant 7).
6. `ScalarType::Datetime` and `Node::key_span` added.
7. JSON driver accepts JSONC and trailing commas everywhere.
8. TOML parser: outcome of the `toml_edit` spike.
9. YAML parser: outcome of the `saphyr-parser` spike.
10. `serde_norway` as the YAML reference parser (dev-only).
11. Extra dev-dependencies: `assert_cmd`, `tempfile`, `arbitrary`, `libfuzzer-sys`.
12. Token escape extension for unrepresentable keys (Q4).
13. Toolchain pin vs MSRV (Q7).

---

## 5. Questions

**Q1. Crate names (blocking for M0, brief §8.1).** `fernet` on crates.io is taken
(the Fernet token crate). `fernet-path`, `fernet-core`, `fernet-store`,
`fernet-formats` and `fernet-fuse` are all free today, but names like
`fernet-core` read as part of the token crate's family.
*Recommendation:* publish every package under a `fernet-fs-` prefix (`fernet-fs`
for the binary package with `[[bin]] name = "fernet"`, `fernet-fs-path`,
`fernet-fs-formats`, …). Directory names stay as in the brief (`crates/fernet-path`).
Rust import paths become `fernet_fs_path`. Alternative: rename only the binary
package to `fernet-fs` and keep the other names.

**Q2. License (blocking for M0: `deny.toml` and headers).** The brief doesn't say.
*Recommendation:* `MIT OR Apache-2.0`, the Rust ecosystem default.

**Q3. Lens placement (blocking for M1 grammar).** Brief 2.1 says "one component may
carry lenses", but section 1 and 2.3 show `src@2026-10-01/` with a file below it, and
so presumably `src@2026-10-01/config.json#/a`. *Recommendation:* at most one `@` and
one `#` per path, with the `@` at or before the `#` component.

**Q4. Keys a filesystem can't name (blocking for M1 listings).** The kernel never
delivers `""`, `.` or `..` as names, and names over 255 bytes fail with
`ENAMETOOLONG`. JSON allows all of these as keys. *Recommendation:* a small,
RFC-compatible extension. In RFC 6901 `~` may only be followed by `0` or `1`, so
anything else is free to define. `~e` means the empty key, `~.` means `.`, and `~..`
means `..`. Listings emit these forms. Keys over 255 bytes are omitted from listings
and stay unreachable (documented). Alternative: omit all of them.

**Q5. TOML datetime (blocking for M1 types).** Add `ScalarType::Datetime` as above?
*Recommendation:* yes.

**Q6. Duplicate keys and YAML specifics (non-blocking; defaults above).**

- Duplicate JSON/YAML keys: last wins, listed once.
- YAML `<<`: literal key, no merge.
- Multi-document YAML: I propose the root is always the first document, with
  later documents unreachable in v0.1. A `---`-separated stream switching between a
  plain root and a `0/1/…` directory when a second document appears would make paths
  unstable. Revisit when someone needs it.

**Q7. Toolchain vs MSRV (non-blocking).** "MSRV pinned in `rust-toolchain.toml`"
would mean developing on the oldest compiler. *Recommendation:*
`rust-toolchain.toml` pins a recent stable (1.97.0, for reproducible clippy).
`rust-version` in `Cargo.toml` declares the MSRV, initially 1.85 (the edition 2024
floor), and CI checks it. Later dependencies (`redb` 4, `jiff`, `fuser`) may bump it,
and each bump gets a DECISIONS entry.

**Q8. `#` on unknown or binary formats (brief §8.5).** *Recommendation:* `ENOENT`
for v0.1. It's honest and cheap, and a raw byte-range view can come later as an
explicit driver (`#` on `.bin` → `0-4095`, …) without breaking anything.

**Q9. CLI root without a store.** With no `.fernet/` above the current directory,
the root is the current directory, so `fernet cat ../x.json#/a` is rejected
("outside the root; try: fernet --root .. cat …"). Alternative: treat each argument's
real prefix as its own root. *Recommendation:* the former. It's the same rule the
mount and the store will use later.

**Q10. Not blocking M0/M1, raising early.**

- `@now` isn't in the spec table but `now` is reserved, and `fernet core` prints
  `now`. Should `@now` be an explicit (read-only) present?
- Version-listing names have one-second resolution (`20261030T164512Z`), but saves
  can land closer together than that (debounce is per file, 2 s, but external
  writes aren't debounced). Proposal for M2: append `.N` on collision, and accept the
  basic-format names as an absolute spec too, so `notes.md@20261030T164512Z` works.
- Is there a whole-store time lens (`.@yesterday/`)? 2.1 needs a component to hang
  `@` on, so the root can't take a lens through the mount.
- Brief §8 Q2–Q4 (FSKit, `x-fernet-type`, time ranges) don't affect M0/M1. I'll
  bring them back at M3/M4.

---

## 6. Risks

- **YAML spans** are the biggest unknown. If no maintained parser gives exact byte
  spans, a YAML reader is weeks of work. The spike decides this early, and I'll
  report before committing to a hand-written one.
- **Fuzz on CI** needs nightly. It's isolated in its own job so a nightly breakage
  can't block stable jobs. I'd propose that job is required but auto-retried once.
- **`clippy::pedantic` as errors** on a moving toolchain: the pin in
  `rust-toolchain.toml` keeps new lints from breaking CI unannounced. Bumps are
  deliberate commits.
