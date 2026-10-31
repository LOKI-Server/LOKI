# LOKI save sync API 1

LOKI includes a per-user file sync service. Its headline use is emulator saves:
play on an Android handheld, keep playing on the desktop, and never lose a save.
The server is authoritative (hub and spoke, not peer to peer). Clients talk to it
over plain HTTP/JSON under `/sync/v1`, so they can be written in any language
that has HTTPS, JSON, and SHA-256. A machine-readable description is in
[sync-api.yaml](sync-api.yaml) (OpenAPI 3.1). A small standard-library Python
client is in [examples/sync-reference-client](../examples/sync-reference-client/README.md).

Sync is the LOKI resource `sync`. Like every new resource it is restricted: only
the owner can use it until the owner grants access to people or groups.

## Model

- A user owns **folders**. Each folder has an `id`, a `name` unique per user, a
  `kind` hint, a `keep_versions` history limit, a `seq`, and a `created` time.
- A folder holds **files** keyed by relative `path`. Each file has a `version`
  (per path, starting at 1), a `hash` (lowercase hex SHA-256 of the contents),
  `size` in bytes, `mtime`, the `device` id that wrote it, and the folder `seq`
  at which it last changed.
- File contents are **blobs**, stored once per hash. A client uploads a blob,
  then **commits** a change that points a path at it.
- `seq` increases by one for every new version in the folder. It is the cursor
  for the changes feed. Never compare `seq` between folders.
- A delete is a **tombstone**: a new version with `hash: null` and `size: 0`.
  Tombstones appear in the changes feed so other devices delete too.
- Replaced, deleted, and conflicting versions move to **history**. The newest
  `keep_versions` resolved entries are kept per path; unresolved conflicts are
  always kept. History can be restored from the API or the dashboard.

`mtime` is an integer the server stores and returns but never interprets or
uses for ordering. This guide and the reference client use the file's
modification time in Unix **milliseconds**. Clocks on handhelds are often
wrong; ordering comes only from `version` and `seq`. Times the server itself
sets (`created`) are Unix seconds.

Clients must ignore unknown JSON fields. New optional fields can appear within
protocol 1.

## Discovery

`GET /sync/v1` needs no authentication:

```json
{"protocol":1,"server":"0.1.0","name":"LOKI","hash":"sha256","max_file_bytes":2147483648,"pairing":true}
```

Refuse to continue if `protocol` is not a version you implement or `hash` is
not `sha256`. `server` is the LOKI version and `name` the installation's display
name. Use the server's canonical public origin (for example
`https://loki.example.com`) as the base URL. Use HTTPS for anything beyond
loopback.

## Authentication

Every other endpoint except pairing and login needs a device token:

```text
Authorization: Bearer <token>
```

The API never accepts LOKI's browser session cookie; a request without a bearer
token returns `401`. Each token belongs to one named device, which the user can
see, rename, and revoke in the dashboard under **Save sync**. Store the token
like a password (Android Keystore, Windows Credential Manager, a `0600` file).

A token stops working when the device is revoked or signs out, the user changes
their password, the owner revokes the user's sessions, or the account is no
longer approved. Then every request returns `401` with code `unauthorized`:
discard the token and pair again. When the token is valid but the user has lost
sync access, requests return `403` with code `forbidden`; show that and retry
later without re-pairing.

### Device pairing (recommended)

Pairing works like TV sign-in: the device never sees the password.

1. `POST /sync/v1/pair`:

   ```json
   {"name":"AYN Thor","platform":"android"}
   ```

   `name` is 1–80 characters. `platform` is free text of at most 40 characters;
   suggested values are `android`, `windows`, `linux`, `macos`, and `ios`.
   Response:

   ```json
   {
     "device_code": "6f1c2d…opaque…",
     "user_code": "BCDF-GHJK",
     "verification_uri": "https://loki.example.com/pair",
     "verification_uri_complete": "https://loki.example.com/pair?code=BCDF-GHJK",
     "interval": 5,
     "expires_in": 600
   }
   ```

2. Show `user_code` and `verification_uri`, and ideally a QR code of
   `verification_uri_complete`. The user signs in to LOKI in any browser,
   checks the device name and platform, and approves or declines. Codes use
   consonants only and are accepted with or without the dash, in any case.
3. Poll `POST /sync/v1/pair/poll` every `interval` seconds:

   ```json
   {"device_code":"6f1c2d…opaque…"}
   ```

   | Response | Meaning |
   | --- | --- |
   | `428`, `code: authorization_pending` | Not approved yet. Keep polling. |
   | `429`, `code: slow_down` | Polled faster than `interval`. Add 5 seconds to the interval. |
   | `403`, `code: access_denied` | The user declined in the dashboard. Stop. |
   | `400`, `code: expired_token` | Expired, unknown, or already used. Start over. |
   | `200` | Approved. |

   Success:

   ```json
   {
     "token": "loki-sync-raw-token",
     "device": {"id":"c1f0…","name":"AYN Thor","platform":"android"},
     "user": {"id":"9a2e…","username":"aspen","display_name":"Aspen"}
   }
   ```

`device_code` is a secret; never display it. A pairing can be consumed once.
Starting pairings is rate-limited server-wide; a `429` without a `code` means
try again later.

### Password login (headless clients)

For scripts and devices with no browser nearby, `POST /sync/v1/login`:

```json
{"username":"aspen","password":"…","code":"123456","name":"Steam Deck","platform":"linux"}
```

`username` may also be the account's email address. `code` is the TOTP code or
a recovery code; omit it for accounts without two-factor authentication. The
response is the same `{token, device, user}` as a successful poll.

| Response | Meaning |
| --- | --- |
| `401`, `code: invalid_credentials` | Wrong username, password, or code. The same answer for each, on purpose. |
| `403`, `code: forbidden` | The account is not approved or has no sync access. |
| `429` without a code | Too many attempts for this account; shared with the browser sign-in. |

Prefer pairing whenever a screen is available, since it keeps the password and
second factor off the device.

### Current device

- `GET /sync/v1/me` returns
  `{"user":{…},"device":{…},"quota":{"used":1048576,"limit":10737418240},"max_file_bytes":2147483648}`.
  `used` counts each distinct blob you uploaded once, including blobs only
  referenced by history.
- `DELETE /sync/v1/device` signs this device out and revokes its token. It
  returns `{"ok":true}`. Local files are untouched.

## Folders

`GET /sync/v1/folders` returns a bare array, sorted by name:

```json
[{"id":"3d9b…","name":"RetroArch saves","kind":"retroarch-saves","keep_versions":10,"seq":42,"created":1793300000}]
```

`POST /sync/v1/folders` with `{"name":"RetroArch saves","kind":"retroarch-saves"}`
returns `201` with the new folder object.

- `name` is 1–100 characters. A name you already use returns `409` with code
  `exists`.
- `kind` defaults to `generic`. It uses lowercase ASCII letters, digits, and
  hyphens, at most 64 characters, such as `retroarch-saves`.
- A user can have at most 200 folders.

`PATCH /sync/v1/folders/{id}` with any of `{"name":"…","keep_versions":20}`
returns the updated folder. `keep_versions` is 1–1000; lowering it prunes
history immediately. Folders can be deleted only from the dashboard, so a buggy
client can never wipe one. A folder that is not yours returns `404` with code
`unknown_folder`, the same as one that does not exist.

## Changes feed

```text
GET /sync/v1/folders/{id}/changes?since=0&limit=1000&wait=0
```

Returns every path whose latest version has `seq > since`, in ascending `seq`
order. Each path appears once, at its current version:

```json
{
  "seq": 44,
  "more": false,
  "reset": false,
  "changes": [
    {"path":"gba/Pokemon Emerald.srm","version":3,"hash":"9f86d08…","size":131072,"mtime":1793320000000,"device":"c1f0…","seq":43},
    {"path":"gba/old.srm","version":2,"hash":null,"size":0,"mtime":0,"device":"77ab…","seq":44}
  ]
}
```

- `since=0` returns the full manifest, including tombstones.
- `limit` defaults to 1000 and is clamped to 1–5000. When `more` is `true`, `seq`
  is the last change returned: request again immediately with `since` set to it.
- Always store the returned `seq` as your cursor, even when `changes` is empty.
- **`reset: true`** means your cursor was ahead of the server's `seq`, for
  example because the server was restored from a backup. The response is then
  the full manifest from `0`. Your local index describes versions the server no
  longer has: discard it (keep the local files), set the cursor to `0`, and
  sync again as a device that has never synced this folder.
- `wait` (0–60 seconds) long-polls: if nothing is newer than `since`, the server
  holds the request until a commit lands in this folder or the wait ends, then
  answers normally, possibly with no changes. The server answers at once when
  too many requests are already waiting, and early during maintenance, so loop
  without assuming the full wait elapsed. Use a client read timeout of at least
  `wait + 15` seconds.

## Blobs

Blob URLs use the lowercase hex SHA-256: `/sync/v1/blobs/{hash}`.

- `HEAD` returns `200` with `Content-Length` if you may use this blob, or `404`.
- `GET` streams the contents (`application/octet-stream`, with an `ETag` of the
  quoted hash) and supports a single `Range: bytes=…` header for resuming
  (`206`; `416` if unsatisfiable). Verify the SHA-256 of what you receive.
  A blob you may not use returns `404` with code `unknown_blob`.
- `PUT` takes the raw bytes as the body (`Content-Type: application/octet-stream`;
  send `Content-Length` so oversized uploads fail before any bytes move). The
  server hashes while it writes and checks the hash, `max_file_bytes`, and your
  quota before keeping it.

  | Response | Meaning |
  | --- | --- |
  | `201` `{"ok":true,"size":131072}` | Stored. |
  | `200` `{"ok":true,"existing":true}` | You already had this blob; nothing was read. |
  | `400`, `code: invalid_hash` | The URL is not a lowercase SHA-256 hex digest. |
  | `400`, `code: hash_mismatch` | The bytes do not match the URL hash. |
  | `413`, `code: too_large` | Larger than `max_file_bytes`. |
  | `413`, `code: quota_exceeded` | Would exceed your quota (see `GET /me`). |

  A failed upload leaves nothing behind; retry from the start.

You can only `HEAD`, `GET`, or commit hashes **you** have uploaded. Another
user's identical file still has to be uploaded by you once. This stops anyone
from probing whether a file exists, or fetching it by knowing its hash. Blobs
that nothing references are garbage-collected after 24 hours, so commit soon
after uploading.

An empty file is the blob
`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` and is uploaded
like any other (with an empty body).

## Commit

```text
POST /sync/v1/folders/{id}/commit
```

```json
{
  "on_conflict": "reject",
  "changes": [
    {"path":"gba/Pokemon Emerald.srm","base_version":2,"hash":"9f86d08…","size":131072,"mtime":1793320000000},
    {"path":"snes/new.srm","base_version":0,"hash":"2c26b46…","size":8192,"mtime":1793320100000},
    {"path":"gba/old.srm","base_version":1,"hash":null,"size":0,"mtime":0}
  ]
}
```

- `base_version` is the version you last saw for this path, including a
  tombstone's version; `0` means you have never seen the path.
- `hash: null` deletes the path. Upload every non-null hash first.
- `size`, when given, must equal the uploaded blob's size.
- 1–1000 changes per commit (at most 4 MiB of JSON), each path at most once.
- `on_conflict` is `reject` (default), `overwrite`, or `stash`; see below.

### Whole-request errors

These reject the **whole** commit and change nothing:

| Status | `code` | Meaning |
| --- | --- | --- |
| `400` | `invalid_path` | A path breaks the [path rules](#paths) |
| `400` | `invalid_hash` | A hash is not lowercase SHA-256 hex |
| `400` | `unknown_blob` | A hash you have not uploaded |
| `400` | `size_mismatch` | `size` differs from the uploaded blob |
| `400` | none | Empty or oversized `changes`, a repeated path, a negative `base_version`, or an unknown `on_conflict` |
| `404` | `unknown_folder` | Not your folder |

The message names the offending path. If you cannot tell which change caused
it, retry the changes one per commit and set the failing ones aside.

### Per-change results

Otherwise each change gets one result, in request order:

```json
{
  "results": [
    {"path":"gba/Pokemon Emerald.srm","status":"ok","version":3,"seq":43,"conflict":false},
    {"path":"snes/new.srm","status":"conflict","current":{"version":1,"hash":"fcde2b2…","size":8192,"mtime":1793319000000,"device":"77ab…","seq":40}},
    {"path":"Zelda.srm","status":"rejected","code":"case_conflict","error":"Zelda.srm cannot coexist with zelda.srm."}
  ]
}
```

| `status` | Fields | Meaning |
| --- | --- | --- |
| `ok` | `version`, `seq`, `conflict` | Your change is the current version. `conflict: true` means `overwrite` replaced a version you had not seen. |
| `conflict` | `current` | Not applied; the server version stays current. |
| `stashed` | `history_id`, `current` | Not applied; your upload was saved in history as an unresolved conflict. |
| `rejected` | `code`, `error` | Not applied because the path cannot coexist with an existing one: `case_conflict` or `path_conflict` (see [Paths](#paths)). |

`current` is the server's current version, `{version, hash, size, mtime,
device, seq}` without the path, or `null` when the server has never had the
path (your `base_version` came from a server that has since been restored).

If the change's hash already equals the server's current hash, including a
delete of a path that is already deleted or absent, the result is `ok` with the
existing `version` and `seq` and no new version is created. A retried commit
whose response was lost is therefore harmless. That shortcut result has no
`conflict` field.

### Conflicts

A change conflicts when `base_version` is not the server's current version:
another device committed in between. What happens depends on `on_conflict`:

| `on_conflict` | Current version afterwards | Result | History |
| --- | --- | --- | --- |
| `reject` | Server's, unchanged | `conflict` | Nothing added |
| `overwrite` | Yours (new version) | `ok`, `conflict: true` | Server's old version, marked as a conflict |
| `stash` | Server's, unchanged | `stashed` | Your upload, marked as a conflict |

A conflicting **delete** with `stash` has nothing to save, so it returns
`conflict` like `reject`. With `overwrite` and `stash`, nothing is lost: the
losing side is kept as an unresolved conflict, exempt from pruning, and shown
in the dashboard until the user picks a version. After a `conflict` or
`stashed` result, download `current` so the local file matches the server
again.

## History and restore

`GET /sync/v1/folders/{id}/history?path=gba%2FPokemon%20Emerald.srm` returns a
bare array, newest first, at most 500 entries:

```json
[
  {"id":118,"path":"gba/Pokemon Emerald.srm","version":2,"hash":"5feceb6…","size":131072,"mtime":1793310000000,"device":"77ab…","device_name":"Desktop","created":1793320005,"reason":"conflict","resolved":false},
  {"id":97,"path":"gba/Pokemon Emerald.srm","version":1,"hash":"6b86b27…","size":131072,"mtime":1793300000000,"device":"c1f0…","device_name":"AYN Thor","created":1793310004,"reason":"replaced","resolved":true}
]
```

- `path` is optional; without it, the whole folder's history is returned.
  `conflicts=true` returns only unresolved conflicts.
- `reason` is `replaced`, `deleted` (the content before a delete), or
  `conflict`. `version` is the version the content had, or for a stashed
  upload, the `base_version` it was committed against.
- `device_name` is the device's current name, `Dashboard` for changes made in
  the browser, or `Removed device`.
- History blobs can be downloaded through `/sync/v1/blobs/{hash}`.

`POST /sync/v1/folders/{id}/restore` with `{"history_id":118}` makes that content
the new current version, so every device pulls it like any other change, and
marks the entry resolved. It returns `{"ok":true,"path":"…","version":4,"seq":45}`.
Restoring content that is already current only marks the entry resolved. An
unknown entry returns `404`; restoring onto a path that now clashes with
another returns `409` with code `path_conflict`.

## Paths

Paths are relative to the folder and use these rules on every platform. A path
that breaks them returns `400 invalid_path`.

- UTF-8, **NFC-normalized**, at most 1024 bytes. Normalize before using a path
  as a key in your index and before sending. macOS and some Android file
  systems return NFD names.
- `/` separates components. No leading `/`, no empty, `.` or `..` components.
- No `\`, `:`, NUL, or other control characters.
- No component may end with a space or `.`.
- No Windows device names as a component, with or without an extension, in any
  case: `CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`.

Two kinds of clash are rejected per change (`status: rejected`) when creating a
path that is not live:

- **`case_conflict`:** an existing live path matches it case-insensitively.
  Windows and many Android storage volumes are case-insensitive, so `Zelda.srm`
  and `zelda.srm` cannot both exist. Renaming only the case of a file is a
  delete plus a create; commit the delete first.
- **`path_conflict`:** a file would be needed where a directory is, or the
  reverse: `gba` cannot be a file while `gba/emerald.srm` exists.

There are no directories in the API. A directory exists locally when some file
is inside it. Clients should skip empty directories, symbolic links, and their
own temporary files, and report (not upload) local names that break these rules.

## Errors

Errors use LOKI's usual shape plus a machine-readable `code`:

```json
{"error":"Your sync storage is full. Delete folders or ask the server owner for more space.","code":"quota_exceeded"}
```

`error` is a human-readable message for logs and UI. Branch on `code` when it
is present, and on the HTTP status otherwise. Plain validation errors (`400`),
rate limits (`429`), and maintenance (`503`) have no `code`.

| `code` | Status | Where |
| --- | --- | --- |
| `unauthorized` | `401` | Any bearer request: token missing, revoked, or invalidated. Pair again. |
| `forbidden` | `403` | Any bearer request or login: no sync access. |
| `invalid_credentials` | `401` | Login |
| `authorization_pending` | `428` | Pairing poll |
| `slow_down` | `429` | Pairing poll |
| `access_denied` | `403` | Pairing poll: declined |
| `expired_token` | `400` | Pairing poll |
| `exists` | `409` | Folder create or rename |
| `unknown_folder` | `404` | Folder routes |
| `invalid_path` | `400` | Commit |
| `invalid_hash` | `400` | Commit, blob upload |
| `unknown_blob` | `400` / `404` | Commit (`400`), blob download (`404`) |
| `size_mismatch` | `400` | Commit |
| `hash_mismatch` | `400` | Blob upload |
| `too_large` | `413` | Blob upload |
| `quota_exceeded` | `413` | Blob upload |
| `case_conflict`, `path_conflict` | in results | Commit (`status: rejected`); `path_conflict` is also a `409` from restore |

Other statuses: `404` without a code for an unknown history entry; `429`
without a code, rate limited (back off); `503` the server is in maintenance
(for example moving its data directory), so retry with backoff.

## Recommended client algorithm

This is what the reference client implements, and what a robust Android or
desktop client should do. Hashes decide whether content changed; `mtime` and
`size` are only a cheap hint for when to re-hash.

### Local state

Per synced folder, persist outside the synced directory (or in an excluded
file such as `.loki-sync-index.json`):

- The folder `id` and `cursor`: the last `seq` received from the changes feed.
- An index of `path → {version, hash, size, mtime}` as of the last sync. `hash`
  is `null` for a tombstone you have applied. Keep tombstones so a re-created
  file is committed with the right `base_version`.

Write the index atomically (temp file and rename) after every step, so a crash
or kill mid-sync resumes cleanly.

### One sync round

1. **Scan.** Walk the local directory. For each file, NFC-normalize the
   relative path. If `size` and `mtime` match the index, assume unchanged;
   otherwise hash it. A file is **locally changed** when its hash differs from
   the index, it is new, or an indexed file with a hash is gone.
2. **Settle.** Wait about 3 seconds and stat again. Skip any file whose size or
   mtime moved; it is still being written. Emulators often write saves while
   running, and in several steps.
3. **Pull.** Read the changes feed from `cursor` (loop while `more`). If
   `reset` is `true`, discard the index, set the cursor to `0`, and start the
   round again. For each remote change:
   - Same version and hash as the index: your own commit echoed back; skip it.
   - Same hash as the local file: record the new version in the index.
   - Not locally changed: apply it. Download the blob to a temporary file in
     the same directory, verify its SHA-256, flush it, then rename it over the
     target. For a tombstone, delete the local file. Update the index.
   - Locally changed: leave the local file alone. Step 4 commits it against
     the old `base_version` and the server reports the conflict.

   Then store the new cursor.
4. **Push.** For each locally changed path, `HEAD` its blob and `PUT` it if
   missing. Then commit, using `base_version` from the index (`0` if absent).
   Group the changes by conflict policy (below), one commit per group.
5. **Apply results.**
   - `ok`: store the returned `version` and your hash in the index.
   - `conflict` or `stashed` with a `current`: download `current` and apply it
     as in step 3, unless you are keeping the local file for the user to
     resolve.
   - `conflict` or `stashed` with `current: null`: the server never had the
     path. Remove it from the index so the next round commits it as new.
   - `rejected`: leave the file alone and tell the user which path clashes.
6. **Wait.** Long-poll the changes feed with `wait=30` (or `60`) and start a
   new round when it returns changes, when a file watcher fires, or every few
   minutes. On a phone, sync when the emulator exits or the app returns to the
   foreground rather than holding a long-poll open on battery.

Never write a local file in place, and never delete one because its `mtime`
is older. Never let the local clock decide which version wins.

### Conflict policy

For binary save files there is no merge, so pick a side and keep the other in
history:

- **The path is in the index and changed locally since the last sync:** use
  `overwrite`. The play session on this device is the newest one the user
  knows about. The other device's version is kept as a conflict.
- **The path is not in the index** (first sync on this device, a lost index,
  or after `reset`) and the server already has it with a different hash: use
  `stash`. The server copy stays current, this device's copy is kept as a
  conflict, and the device downloads the server copy. A freshly installed
  device can never clobber saves.
- **Deletes:** use `reject`. If another device changed the file, download its
  version instead of deleting someone's progress.
- **Interactive clients** may use `reject` and ask the user, then commit again
  with the chosen side and `current.version` as `base_version`.

### Retries

Reads, blob uploads, and commits are all safe to retry. A repeated `PUT` of a
blob you have returns `200`. A repeated commit whose first attempt succeeded
returns `ok` with the same version, because the hash already matches. If a
response was lost, pulling first also works: your own change comes back in the
feed with the hash you have locally.

## Emulator notes

- **SRAM saves** (`.srm`, `.sav`, `.mcr`, `.dsv`, memory cards) are small and
  the main thing to sync. They are what the game itself wrote and stay
  compatible across devices running the same core or emulator.
- **Save states** (`.state`, `.state1`, `.ss0`, …) are snapshots of the
  emulator's memory. They are large, and often only load in the same emulator,
  core version, and sometimes the same architecture. Sync them in a separate
  folder, or not at all, so a state from one build cannot replace a working
  save on another.
- **Per-device mapping.** The server only knows folder-relative paths. Each
  device maps a folder to its own local directory, for example
  `/storage/emulated/0/RetroArch/saves` on the AYN Thor and
  `C:\RetroArch\saves` on Windows. Keep the same layout below that root on all
  devices (RetroArch's "sort saves by core" setting, for example, must match).
- **`kind` hints.** `kind` is a hint for clients to choose defaults (file
  filters, local paths, name translation). Suggested values: `generic`,
  `retroarch-saves`, `retroarch-states`, `dolphin-gc`, `pcsx2-memcards`,
  `duckstation-memcards`, `ppsspp-saves`. The server never interprets it.
- **Names.** ROM-derived names often contain `:` (not allowed) or differ only
  in case between front ends. Translate or skip such names on the client, and
  report the ones you skip.
- **Write timing.** Many emulators flush SRAM periodically while running.
  Prefer syncing after the emulator closes; otherwise rely on the settle window
  and the version history.
