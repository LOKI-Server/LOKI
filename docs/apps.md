# Building LOKI apps

A LOKI app is a WebAssembly component (`wasm32-wasip2`) packaged with its assets
in a `.loki-app` file. One package runs on every OS and CPU LOKI supports. LOKI
hosts the app's HTTP traffic and gives it sign-in, permissions, SQLite, uploads,
storage and file streaming, so the app needs no web server, account system or
database library of its own.

Apps export the standard `wasi:http/incoming-handler` and import LOKI's typed
host interfaces, the `loki:app@0.3.0` package in
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
page, grant access, and enable it.

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
  "built_with": {"sdk": "0.3.0", "cargo_loki": "0.3.0"}
}
```

- **IDs** are lowercase ASCII letters, digits and hyphens, at most 64 bytes;
  `loki-` is reserved. **Versions** use semver.
- **Capabilities** are opt-in: `database`, `uploads`, `forms`, `storage`,
  `realtime`. LOKI enforces them even if the SDK is bypassed.
- **Fields** are the app's settings, shown to the owner: kinds `text`, `secret`,
  `number` and `bool`. Secrets are masked in the dashboard.
- **Limits** accept `memory_mib` 16–256, `fuel` 100,000–10,000,000,000 per
  request (roughly one unit per Wasm instruction), `timeout_seconds` 1–120,
  `concurrent_requests` 1–8 and `request_bytes` 1 KiB–1 GiB.
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

- only adding to the `loki:app@0.3` host interfaces, as 0.3.x versions, never
  changing or removing what an app imports;
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
| `storage` | `set`, `get`, `delete` JSON values; `write-file`, `read-file`, `get-file-info`, `delete-file`, `list-files`, `get-usage` | `storage` |
| `responses` | `serve-file`: answer with an upload, storage file or package asset | that file's capability |
| `files` | `size` and ranged `read` (at most 1 MiB) of the app's own files | that file's capability |
| `realtime-control` | `restart` the realtime instance | `realtime` |
| `realtime` | `send`, `send-file`, `close` a WebSocket connection; realtime instance only | `realtime` |

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
or read) under `app-data/APP_ID/storage`, up to 64 MiB and 4,096 entries. Names
are at most 120 UTF-8 bytes; keys can't contain slashes, and file paths can't
traverse or name devices. Listings return up to 100 sorted paths per page.

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
in the database or storage. Instances are bounded by the manifest's memory, fuel
and time limits, and at most eight component requests run at once across LOKI.
Responses built in the app are limited to 8 MiB. Apps get no environment, stdio,
directories, sockets or outgoing HTTP. A trap or an exhausted limit fails that
request only; LOKI and other requests carry on.

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
component also exports `realtime-handler`:

| Export | When LOKI calls it |
| --- | --- |
| `on-start` | The instance started, or restarted with a fresh state |
| `on-open(connection, peer)` | A WebSocket connected; `peer` has its `IP:port` (port 0 behind Caddy) and, for a signed-in browser, the user |
| `on-message(connection, data)` | A message arrived |
| `on-close(connection)` | The connection closed, from either side |
| `on-tick` | Every `tick_ms`, when set |

It answers with `realtime.send`, `send-file` (streams a file as 64 KiB messages,
in order with other sends) and `close`. HTTP handlers can restart it with
`realtime-control.restart`, for example after a settings change. The SDK's
`FileReader` reads large files in ranges with `Read + Seek`.

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
