# fernet path grammar

Status: **normative** for v0.1. Implemented by `fernet-fs-path`. Any change to
this file lands in the same commit as the matching code change.

## 1. Shape

```
<real components> / <name>[@<timespec>][#] [/ <pointer token>]*
```

A path is split on `/`. Empty segments (`a//b`, a trailing `/`) are dropped, as
the kernel does. A path must be relative (no leading `/`), non-empty, and free of
NUL bytes; those are the only parse errors.

Each segment is a **component**. A component may carry:

- a **time lens** `@<timespec>`: every component below it resolves in history at
  that moment. It may sit on a file or a directory.
- an **open lens** `#` as its last character: a regular file becomes a directory,
  and every later segment is a **pointer token**.

At most one time lens and one open lens apply per path: the first of each, with
the time lens on the open component or before it. Lens-shaped text in later
segments is literal: a later name before the open lens, or a pointer token after
it.

## 2. Reading one component

Parsing a component never fails. The rules, in order:

1. One trailing `#` is stripped and marks the component open. `a##` is the name
   `a#`, opened. A `#` anywhere else is part of the name.
2. The rest is split at its **last** `@`. If the text after it is a time spec in
   **canonical form** (section 3) and the text before it is non-empty, that is the
   time lens. Otherwise the `@` is part of the name: `user@example.com` and
   `x@-02h` are plain names.
3. `.` and `..` never carry lenses, and a bare `#` is a plain name.

Printing a component is the exact inverse of reading it: for every segment `s`,
`print(read(s)) == s`. This is what lets the resolver apply "real names win"
exactly.

## 3. Time specs

Only the spellings below are specs. Keywords are lowercase. Numbers have no sign
and no leading zeros.

| Form | Grammar | Example | Meaning |
| --- | --- | --- | --- |
| relative | `-` *N* *unit*, unit in `s m h d w` | `-90s`, `-2h`, `-3d`, `-2w` | now minus the duration (`d` = 24 h, `w` = 7 d). `-0s` is now |
| named day | `yesterday`, `monday` … `sunday` | `@yesterday` | 00:00 local time at the start of that day; the most recent past one (`@monday` on a Monday is one week ago) |
| absolute | `YYYY-MM-DD` [`THH:MM`[`:SS`] [`Z` \| `±HH:MM`]] | `2026-10-01`, `2026-10-01T14:30:05Z` | local time unless an offset is given. Dates are calendar-checked; hours 00–23, minutes and seconds 00–59; offsets only with a time |
| count | `~` *N*, N ≥ 1 | `@~3` | N versions before the current one |
| first | `first` | `@first` | the earliest kept version |
| now | `now` | `@now` | the present: the latest version (read-only, like every `@` path) |
| stamp | `YYYYMMDD` `T` `HHMMSS` `Z` | `@20261030T164512Z` | a UTC second, as version listings name versions; covers the whole second, so the listed version is selected |
| mark | `[a-z][a-z0-9-]{0,63}` | `@before-migration` | a named mark |
| listing | empty | `notes.md@` | a directory listing every kept version |

Reserved words, never mark names: `yesterday`, `first`, `now`, and the seven
weekday names. All forms resolve to the latest version at or before the resulting
instant. Internally every instant is UTC nanoseconds.

## 4. Pointer tokens

Segments after the open component are pointer tokens, with RFC 6901 escapes:
`~1` is `/` and `~0` is `~`. RFC 6901 allows nothing else after `~`, so fernet
adds three whole-token forms for keys no filesystem can name:

| Token | Key |
| --- | --- |
| `~e` | the empty key `""` |
| `~.` | `.` |
| `~..` | `..` |

Listings show keys in this encoded form. Any other `~` escape is invalid, and the
token matches nothing. Keys containing NUL, and keys whose encoded form is longer
than 255 bytes, can't be named and are left out of listings.

Inside a structured file, object keys are matched exactly and array entries are
`0`, `1`, `2` … in canonical decimal (`01` matches nothing).

## 5. Real names win

The resolver checks the backing directory **before** reading lenses. If a real
entry exists with the component's exact text, it is served as-is, even if that
text contains `@` or `#`. Lenses exist only where no real entry does.

## 6. Time lenses on files and directories

- On a **file**, every form selects one kept version. `@~N` counts back from the
  newest kept version, and `@first` is the oldest.
- On a **directory**, an instant (relative, named day, absolute, mark) shows the
  directory as it was then: every file that existed at that instant, and the
  directories holding them. `@first` is the earliest instant anything below the
  directory existed. `@~N` and the listing `@` are per-file, so on a directory
  they match nothing.
- Deleted files (tombstones) don't exist after their deletion, but their
  versions stay listed under `name@`.
- Below a time lens, names are looked up as they were at that instant, and real
  names win there too.
- When a file has history but nothing at or before the instant, the error says
  so and names the first kept version.

## 7. Discovery and mutability

- Lenses never appear in `readdir` output; they exist only when named.
- `ls notes.md@` lists kept versions oldest first, named in ISO 8601 basic
  format, UTC (`20261030T164512Z`). Versions within the same second get `.1`,
  `.2`, … after the first. Then comes one entry per mark covering the file, named
  after the mark. Each entry is the file at that version; `entry#` opens it.
- `ls config.json#` lists top-level keys in source order.
- Any path with a time lens is read-only (`EROFS`). Open-lens paths are writable
  only at the present. `.fernet/` is hidden and never writable through the mount.

## 8. zsh

With `EXTENDED_GLOB`, zsh treats `#` as a glob operator. Quote fernet paths or
use `noglob`.
