# Getting started

## Install

Download a release binary for Linux (x86_64, aarch64) or macOS (aarch64), or
build from source:

```sh
cargo install --path crates/fernet
```

The mount needs FUSE: `fuse3` on Linux; FUSE-T or macFUSE on macOS (see
[MACOS.md](../MACOS.md)). Everything else works without it.

## Read inside files, no setup

`#` needs nothing but a file fernet understands:

```sh
$ fernet cat package.json#/scripts/build
tsc -p tsconfig.json && vite build
$ fernet ls docker-compose.yml#/services
api
db
$ fernet write config.json#/server/port 9090
```

Writes are byte splices: only the value's bytes change, comments and
formatting stay exactly as they were.

## Keep history

```sh
$ fernet init                 # creates .fernet/ and captures every file
$ fernet watch                # captures changes until ctrl-c
```

or mount it, which captures every save made through the mount (and outside it):

```sh
$ fernet mount . ~/mnt/project
```

Then go back in time:

```sh
$ fernet cat notes.md@yesterday
$ fernet ls notes.md@                       # every kept version
$ fernet diff config.json@first config.json
$ fernet core infra.yaml#/services/api/image
now         api:2.4.1
-3h         api:2.4.0
──────────── tephra · before-migration ────────────
oct 28      api:2.3.9
```

## Shell completion

```sh
eval "$(fernet completions bash)"   # or zsh; fish writes a file
```

In zsh with `EXTENDED_GLOB`, quote `#` paths or `alias fernet='noglob fernet'`.
