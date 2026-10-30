# History

## Capture

History is captured automatically, with no commits:

- `fernet init` captures every file once.
- `fernet watch` and `fernet mount` capture each change, debounced: a path is
  captured once it has been quiet for 2 s (`debounce_ms` in
  `.fernet/config.toml`). An editor's atomic save is one version.
- `fernet write` captures what it writes.

Identical content is never stored twice. `.fernetignore` (gitignore syntax)
and the defaults (`.git/`, `node_modules/`, `target/`, `.DS_Store`, editor temp
files, files over 50 MiB) are never captured.

## Marks

```sh
fernet mark before-migration        # a tephra layer across the whole store
fernet cat config.json@before-migration
fernet mark --rm before-migration
```

## Compaction

`fernet compact` thins old versions: everything younger than a day, the latest
per hour for a month, the latest per day for a year, then the latest per week,
forever. The newest version of every file and every marked version are always
kept. `--dry-run` shows what would go. The layers are configurable under
`[retention]`.

## Safety

- Deleting `.fernet/` never loses current data: your files are the truth.
- History is append-only; only compaction removes versions.
- `fernet verify` checks every record and blob; `fernet reindex` rebuilds the
  disposable index from the log.
- The on-disk format is a written, frozen spec ([FORMAT.md](../FORMAT.md)).
