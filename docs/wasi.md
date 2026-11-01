# LOKI WASI components

WASI components are LOKI's primary application format. A single
`wasm32-wasip2` component and its assets are packaged in a `.loki-app` ZIP and
run on each supported LOKI OS/CPU. Repositories remain independent: the contract
is WIT and HTTP, with no dependency on server internals or its account database. Rust apps use
the small `loki-sdk` crate to access this versioned contract.

Components export the standard `wasi:http/incoming-handler` interface and can
import `loki:app/platform@0.1.0`. The checked-in [WIT](../crates/loki-sdk/wit/platform.wit) defines
that optional host interface. LOKI embeds Wasmtime 47.0.4; no external runtime,
app listener, child process, OIDC client, or SQLite library is needed in a guest.

## Rust development

Install `cargo-loki` from `crates/cargo-loki`, then run `cargo loki new NAME
--sdk-path PATH_TO_LOKI_SDK`. Implement a plain `fn(Request) -> Result<Response>`
and export it with `loki_sdk::export!(handle)`. `Router` supports methods and
`{parameters}`; `Response` supports HTML, JSON, redirects, and optional Askama
rendering. No web server runs in the guest.

`request.context()` supplies typed identity, roles, CSRF, and configuration.
`Database` offers bound parameters and typed row decoding. `Form` supplies typed
fields and upload handles; `Upload::retain` and `StoredFile` keep media streaming
in the host. Enable the corresponding manifest capabilities. These APIs wrap
the operations below; the host enforces permissions and bounds independently.
See the [SDK guide](../crates/loki-sdk/README.md) for examples.

`cargo loki package` reads `[package.metadata.loki]` in Cargo.toml, builds the
release component, gathers explicitly declared assets, and generates checksums.
The Cargo package version is the app version. `cargo loki dev` builds and watches
an app in a separate development installation. Its local account and app data
persist between runs, under `target/loki/dev/APP_ID`.

## Manifest

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
  "files": {"my_app.wasm": "SHA256_HEX", "static/style.css": "SHA256_HEX"}
}
```

Capabilities are opt-in. Package path, size, and checksum validation also applies
to components. The entry must be a WebAssembly component, not a core Wasm module
or a platform-specific precompiled Wasmtime cache. Code is limited to 32 MiB.
The health request must succeed before the app becomes available. It may perform
idempotent application database initialization; no user is present during health.

## Shared server and identity

Each component has a separate origin, including local installations:
`http://APP_ID.localhost:LOKI_PORT`. Modern browsers resolve `.localhost` to
loopback; command-line clients may need a loopback DNS override. A configured
public hostname uses HTTPS and Caddy forwards it to the same LOKI listener.
Extensions are not mounted under the dashboard origin, so extension JavaScript
does not receive dashboard-origin access.

LOKI handles sign-in using a single-use authorization code, PKCE, exact app
callbacks, and per-host HttpOnly cookies. Public HTTPS authentication cookies
have the `__Host-` prefix. On every request, LOKI checks approval, current resource
access, and declared app roles. Suspensions, credential resets, and session
revocation invalidate access. Browser authentication cookies, Authorization
headers, identity-provider keys, and client secrets are withheld from the guest.
Guest responses cannot set cookies; host authentication endpoints manage them.

`/_loki/start` and `/_loki/callback` belong to LOKI. POST `/logout` revokes the
current app session and returns to LOKI. This does not log out other app hosts.
Restricted access is the default. Public apps can serve anonymous reads; state
changing browser requests require an approved signed-in account.

## Host operations

`platform.call(operation, input)` takes JSON text and returns JSON text or an
error string. Inputs are limited to 2 MiB. Host services are always scoped to the
installed app, not a guest-supplied app ID or filesystem root.

| Operation | Input | Result |
| --- | --- | --- |
| `context` | `null` | `{app_id, origin, issuer, dashboard_url, config, csrf, user}`; `user` is null or `{id, username, name, email, roles}` |
| `form` | `null` | `{fields:{...}, files:[{field,name,token,content_type,size}]}`; requires `forms` |
| `database` | `{statements:[{sql,params:[...]}]}` | `[{rows:[{column:value}],affected}]`; requires `database` |
| `retain-upload` | `{token,path}` | `{path}`; move a request's temporary upload to an unused path under this app's uploads; requires `uploads` |
| `delete-file` | `{path}` | `null`; delete an app upload; requires `uploads` |
| `file` | `{path,filename,download}` | `null`; replace the HTTP response with a streamed app upload; requires `uploads` |
| `asset` | `{path}` | `null`; stream a declared package asset, excluding the component executable |
| `log` | `{message}` | `null`; bounded application log |
| `storage-set` | `{key,value}` | `null`; atomic JSON replacement; requires `storage` |
| `storage-get` | `{key}` | `{found,value?}`; missing is distinct from stored null |
| `storage-delete` | `{key}` | boolean indicating whether the value existed |
| `storage-file-write` | `{path,data}` | `{path,size}`; data is standard base64; atomic replacement |
| `storage-file-read` | `{path}` | `{data}`; bounded standard-base64 file contents |
| `storage-file-info` | `{path}` | `{path,size}` |
| `storage-file-delete` | `{path}` | boolean indicating whether the file existed |
| `storage-list` | `{prefix?,after?}` | `{files:[{path,size}],next}`; at most 100 sorted file paths |
| `storage-usage` | `null` | `{bytes,entries,limit_bytes,max_entries,max_file_bytes,max_value_bytes}` |
| `storage-file-serve` | `{path,filename,download}` | `null`; stream a generated file as the HTTP response |

All `storage-*` operations require the `storage` capability. They use a separate
app-owned `storage/values` and `storage/files` directory under `app-data/APP_ID`,
with encoded, case-sensitive names and no ambient filesystem access. Limits are
64 MiB committed data, 4,096 entries, 128 KiB JSON values, and 1 MiB generated
file writes/buffered reads. Logical names are at most 120 UTF-8 bytes; JSON keys
cannot contain slashes. File paths may use logical slashes but cannot traverse
directories or contain absolute/device paths, backslashes, or Windows reserved
names. Listings expose logical paths, not encoded host filenames. Storage shares
the app's normal backup/snapshot/rollback lifecycle. Pending filesystem jobs
drain before snapshots, even after a guest request has timed out.

New hosts encode errors in the WIT error string as
`{"loki_error":1,"status":403,"message":"..."}`. The Rust SDK maps this to
`ErrorKind` and retains the status. It also accepts older plain-string errors.
The WIT signature and protocol version remain unchanged. Successful file/asset
markers replace only successful guest responses; they cannot override a later
guest authorization/error response.

The guest returns an ordinary successful HTTP response after `file` or `asset`;
LOKI supplies the actual body, MIME handling, ranges, and download disposition.
Files stream in bounded 64 KiB buffers rather than through Wasm linear memory.
HTML, SVG, and unknown upload formats are downloaded instead of rendered inline.
Package assets may contain the extension's own HTML, JavaScript, CSS, and fonts.

Stable `user.id` comes from LOKI. Usernames, names, and email can change; email is
unverified profile information. Guest code still applies its business rules,
including any administrator-only operations, using the supplied current roles.

## Forms and uploads

With `forms`, LOKI consumes URL-encoded/multipart requests, checks their
`csrf_token` field, and provides them through `form`. The standard incoming body
is then empty; components opting into this facility use `form`, not a second
multipart parser. Every HTML form includes the `csrf` value from `context`.
JSON or other raw requests instead include `X-CSRF-Token`. Requests with a
different Origin are rejected. Read-only methods do not require a CSRF token.

Multipart files stream to private temporary files. Guest code validates file
names, media rules, and ownership using their metadata before calling
`retain-upload`. Retained paths must be safe relative paths and cannot overwrite
an existing file. Unretained uploads are removed when the request ends, including
after a trap. The guest deletes retained files if its database operation fails.
LOKI rejects traversal and symlink/junction escapes when opening downloads.

Limits: 64 distinct fields, 8 nonempty files, 64 KiB per text field. The entire
request is bounded by `request_bytes`; each file is also bounded by the optional
`max-upload` numeric app setting. Bigshot defaults to two 5 MiB files plus form
overhead within a 16 MiB request limit.

## Database and execution model

The `database` capability provides a dedicated `app-data/APP_ID/app.db` SQLite
database with foreign keys, WAL, and at most two connections. Each host call is
one atomic transaction with at most 64 statements and 256 scalar parameters per
statement. Results are limited to 2,000 rows per statement and 4 MiB per call;
use pagination. SQL is limited to 64 KiB, values to 1 MiB, query execution to about
five seconds, and the database to 256 MiB. Binary media belongs in uploads.
SQLite temporary sorting can use disk. These are host-service limits separate
from the guest's linear-memory budget.

A SQLite authorizer blocks ATTACH, DETACH, PRAGMA, virtual-table creation, and
filesystem/extension-loading functions. A progress handler interrupts excessive
SQL execution. Database operations cannot select another app's database.

LOKI compiles once per enabled package and reuses prelinked code. Each request
gets a fresh Store and instance; Rust globals are not persistent application
state. Use the app database and files for persistence. Stores have memory, CPU
fuel, and wall-clock limits and yield regularly to Tokio. At most eight component
requests run across the installation, additionally bounded per app. Responses
produced inside Wasm are limited to 8 MiB; use host file streaming for large data.

No host environment, stdio, ambient directories, TCP/UDP listeners, DNS, or
outgoing HTTP is inherited. WASI filesystem calls have no preopened directories.
The current capability set does not provide arbitrary networking, native library
loading, or background threads. Long-lived state and WebSockets are available
through the `realtime` capability below. Guest traps, exhausted fuel, and failed
memory growth fail the request; other requests and LOKI continue.
The runtime and host API remain part of the trusted implementation.

Disable/restart/update waits for active component requests, closes the app's SQL
pool, and releases its compiled runtime. Updates snapshot stopped app data and
retain the previous package/configuration for rollback. During initial
development, owner-initiated replacement may keep the same app version; older
versions are rejected. LOKI is at version **0.2.0** and has not had a stable release yet.

## Ranged file reads

The `loki:app/files` WIT interface reads the app's own files in pieces, so a large
upload never has to fit in guest memory. `size(source, path)` returns the size and
`read(source, path, offset, length)` returns at most 1 MiB. `source` is `upload`
(requires `uploads`), `storage` (requires `storage`) or `asset` (a declared package
file other than the component). Paths are confined exactly as for serving them.
The Rust SDK's `realtime` feature wraps these as `file_size`, `read_file` and a
`FileReader` that implements `Read + Seek`, which is enough to open a ZIP archive.

## Realtime apps

An app with the `realtime` capability gets one **long-lived instance** as well as
the usual per-request instances. LOKI keeps it alive while the app is enabled and
calls it one event at a time, so its memory persists between calls and needs no
locks. This suits game servers, chat rooms and live dashboards.

The component targets the `realtime-app` world in `loki-sdk/wit/platform.wit`. It
still exports the `wasi:http` handler for its pages, and also exports
`realtime-handler`:

| Export | When LOKI calls it |
| --- | --- |
| `on-start` | The instance started, or restarted with a fresh state |
| `on-open(connection, info)` | A WebSocket connected. `info` is JSON `{remote, user}` |
| `on-message(connection, data)` | A binary or text message arrived |
| `on-close(connection)` | The connection closed, from either side |
| `on-tick` | Every `tick_ms`, when set |

The instance answers with the `realtime` interface: `send(connection, data)`,
`send-file(connection, source, path)` and `close(connection, code, reason)`.
`send-file` streams the file from disk as consecutive 64 KiB binary messages, in
order with other sends, so large downloads never pass through guest memory. Only
the realtime instance can use connections. HTTP handlers can restart it with the
`realtime-restart` host operation, for example after its settings change.

```json
"capabilities": ["realtime"],
"limits": {
  "realtime": {
    "path": "/multiplayer",
    "max_connections": 64,
    "max_message_bytes": 65536,
    "tick_ms": 50,
    "event_fuel": 5000000
  }
}
```

| Limit | Default | Range |
| --- | --- | --- |
| `path` | `/multiplayer` | Starts with `/`; not the health path, `/logout` or `/_loki` |
| `max_connections` | 64 | 1–256 per app; LOKI allows 512 across all apps |
| `max_message_bytes` | 64 KiB | 1 KiB–2 MiB, for incoming messages |
| `tick_ms` | none | none, or 20–1000 |
| `event_fuel` | 5,000,000 | 100,000–100,000,000 per event |

`on-start` runs with the request `fuel` and `timeout_seconds`. Every other event
gets `event_fuel` and five seconds. The instance's memory is bounded by
`memory_mib`.

**Who can connect.** The WebSocket path accepts connections from **anyone who can
reach the app**, whatever its visibility, so players and other clients need no
LOKI account. The app decides who may stay, for example with its own password.
When a browser signed in to LOKI connects from the app's own origin and has access,
`info.user` says who it is; otherwise it is `null`. The dashboard says this on the
app's settings.

**Failures.** A trap, running out of fuel or time, or failed memory growth ends
the instance. LOKI closes every connection with code 1011, logs the error, and
starts a fresh instance after 1 second, doubling to at most 60 seconds while
failures continue. Connections that arrive meanwhile are closed with 1013.

**Limits on connections.** Larger incoming messages close the connection with
1009. A connection with more than 4 MiB of messages waiting to be sent (files
excluded) is closed with 1008 instead of buffering further. Closing a connection
yourself takes code 1000 or 3000–4999.

**Lifecycle.** Disabling, restarting, updating or backing up the app closes every
connection with 1001 first, so long-lived connections never hold LOKI up.
`realtime-restart` closes them with 1012. Keep anything that must survive in the
app's database or storage.

**LAN address.** The owner can give a realtime app a LAN address in its settings,
such as `0.0.0.0:4767`. LOKI then also listens there, so devices on the local
network can connect by IP, without a hostname. Only the realtime path and the
health check answer on that address; everything else is 404, and no sign-in runs
there.

## Building and verification

Bigshot is a plugin-only Rust crate using `loki-sdk`. It retains its templates,
domain models, Markdown renderer, and media allowlists. The SDK hides WIT
bindings behind ordinary Rust request/response and host-service APIs. Its guest contains no Axum, Tokio server, SQLx/SQLite, password
handling, or HTTP identity client. CSS, JavaScript, and fonts are separate package
assets served directly by LOKI.

```powershell
rustup target add wasm32-wasip2
# From the Bigshot repository:
cargo loki package
```

From the LOKI repository, run the complete real-component workflow:

```powershell
cargo xtask wasi-smoke --loki target/release/loki-server.exe --package PATH_TO_BIGSHOT.loki-app
```

The sandbox regression fixture is intentionally adversarial:

```powershell
cargo build --release --target wasm32-wasip2 --manifest-path crates/loki-server/tests/fixtures/wasi-probe/Cargo.toml --target-dir target/wasi-probe
$env:LOKI_COMPONENT_FIXTURE="$PWD/target/wasi-probe/wasm32-wasip2/release/wasi_probe.wasm"
cargo test -p loki-server --test components
```

The realtime fixture exercises long-lived state, ticks, file streaming, limits,
failure recovery, restarts, the LAN address and shutdown with real WebSockets:

```powershell
cargo build --release --target wasm32-wasip2 --manifest-path crates/loki-server/tests/fixtures/realtime-echo/Cargo.toml --target-dir target/realtime-echo
$env:LOKI_REALTIME_FIXTURE="$PWD/target/realtime-echo/wasm32-wasip2/release/realtime_echo.wasm"
cargo test -p loki-server --test realtime
```

Build this fixture locally to verify denied filesystem/network/SQL access,
withheld credentials, missing capabilities, guest traps, CPU/memory limits,
continued service after failure, and disabling without stopping LOKI.
