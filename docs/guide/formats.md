# Formats

| Format | `#` shows | Writes |
| --- | --- | --- |
| JSON (with comments, trailing commas) | objects and arrays | values, new keys, removals |
| TOML | tables and arrays | values, new keys, removals |
| YAML | mappings and sequences (first document) | values; anchors, aliases, block scalars are read-only |
| Markdown | headings as directories, each with a `body` file | bodies, new sections, removals |
| CSV | rows `0`, `1`, … of fields by header | values, new rows, removals |
| SQLite | tables → rows by primary key → columns | read-only |
| zip, tar | the archive's tree | read-only |

Files are recognised by extension, then by content for files with an
unknown extension. `#` on anything else matches nothing.
