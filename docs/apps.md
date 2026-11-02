# Building LOKI apps

A LOKI app is a WebAssembly component (`wasm32-wasip2`) packaged with its assets
in a `.loki-app` file. One package runs on every OS and CPU LOKI supports. LOKI
hosts the app's HTTP traffic and gives it sign-in, permissions, SQLite, uploads,
storage and file streaming, so the app needs no web server, account system or
database library of its own.

Apps export the standard `wasi:http/incoming-handler` and import LOKI's typed
host interfaces, the `loki:app@0.3.1` package in
[`crates/loki-sdk/wit/platform.wit`](../crates/loki-sdk/wit/platform.wit). Rust
apps use the [`loki-sdk`](../crates/loki-sdk/README.md) crate and the
[`cargo-loki`](../crates/cargo-loki/README.md) tool, and never touch the WIT
directly.

## Getting started

```powershell
rustup target add wasm32-wasip2
cargo install cargo-loki --locked
cargo loki new my-app
cd my-app
cargo loki dev --server PATH_TO_LOKI_SERVER
```

Implement a plain `fn(Request) -> Result<Response>` and export it with
`loki_sdk::export!(handle)`. `cargo loki dev` runs a separate local LOKI with a
development account, installs the app and rebuilds it on changes; its data
persists under `target/loki/dev/APP_ID`. `cargo loki package` builds the release
component and writes the `.loki-app`. Import that in LOKI's **Applications**
page, grant access, and enable it. To let others install it, list it in the
[app registry](#publishing-to-the-app-registry).

App metadata lives in `[package.metadata.loki]` in `Cargo.toml`; the Cargo
package version is the app version. See the
[SDK guide](../crates/loki-sdk/README.md) for the Rust API.

## Packages

A `.loki-app` is a ZIP with `manifest.json` at its root, which `cargo loki
package` writes:

```json
{
  "protocol": 1,
  "runtime": "wasi",
  "target": "wasm32-wasip2",
  "id": "my-app",
  "name": "My app",
  "version": "0.1.0",
  "entry": "my_app.wasm",
  "health": "/health",
  "roles": ["admin"],
  "capabilities": ["database", "uploads", "forms"],
  "limits": {
    "memory_mib": 64,
    "fuel": 50000000,
    "timeout_seconds": 30,
    "concurrent_requests": 4,
    "request_bytes": 16777216
  },
  "fields": [],
  "files": {"my_app.wasm": "SHA256_HEX", "static/style.css": "SHA256_HEX"},
  "built_with": {"sdk": "0.5.0", "cargo_loki": "0.5.0"}
}
```

- **IDs** are lowercase ASCII letters, digits and hyphens, at most 64 bytes;
  `loki-` is reserved. **Versions** use semver.
- **Capabilities** are opt-in: `database`, `uploads`, `forms`, `storage`,
  `realtime`, `outbound`. LOKI enforces them even if the SDK is bypassed.
- **Fields** are the app's settings, shown to the owner: kinds `text`, `secret`,
  `number` and `bool`. Secrets are masked in the dashboard.
- **Limits** accept `memory_mib` 16–256, `fuel` 100,000–10,000,000,000 per
  request (roughly one unit per Wasm instruction), `timeout_seconds` 1–120,
  `concurrent_requests` 1–8, `request_bytes` 1 KiB–1 GiB, `storage_mib` 1–4096
  (default 64), `storage_entries` 16–65,536 (default 4,096) and
  `reuse_instances` (default false; see [Database and execution](#database-and-execution)).
- **Outbound** lists the hosts an app with `outbound` may send requests to; see
  [Outgoing requests](#outgoing-requests).
- **Schedules** are jobs LOKI runs on a timer; see [Scheduled jobs](#scheduled-jobs).
- **Health:** a GET to `health` must succeed before the app goes live. It runs
  without a user and may create the app's database tables.
- **Files:** every entry must be listed with its SHA-256. Links, traversal,
  duplicates and undeclared files are rejected. Limits are 2,048 entries, a
  128 KiB manifest, 512 MiB expanded, and 32 MiB of component code. Checksums
  catch corruption, not tampering, so import only packages you trust.

### Compatibility

**An app built with `loki-sdk` 0.3 or later keeps running on later LOKI
releases.** You don't need to rebuild it when LOKI updates.

LOKI, `loki-sdk` and `cargo-loki` share one version number, so an app built
with SDK X runs on LOKI X or newer. `built_with` records the SDK version the app
resolved to. LOKI runs an app when that SDK is at least the oldest it supports
(0.3.0) and no newer than LOKI itself. Otherwise the import, or the start of an
installed app, fails with what to do: update LOKI for an app built with a newer
SDK, or rebuild an app built before 0.3. Packages without `built_with` predate
LOKI 0.3.

LOKI keeps that promise by:

- only adding to the `loki:app@0.3` host interfaces, as 0.3.x versions (now
  0.3.1), never changing or removing what an app imports;
- providing any breaking new interface version alongside the old one;
- testing every change against apps frozen at each supported SDK.

Support for an SDK ends only by a deliberate release that raises the oldest
supported version and says so in its release notes.

## Identity and access

Each app has its own origin: `http://APP_ID.localhost:LOKI_PORT` locally, or an
HTTPS hostname that LOKI's Caddy forwards to the same listener. Apps never share
the dashboard's origin.

LOKI signs people in on the app's origin and passes the current user to the app
on every request, with the roles they hold from current grants. It checks
approval, access and roles each time, so suspensions, password resets and revoked
sessions take effect immediately. Cookies, `Authorization` headers, keys and
client secrets never reach the app, and app responses cannot set cookies.

- Use the user's stable `id` as the identity key. Usernames, names and email can
  change, and email is unverified.
- Access is restricted by default. Public apps can serve anonymous reads; changes
  from a browser need a signed-in, approved account.
- The owner has every app role. Other people get a role such as `admin` only
  when granted it.
- `/_loki/*` belongs to LOKI.

### Signing in and out

People sign in with their LOKI account, and type their password only on LOKI's
own address, never the app's.

- **Signing in.** Opening a page of a restricted app without being signed in
  shows LOKI's "Sign in to APP" page on the app's own address. Its button opens
  LOKI's sign-in in a small window, like "Sign in with Google", which closes
  itself and returns the person to the page they asked for; if the browser blocks
  the window, the sign-in happens in the page instead. Someone already signed in
  to LOKI just clicks through. Requests from scripts redirect to `/_loki/start`.
- **Your own sign-in link**, for example in a public app: `/_loki/start?next=/path`
  returns to `/path`, and adding `&popup=1` finishes in a window opened by your
  page.
- **Signing out.** A POST to `/logout` (with the CSRF token) signs out of the
  app and shows a "signed out" page on its address, which also offers signing
  out of LOKI. The app stays signed out until the person signs in again.
- **Signing out of LOKI** signs out of every app and gated route opened from that
  sign-in. App sessions last at most seven days and never outlive it.

### Device sign-in

Devices that can't comfortably use a browser, such as consoles, TVs and desktop
clients, sign in with a code (RFC 8628):

1. The device POSTs a form with `client_id=component-APP_ID` (and optionally
   `name` and `platform` for the approval page) to `/oidc/device`. No secret is
   needed. The response has `device_code`, `user_code`,
   `verification_uri_complete`, `interval` (5) and `expires_in` (600).
2. The device shows `user_code`, or a QR code of `verification_uri_complete`.
   Someone with access to the app approves it at `/pair` in LOKI.
3. The device POSTs `grant_type=urn:ietf:params:oauth:grant-type:device_code`,
   `client_id` and `device_code` to `/oidc/token` every `interval` seconds. Until
   approval it gets `400 authorization_pending`; polling too fast gives
   `slow_down` (add 5 seconds to the interval), a declined code `access_denied`,
   and an expired one `expired_token`.

The device then sends `Authorization: Bearer ACCESS_TOKEN` to the app, which
sees the user as for a browser; bearer requests need no CSRF token. Access tokens
last 15 minutes. The device refreshes with `grant_type=refresh_token`,
`refresh_token` and `client_id`; refresh tokens last seven days, rotate, and
reusing a spent one ends the sign-in.

## Host API

| Interface | Functions | Needs |
| --- | --- | --- |
| `request` | `get-context` (app, origin, user with roles, CSRF token, settings as JSON, panel action, LAN addresses), `get-form`, `log` | `get-form` needs `forms` |
| `database` | `transaction`: 1–64 statements with typed parameters, returning columns, rows and affected counts | `database` |
| `uploads` | `retain` a request's upload at a new path, `delete` a retained one | `uploads` |
| `storage` | `set`, `get`, `delete` JSON values; `write-file`, `read-file`, `get-file-info`, `delete-file`, `list-files`, `get-usage`, `append-file`, `rename-file` | `storage` |
| `responses` | `serve-file`: answer with an upload, storage file or package asset | that file's capability |
| `files` | `size` and ranged `read` (at most 1 MiB) of the app's own files | that file's capability |
| `realtime-control` | `restart` the realtime instances, `call` one, `list-rooms` | `realtime` |
| `realtime` | `send`, `send-text`, `broadcast`, `send-file`, `close` a WebSocket connection; `set-timer`, `cancel-timer`; `room`, `open-room`, `close-room`, `post`, `list-rooms`; realtime instances only | `realtime` |
| `wasi:http/outgoing-handler` | Standard outgoing HTTP(S) requests, to listed hosts | `outbound` |

Every service is scoped to the installed app. A failed call returns an `error`
whose `status` follows HTTP: 400 invalid input, 403 a missing capability or
access, 404 not found, 409 a conflict, 413 over a limit, 503 unavailable, 500 a
host failure.

**Serving files.** After `serve-file` the app returns an ordinary 200 response,
and LOKI streams the file in its place, with ranges, content type and download
disposition. An error status from the app wins instead. Files stream from disk in
64 KiB pieces, never through guest memory. HTML, SVG and unknown upload formats
download rather than display. Assets are the package's declared files other than
the component.

**Storage** holds JSON values (128 KiB each) and generated files (1 MiB per write
or read) under `app-data/APP_ID/storage`, up to the app's quota: `storage_mib`
(64 MiB unless the app asks for more, up to 4 GiB) and `storage_entries`. The
owner can set a different quota in the app's settings. `append-file` adds up to
1 MiB at a time, so a file such as a game world can grow to the whole quota; read
it back in ranges with `files.read`. `rename-file` moves a file in one step:
append a new version under a temporary name, then rename it over the old one, so
nothing ever reads half a file. Names are at most 120 UTF-8 bytes; keys can't
contain slashes, and file paths can't traverse or name devices. Listings return up
to 100 sorted paths per page.

## Forms and uploads

With `forms`, LOKI parses URL-encoded and multipart requests, checks their
`csrf_token` field against the context's CSRF token, and hands them to the app
through `get-form`. JSON and other requests send `X-CSRF-Token` instead.
Cross-origin requests are rejected; reads need no token.

Uploaded files stream to private temporary files. The app checks their names and
types, then keeps the ones it wants with `uploads.retain` at a path that must not
exist yet. Anything not retained is deleted when the request ends.

Limits: 64 fields, 8 files, 64 KiB per text field, the whole request within
`request_bytes`, and each file within the app's `max-upload` setting if it has
one.

## Database and execution

The `database` capability gives the app its own SQLite database,
`app-data/APP_ID/app.db`, with foreign keys and WAL. Each `transaction` call is
atomic: up to 64 statements, 256 parameters each, 64 KiB of SQL, 1 MiB per
value, about five seconds of execution, and results of 2,000 rows and 4 MiB.
The database can grow to 256 MiB. Store binary data as uploads or storage files,
not blobs. ATTACH, PRAGMA, virtual tables and file-loading functions are blocked.

Each request runs in a fresh instance, so Rust globals don't persist; keep state
in the database or storage. With `"reuse_instances": true` in `limits`, LOKI keeps
finished instances for the next request instead, so memory such as parsed
templates or caches carries over. Any request may still get a fresh instance: an
instance is replaced after a trap, a timeout or 1,000 requests, and health checks
always get a new one. Don't keep anything there you can't rebuild.

Instances are bounded by the manifest's memory, fuel and time limits, and LOKI
runs two component requests per CPU core at once (8–64) across every app.
Responses stream as the app writes them, up to 1 GiB, within `timeout_seconds`;
short ones are sent whole with their length. LOKI compiles each component once
and keeps the result in `cache/components` in its data folder, so apps start in
milliseconds after the first time. Apps get no environment, stdio, directories or
sockets, and no outgoing HTTP without `outbound`. A trap or an exhausted limit
fails that request only; LOKI and other requests carry on.

## Outgoing requests

With the `outbound` capability, an app can send HTTP and HTTPS requests to the
hosts its manifest lists, for example to announce itself to a server list or call
a webhook:

```json
"capabilities": ["outbound"],
"outbound": ["api.example.com", "*.discord.com", "192.168.1.20:8080"]
```

- An entry is a hostname or IP address; `*.example.com` matches any subdomain,
  not `example.com` itself. Without a port, ports 80 and 443 are allowed. At most
  32 entries.
- LOKI refuses addresses on this machine or a private network (loopback,
  10.x, 192.168.x, link-local and the like) unless the entry is that IP address
  itself, so a listed name can't be pointed at your LAN.
- Requests connect within 10 seconds and must start answering within 30. An app
  may have 8 requests waiting at once.
- Apps use the standard `wasi:http/outgoing-handler`; the Rust SDK wraps it as
  `loki_sdk::fetch`. A refused request fails with 403.

## Scheduled jobs

LOKI can run jobs on a timer while the app is enabled, such as cleaning up old
rooms or posting a daily summary:

```json
"schedules": [{"name": "cleanup", "every_minutes": 60}]
```

Every `every_minutes` (1–10,080) LOKI sends `POST /_loki/schedule/NAME` to the
app's own handler, without a user, within the app's request limits. The first run
comes one interval after the app starts. A job still running when the next is due
makes that one skip. Requests to `/_loki/schedule/*` from outside never reach the
app. Failures go to the app's log. In Rust, `Router::schedule("cleanup", handler)`.

## Panels

With `"panel": true`, LOKI draws a panel for the app inside its dashboard, so
services such as game servers need no pages of their own. Add `"site": false`
when the app has no pages at all. Everyone with access to the app gets an **Open
panel** button, and the app decides what each person sees from their roles.

LOKI calls the app's own handler: `GET /_loki/panel` to show the panel, and
`POST /_loki/panel` to perform `context.panel_action` first. A panel action is
`{action, value, fields, files}`: a button's action and value, a form's fields,
or uploaded files to retain. The app answers with JSON, at most 256 KiB:

```json
{"panel":{"sections":[{"title":"Dreams","items":[
  {"type":"stats","items":[{"label":"Players","value":"2"}]},
  {"type":"list","empty":"No dreams yet.","items":[{"title":"Poolrooms","detail":"0.3 MB","badge":"Playing",
    "actions":[{"action":"remove","label":"Remove","value":"a1","style":"danger","confirm":"Remove this dream?"}]}]},
  {"type":"upload","action":"upload","label":"Add a dream","accept":".dream","button":"Upload"}
]}]},"notice":"Uploaded.","error":""}
```

| Item | Fields |
| --- | --- |
| `text` | `text`, `tone`: `normal`, `muted` or `warning` |
| `stats` | `items`: up to 16 `{label, value}` |
| `list` | `items`: up to 500 `{title, detail, badge, actions}` (4 actions each), `empty` |
| `upload` | `action`, `label`, `accept`, `button` |
| `buttons` | `items`: up to 8 actions |
| `form` | `action`, `button`, `fields`: up to 16 `{key, label, kind, value, options}`; `kind` is `text`, `number`, `bool` or `select` |
| `code` | `label`, `text`: a value to copy, such as an address |

An action is `{action, label, value, style, confirm}`, with `style` `primary`,
`subtle` or `danger`. A panel has at most 16 sections of 32 items, all shown as
plain text. `notice` confirms an action; `error` explains a failure. The SDK has
typed builders in `loki_sdk::panel` and `Router::panel`.

## Realtime apps

With `realtime`, an app also gets one long-lived instance that LOKI keeps running
while the app is enabled, for game servers, chat and live dashboards. LOKI calls
it one event at a time, so its memory persists and needs no locks. The
component also exports `realtime-handler`, and apps built with SDK 0.5 or later
`realtime-events` too:

| Export | When LOKI calls it |
| --- | --- |
| `on-start` | The instance started, or restarted with a fresh state |
| `on-open(connection, peer)` | A WebSocket connected; `peer` has its `IP:port` (port 0 behind Caddy) and, for a signed-in browser, the user |
| `on-message(connection, data)` | A message arrived |
| `on-close(connection)` | The connection closed, from either side |
| `on-tick` | Every `tick_ms`, when set |
| `on-messages(messages)` | Several messages were waiting, in order, in place of `on-message` each (`realtime-events`) |
| `on-timer(id)` | A timer from `set-timer` ran out (`realtime-events`) |
| `on-call(data) -> data` | `realtime-control.call` asked this instance something (`realtime-events`) |
| `on-post(from, data)` | Another room posted to this one (`realtime-events`) |

It answers with `realtime.send` (binary), `send-text`, `broadcast` (one message
to many connections, copied once), `send-file` (streams a file as 64 KiB messages,
in order with other sends) and `close`. `set-timer(id, delay-ms)` calls `on-timer`
once, replacing a waiting timer with the same id; at most 256 wait at once, and
they end with the instance. HTTP handlers can restart the instance with
`realtime-control.restart`, for example after a settings change, and ask it for
live state, such as who is playing, with `realtime-control.call`: the instance
answers from `on-call` within five seconds. The SDK's `FileReader` reads large
files in ranges with `Read + Seek`.

### Rooms

With `rooms` in its realtime limits, each room is a separate instance of the app,
with its own memory, connections, ticks and timers. Rooms run in parallel with
each other, so one busy match doesn't slow the rest.

```json
"limits": {"realtime": {"path": "/multiplayer", "rooms": {"max": 16, "idle_seconds": 60}}}
```

- Clients connect to `{path}/{room}`, such as `/multiplayer/lobby-3`; room names
  are 1–64 lowercase letters, digits and hyphens. `{path}` alone reaches the main
  instance, which always runs.
- A room starts on its first connection, or when an instance calls `open-room`,
  and closes after `idle_seconds` (5–3,600) with no connections, or on
  `close-room`. At most `max` rooms (1–64) run at once; past that, connections to
  a new room get 503.
- `room()` tells an instance which room it is; it's empty for the main instance.
  `post(room, data)` hands data to another room's `on-post`, and `list-rooms`
  lists the running instances with their connection counts.
- `max_connections` counts every room together. A room that fails closes its
  connections with 1011 and ends; the next connection starts it afresh. Restarting
  the app closes every room.

```json
"capabilities": ["realtime"],
"limits": {"realtime": {"path": "/multiplayer", "max_connections": 64, "max_message_bytes": 65536, "tick_ms": 50, "event_fuel": 5000000}}
```

| Limit | Default | Range |
| --- | --- | --- |
| `path` | `/multiplayer` | Starts with `/`; not the health path, `/logout` or `/_loki` |
| `max_connections` | 64 | 1–256 per app; 512 across LOKI |
| `max_message_bytes` | 64 KiB | 1 KiB–2 MiB, incoming |
| `tick_ms` | none | none, or 20–1000 |
| `event_fuel` | 5,000,000 | 100,000–100,000,000 per event |
| `rooms` | none | none, or `{max, idle_seconds}`: 1–64 rooms, idle 5–3,600 s (default 16, 60) |

`on-start` gets the request `fuel` and `timeout_seconds`; other events get
`event_fuel` and five seconds.

- **Who can connect:** anyone who can reach the app, whatever its visibility, so
  clients need no LOKI account. The app decides who may stay.
- **Failures:** a trap or exhausted limit closes every connection with 1011, and
  LOKI starts a fresh instance after 1 second, backing off to 60. Connections
  arriving meanwhile get 1013.
- **Limits:** a message over `max_message_bytes` closes with 1009, and more than
  4 MiB waiting to send closes with 1008. Apps close with 1000 or 3000–4999.
- **Lifecycle:** disabling, updating or backing up the app closes connections
  with 1001; a restart with 1012. Keep anything that must survive in the
  database or storage.
- **LAN address:** the owner can give a realtime app an address such as
  `0.0.0.0:4767` in its settings. LOKI then also listens there for the realtime
  path and health check only, without sign-in, so local devices can connect by
  IP.

## Updates and rollback

Importing a package with the same ID updates the app; an older version is
rejected. LOKI stops the app, snapshots its data, keeps the previous package, and
starts the new one if the app was enabled. If it fails to start, LOKI restores
the previous package and data. Settings keep matching keys and gain new
defaults. **Rollback** restores the previous package, settings and data
snapshot; changes made since are lost. Data migrations run inside the app, and
LOKI never runs installer scripts from a package.

Apps installed from the app registry update the same way. When the registry
lists a newer release, the app's card offers **Update**. Importing a package
by hand over a registry app stops those offers for it.

## Publishing to the app registry

The registry, [LOKI-Server/apps](https://github.com/LOKI-Server/apps), is a
reviewed list of apps. People browse it at https://loki-server.github.io/apps/,
and owners install from it in LOKI under **Applications → Browse apps**. Each
app is a folder holding `app.json`, which describes the latest release, and an
optional `icon.png`:

```json
{
  "id": "my-app",
  "name": "My app",
  "description": "What it does.",
  "source": "https://github.com/me/my-app",
  "version": "1.2.0",
  "url": "https://github.com/me/my-app/releases/download/v1.2.0/my-app-1.2.0.loki-app",
  "sha256": "…",
  "size": 287908,
  "sdk": "0.5.0",
  "capabilities": ["database", "forms"]
}
```

To add an app or release a new version:

1. Publish the `.loki-app` at a permanent https link, such as a GitHub release
   asset.
2. In a clone of your fork of the registry, run
   `cargo loki registry add <link> --source <repository> [--icon icon.png]`.
   It downloads the package, checks it as LOKI does, and writes
   `apps/<id>/app.json`.
3. Open a pull request. A new app also adds its folder to `CODEOWNERS`, so later
   updates need its author's approval.

Reviewers run `cargo loki registry check`, which downloads every package and
confirms it matches its entry. Merging to `master` republishes the site and its
`index.json`.

LOKI reads `index.json` from the address under **Server settings → App
registry** (empty turns it off) and checks it for updates every few hours. It
installs a release only when the download's size and SHA-256 match the entry
and the package's ID, version, SDK and capabilities match too, so it installs
exactly the file that was reviewed. Apps built with a newer SDK than the server
are listed with a request to update LOKI.
