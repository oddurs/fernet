# Changelog

All notable changes to fernet. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/). The on-disk store format has its
own version, specified in [docs/FORMAT.md](docs/FORMAT.md).

## [1.0.0] - 2026-10-31

First stable release. Store format v1 is frozen.

### Paths
- `#` goes into a file, `@` goes back in time. The grammar is total and
  bijective ([docs/GRAMMAR.md](docs/GRAMMAR.md)); real names always win.
- Time specs: relative (`@2h`, `@3d`), named days (`@yesterday`,
  `@monday`), absolute dates and times, `@~N` (N versions back), `@first`,
  `@now`, version stamps, tephra marks, and `@` version listings.

### History
- An append-only, checksummed store under `.fernet/`: content-addressed
  BLAKE3 + zstd blobs, a crash-safe manifest, and torn-tail recovery.
- Capture with `fernet init`, `fernet watch` or a mount, with debouncing and
  editor-temp-file ignores. Deletions, moves and replacements are recorded.
- `fernet log`, `diff`, `core`, `mark`, `verify`, `reindex`, and
  mark-aware, crash-safe `compact`.
- An independent reader in `tools/format-reader` and a committed fixture
  store hold the format to its spec.

### Formats
- Reads and type-preserving writes for JSON (with comments), TOML, YAML,
  Markdown and CSV. Writes are byte splices: nothing else in the file moves.
- Read-only SQLite, zip, tar and `.tar.gz`.

### Mount
- A FUSE mount (Linux; macOS via macFUSE) that exposes `#` and `@` paths
  alongside the real tree. It is writable by default (`--read-only` to
  turn that off).
- Mounted reads run at native speed via FUSE passthrough on Linux 6.9+, with
  a kernel page-cache fallback on older kernels.

### CLI
- `fernet cat`, `ls`, `write` (binary stdin for files), and shell
  completions for bash, zsh and fish that complete `#` keys and `@` versions.

[1.0.0]: https://github.com/oddurs/fernet/releases/tag/v1.0.0
