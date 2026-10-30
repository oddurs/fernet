# fernet

**every file has depth, every moment is kept.**

fernet is a filesystem with two extra characters: `#` goes **into** a file, and
`@` goes **back** in time. Everything else is an ordinary folder of ordinary files.

```
$ cat infra.yaml@yesterday#/services/api/image
api:2.3.9
```

fernet wraps an existing directory and exposes the same tree through a FUSE mount
and a CLI. Your files stay plain files in a plain folder; fernet only adds names
that don't exist there.

> Status: early development (milestone 0). Nothing here works yet.

## The grammar

A fernet path is a normal relative path where components may carry lenses:

```
<real components> / <name>[@<timespec>][#] [/ <pointer token>]*
```

| You write | You get |
| --- | --- |
| `config.json#` | a directory of the top-level keys |
| `config.json#/server/port` | a file holding one value; readable, and writable at the present |
| `notes.md@-2h` | the file as it was two hours ago |
| `src@2026-10-01/` | the whole folder as of that date |
| `notes.md@` | a directory of every kept version |

Time specs: `@-90s` `@-2h` `@-3d` `@-2w` · `@yesterday` `@monday` · `@2026-10-01`
`@2026-10-01T14:30` `@2026-10-01T14:30:05Z` · `@~1` · `@first` · `@<mark>`.
Pointer tokens follow RFC 6901: `~1` is `/` and `~0` is `~`.

Real names always win: if a file is literally called `user@example.com`, that's
what you get. Lenses never show up in `ls`, `find` or `rg`; they exist only where
you name them. Anything under `@` is read-only.

The full grammar is in [docs/GRAMMAR.md](docs/GRAMMAR.md).

## zsh and `#`

With `setopt EXTENDED_GLOB`, zsh treats `#` as a glob operator. Quote fernet paths
or disable globbing for the command:

```zsh
cat 'config.json#/server/port'
noglob fernet cat config.json#/server/port
alias fernet='noglob fernet'
```

## Why "fernet"

It nods to *firn*, the glaciologist's word for old snow compacting into glacier
ice. An ice core is a readable record of time. A captured version is a
**snowfall**, thinning old versions is **compaction**, a named moment is a
**tephra** mark, and the history of one value is a **core sample**.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
