# fernet

*every file has depth, every moment is kept.*

fernet wraps a directory you already have. Your files stay plain files in a
plain folder. fernet adds two characters to their names:

- `#` goes **into** a file: `config.json#/server/port` is a file holding `8080`.
- `@` goes **back** in time: `notes.md@-2h` is the file two hours ago.

Both work from the command line and through a FUSE mount, with exactly the same
results (the mount and the CLI share one resolver).

- [Getting started](getting-started.md)
- [Paths: `#` and `@`](paths.md)
- [History: capture, marks, compaction](history.md)
- [The mount](mount.md)
- [Formats](formats.md)
- Reference: [GRAMMAR.md](../GRAMMAR.md) · [FORMAT.md](../FORMAT.md) ·
  [DECISIONS.md](../DECISIONS.md) · [MACOS.md](../MACOS.md)
