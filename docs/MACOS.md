# fernet on macOS

The CLI works on macOS with no extra software. The mount needs a FUSE
implementation. fernet targets **FUSE-T** (kext-free, built on the NFS client)
first, and **macFUSE** second.

> Status: CI builds and unit-tests fernet on macOS (aarch64). The mount steps
> below follow `fuser`'s documented macOS support. They have **not yet been
> verified by hand** on a Mac; see "Verification checklist".

## Install FUSE-T

```sh
brew install --cask macos-fuse-t/homebrew-cask/fuse-t
```

FUSE-T installs `libfuse` compatibility shims and the `mount_fusefs`-style
helper that `fuser` calls. No kernel extension and no reboot are needed.

macFUSE works too (`brew install --cask macfuse`), but it needs its kernel
extension approved in System Settings → Privacy & Security, and a reboot.

## Build and mount

```sh
cargo install --path crates/fernet       # or use a release binary
mkdir -p ~/mnt/project
fernet mount ~/code/project ~/mnt/project
```

`fernet mount` runs in the foreground. Ctrl-C (SIGINT) or SIGTERM unmounts. To
run it in the background, add `--daemon`, and unmount with:

```sh
umount ~/mnt/project
```

## zsh

macOS's default shell is zsh. With `setopt EXTENDED_GLOB`, quote `#` paths:

```sh
cat '~/mnt/project/package.json#/version'
noglob cat ~/mnt/project/package.json#/version
```

## Verification checklist

Run these on a Mac with FUSE-T and record the macOS version, FUSE-T version and
results in this file:

1. `fernet init` in a scratch project, edit a file, then `fernet watch` for a few
   seconds so there is some history.
2. `fernet mount <dir> <mnt>` succeeds, and `mount | grep fernet` shows it.
3. `ls <mnt>` matches `ls <dir>` (minus `.fernet`).
4. `cat '<mnt>/file#/key'`, `cat '<mnt>/file@first'` and `ls '<mnt>/file@'` all
   work.
5. Finder shows only real entries, with no lens names.
6. `echo x > <mnt>/new` fails with "Read-only file system".
7. Ctrl-C unmounts cleanly; `--daemon` plus `umount` does too.

## Known differences

- FUSE-T is NFS-based, so attribute caching follows the NFS client's rules.
  Present-day values can lag their source file by up to a second or two.
- Spotlight and Finder may create `._*` and `.DS_Store` files. The mount is
  read-only until v0.4, so they're refused. After v0.4, `.DS_Store` is in the
  default ignore list.
