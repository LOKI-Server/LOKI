# Save sync reference client

`loki_sync.py` is a small client for LOKI's [save sync API](../../docs/sync-api.md).
It uses only the Python 3.8+ standard library. It is a test tool and an
executable example of the documented client algorithm, not a polished app.

Sign in once per server, either by pairing (recommended) or with a password:

```sh
python loki_sync.py pair http://127.0.0.1:7070 --name "Test laptop"
python loki_sync.py login http://127.0.0.1:7070 --username aspen
```

`pair` prints a user code and a `/pair?code=…` link. Open it in a browser that
is signed in to LOKI and approve the device. `login` asks for the password and,
if the account uses two-factor authentication, a code. The device token is
saved in `~/.loki-sync.json` (one token per server origin); revoke it from
**Save sync** in the dashboard.

Sync a local directory with a folder, creating the folder if needed:

```sh
python loki_sync.py sync http://127.0.0.1:7070 "RetroArch saves" ./saves --kind retroarch-saves
python loki_sync.py sync http://127.0.0.1:7070 "RetroArch saves" ./saves --watch
```

Each run scans the directory, waits 3 seconds for files to settle, pulls remote
changes, then uploads and commits local ones. `--watch` repeats, long-polling
the changes feed between rounds. The local index (versions, hashes, and the
changes cursor) is `.loki-sync-index.json` inside the synced directory; it is
never uploaded. Deleting it is safe: the next run treats every local file as
new and uses `stash`, so nothing on the server is overwritten. The client does
the same by itself when the changes feed reports `reset` (the server was
restored from a backup).

Conflict policy follows the docs: a file changed locally since its last sync
wins with `overwrite`; a file this device has never synced uses `stash`; a
local delete uses `reject`. In every case the losing version stays in the
server's history, and conflicts appear in the dashboard.

To try two devices on one machine, sync two directories against the same
folder. Note that both share one device token from `~/.loki-sync.json`.
Paths the server rejects (for example names with `:`) are reported as
`skipped` and retried on the next run.
