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

> Status: 1.0. History is captured with `fernet init`, `fernet watch` or a
> mount. Values inside JSON, TOML, YAML, Markdown and CSV files are readable
> and writable through `#`; SQLite databases, zip and tar archives are
> readable.

```
$ echo 9090 > 'mnt/config.json#/server/port'      # a byte splice: nothing else moves
$ mkdir 'mnt/config.json#/limits'                   # inserts {}
$ cat 'mnt/config.json@~1#/server/port'
8080
```

```
$ fernet cat package.json#/scripts/build
tsc -p tsconfig.json && vite build
$ fernet ls docker-compose.yml#/services
api
db
cache
```

## Install

Prebuilt binaries for Linux (x86_64, aarch64) and macOS (Apple silicon) are on
the [releases page](https://github.com/oddurs/fernet/releases), with shell
completions in each archive. Or build from source with Rust 1.90 or later:

```
cargo install --locked --git https://github.com/oddurs/fernet fernet-fs
```

The mount needs FUSE: `fuse3` on Linux, [macFUSE](docs/MACOS.md) on macOS.
Changes are listed in [CHANGELOG.md](CHANGELOG.md).

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

The full grammar is in [docs/GRAMMAR.md](docs/GRAMMAR.md). The user guide starts at
[docs/guide/index.md](docs/guide/index.md); the on-disk format is the frozen
spec in [docs/FORMAT.md](docs/FORMAT.md).

## Commands

```
fernet init [dir]                    create .fernet/ and capture the first snowfall
fernet watch [dir]                   capture changes until interrupted
fernet mount <dir> <mountpoint>      mount (writable); --read-only, --daemon
fernet write <path> [value|-]        set a value (stdin with -)
fernet cat <path>                    print a file, past file or value
fernet ls <path>                     list a dir, version listing or keys
fernet core <path>                   core sample of a file or value
fernet log [path]                    versions, newest first; --size for store usage
fernet mark <name> | --rm <name>     add or remove a tephra mark
fernet diff <a> <b>                  unified diff of any two fernet paths
fernet compact [--dry-run]           apply the retention policy
fernet verify                        check log, blobs, manifest
fernet completions <shell>           print shell completions
fernet reindex                       rebuild the index from the log
```

On macOS, see [docs/MACOS.md](docs/MACOS.md) for the mount.

## Formats

| Format | `#` shows | Writes |
| --- | --- | --- |
| JSON (with comments) | objects and arrays | values, new keys, removals |
| TOML | tables and arrays | values, new keys, removals |
| YAML | mappings and sequences | values (not anchors, aliases or block scalars) |
| Markdown | headings as directories, each with a `body` file | bodies, new sections, removals |
| CSV | rows `0`, `1`, … of fields by header | values, new rows, removals |
| SQLite | tables → rows by primary key → columns | read-only |
| zip, tar | the archive's tree | read-only |

## Shell completion

```sh
eval "$(fernet completions bash)"    # or zsh; fish: fernet completions fish > ~/.config/fish/completions/fernet.fish
```

Keys complete after `#/`, and versions, marks and time words after `@`.

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
