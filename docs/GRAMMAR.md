# fernet path grammar

Status: **draft** (milestone 0 stub). Milestone 1 makes this normative.

## Shape

```
<real components> / <name>[@<timespec>][#] [/ <pointer token>]*
```

- `@<timespec>` is the time lens. It may sit on a file or a directory component;
  every component below it resolves in history at that moment.
- A trailing `#` is the open lens. It turns a regular file into a directory. Every
  component after it is a pointer token (RFC 6901: `~1` is `/`, `~0` is `~`).
- Order on one component is always `name@spec#`.

## Real names win

If a real entry with the exact component text exists, it is served as-is, even if
the text contains `@` or `#`. Lenses exist only where no real entry does.

## Time specs

| Form | Example |
| --- | --- |
| relative | `@-90s`, `@-2h`, `@-3d`, `@-2w` |
| named day | `@yesterday`, `@monday` … `@sunday` |
| absolute | `@2026-10-01`, `@2026-10-01T14:30`, `@2026-10-01T14:30:05Z` |
| count | `@~1` |
| first | `@first` |
| mark | `@before-migration` |
| listing | `@` |
