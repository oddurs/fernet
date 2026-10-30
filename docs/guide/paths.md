# Paths: `#` and `@`

A fernet path is an ordinary path in which one component may carry lenses:
`name@spec#`. The full grammar is in [GRAMMAR.md](../GRAMMAR.md).

## `#`: into a file

- `config.json#` is a directory of top-level keys; arrays list `0`, `1`, ….
- Values are files holding the value's text plus a newline. Strings are shown
  unquoted.
- Keys with `/` or `~` use JSON Pointer escapes: `a~1b` is `a/b`, `m~0n` is
  `m~n`. Keys `""`, `.` and `..` are `~e`, `~.` and `~..`.
- Writing keeps the type: a number stays a number (`EINVAL` otherwise), a
  string takes anything. `mkdir` adds an empty object, `rm` removes a key, and
  writing a new name adds it.

## `@`: back in time

| You write | You get |
| --- | --- |
| `notes.md@-90s`, `@-2h`, `@-3d`, `@-2w` | that long ago |
| `@yesterday`, `@monday` | local midnight starting that day (the last one) |
| `@2026-10-01`, `@2026-10-01T14:30`, `@…Z` | that date and time (local unless an offset is given) |
| `@~1`, `@~3` | one or three versions back |
| `@first` | the earliest kept version |
| `@before-migration` | a mark |
| `notes.md@` | a directory of every kept version |

Every form picks the latest version at or before the instant. On a directory,
`src@2026-10-01/` is the whole folder as it was then. Anything under `@` is
read-only.

## Real names win

If a file is literally called `user@example.com` or `odd#`, that's what you
get. Lenses only exist where no real file does, and they never show up in
`ls`, `find` or `rg`.
