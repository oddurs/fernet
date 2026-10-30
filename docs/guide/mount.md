# The mount

```sh
fernet mount <dir> <mountpoint>            # foreground; ctrl-c unmounts
fernet mount <dir> <mountpoint> --daemon   # background; umount / fusermount3 -u
fernet mount <dir> <mountpoint> --read-only
```

- Real files pass straight through. On Linux 6.9+ with the privileges for it,
  reads and writes use kernel passthrough and run at native speed. Otherwise
  the kernel keeps their page cache and drops it when a file changes.
- `cat`, `cp`, `diff`, `grep`, editors: everything works on `#` and `@` paths
  too. `echo 9090 > 'mnt/config.json#/server/port'` edits one value.
- A refused value write (wrong type) fails at `close()`. Shells ignore close
  errors on redirections, so check with `fernet cat` or use `fernet write`.
- The mount holds the store's writer lock, so `fernet watch` doesn't run next
  to it. Readers (`fernet cat x@-1h`) always work.
