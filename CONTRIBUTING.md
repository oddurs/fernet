# Contributing

## Workflow

1. Branch from `main`: `feat/…`, `fix/…`, `docs/…`, `chore/…`, `test/…`.
2. Make small, focused commits with conventional messages
   (`feat(path): parse relative time specs`, `fix(fuse): …`).
3. Push and open a pull request against `main`. The template asks which
   invariants the change touches.
4. CI must be green: fmt, clippy (pedantic, warnings are errors), tests on
   Linux (including the FUSE mount tests) and macOS, docs, MSRV, `cargo deny`,
   and a fuzz smoke run per target.
5. Squash-merge, then delete the branch.

`main` is always releasable. To release, open a PR that bumps the workspace
version and adds a `CHANGELOG.md` section; after it merges, tag the merge
commit (`v1.2.3`) and push the tag. The release workflow builds and publishes
the binaries.

## Rules that don't bend

- The seven invariants in the README: a change that weakens one needs
  explicit agreement first, and every invariant has tests that must keep
  passing.
- Invariant tests come before the code they protect.
- `docs/GRAMMAR.md` and `docs/FORMAT.md` change in the same commit as the
  grammar or format. FORMAT.md is frozen at version 1: incompatible changes
  need a new `FORMAT` line and must keep reading version 1.
- Every non-obvious decision gets a numbered entry in `docs/DECISIONS.md`.
- No `unsafe` outside `fernet-fs-fuse`, and there only with a `// SAFETY:`
  comment and a decision entry. No `unwrap`/`expect` outside tests.

## Running things

```sh
cargo test --workspace                                   # everything (FUSE tests skip without /dev/fuse)
cargo test --release -p fernet-fs --test bench -- --ignored --nocapture
cd fuzz && cargo +nightly fuzz run json_parse            # any target in fuzz/Cargo.toml
```
