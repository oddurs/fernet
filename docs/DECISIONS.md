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
