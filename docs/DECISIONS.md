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
