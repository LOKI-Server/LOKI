# loki-sdk

Typed Rust APIs for portable LOKI plugins. The SDK contains the WASI bindings;
plugins implement plain request handlers and call `loki_sdk::export!(handle)`.
LOKI provides HTTP, identity, SQLite, uploads, and streaming at runtime.

```rust,ignore
use loki_sdk::{Request, Response, Result, Router};
fn hello(request: Request) -> Result<Response> {
    Response::json(&request.context().user)
}
fn health(_: Request) -> Result<Response> { Ok(Response::text("ready")) }
fn handle(request: Request) -> Result<Response> {
    Router::new().get("/", hello).get("/health", health).handle(request)
}
loki_sdk::export!(handle);
```

Use `Context::require_user` / `require_role`, `Database::fetch_all` /
`fetch_one` / `execute` / `transaction` with `params![]`, typed `Form` and
`Upload::retain`, `StoredFile::download` / `inline`, and `asset` / `log`.
The `templates` feature adds `Response::render` for Askama templates. The
`testing` feature supplies a thread-local fake host and synthetic requests for
native unit tests. SDK services never start their own server or open SQLite.

Build with `cargo loki build`; package with `cargo loki package`. A plugin's
Cargo metadata declares capabilities, assets, roles, settings, and limits. Missing
capabilities and permissions are enforced by LOKI even when the SDK is bypassed.

The `manifest` feature exposes shared package types/validation for native tooling
without pulling in guest bindings. All LOKI crates share one version, currently 0.2.0.

## Persistent app storage

Add `"storage"` to `[package.metadata.loki].capabilities`. Storage is managed by
LOKI under `<LOKI data directory>/app-data/APP_ID/storage`, separate from the
executable/package and uploaded media. It survives restarts and is included in
app update snapshots, rollback, and backups. The SDK exposes logical names, never
the host directory. Ordinary `std::fs` calls remain unavailable in the guest.

```rust,ignore
use loki_sdk::{Storage, ErrorKind};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Preferences { gold: bool }

Storage.set_json("preferences", &Preferences { gold: true })?;
let preferences: Option<Preferences> = Storage.get_json("preferences")?;
let file = Storage.write("reports/hello.txt", b"Hello, multiverse!")?;
let text = file.read_text()?;
let page = Storage.list_files("reports/", None)?;
// Pass page.next.as_deref() as the next request's cursor.
let usage = Storage.usage()?;
// Return the marker directly: LOKI streams its body, including range requests.
return file.download("hello.txt");
```

`request.context().storage()` returns the same app-scoped handle. Missing JSON
keys return `None`; stored JSON `null` stays distinct from an absent key.
`delete_json` and `DataFile::delete` return whether an item existed. Files also
support `info`, bounded `read`, `read_text`, and safe `inline` responses. Active
formats such as HTML and SVG are downloaded rather than rendered inline.

Limits per app: **64 MiB total**, **4,096 values/files**, **128 KiB per encoded
JSON value**, and **1 MiB per generated-file write or buffered read**. Existing
large incoming media should use host uploads. File listings are sorted, flat
pages of at most 100 logical paths. Cursors do not give a snapshot across writes.
`usage()` reports the limits and committed byte/entry counts. Writes are atomic
replacements and are serialized across concurrent requests; rejected writes keep
the previous content. Atomic replacement temporarily needs room for both copies
on disk. In-flight filesystem work drains before a stopped-app snapshot.

Keys and file paths are 1–120 UTF-8 bytes; keys have no slashes and file paths
may contain logical `/` separators. Traversal, absolute/device paths, backslashes,
and reserved Windows names are rejected. Names are encoded internally so case
and Unicode behave consistently on Windows, Linux, and macOS. Do not manipulate
the encoded files directly. Storage is shared by an app's users: include stable
user IDs in keys/paths and enforce ownership in your handlers for private data.

Match `error.kind()` on `ErrorKind::NotFound`, `PermissionDenied`, `InvalidInput`,
`Conflict`, `LimitExceeded`, `Unavailable`, `Unauthenticated`, or `HostFailure`.
Host errors retain their HTTP status without requiring message parsing. Old
unstructured host errors remain supported as `HostFailure`.

The complete [Branch Notes example](../../examples/storage-notes/README.md)
demonstrates identity, per-user saved preferences, SQL notes, CSRF-protected forms,
and downloadable generated exports.

## Project metadata

`cargo loki new` supplies the library target and a minimal handler. Extend its
Cargo.toml to opt into the services you need:

```toml
[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
loki-sdk = { version = "0.2.0", features = ["templates"] }
serde = { version = "1", features = ["derive"] }

[package.metadata.loki]
id = "my-app"
name = "My app"
health = "/health"
roles = ["admin"]
capabilities = ["database", "forms", "uploads"]
assets = ["static"]

[[package.metadata.loki.fields]]
key = "greeting"
label = "Greeting"
kind = "text"
default = "Hello"
```

Before registry publication, add `path = "PATH_TO_LOKI/crates/loki-sdk"` to the
SDK dependency. `cargo loki new --sdk-path PATH` does this for you. These crates
have not been published. App versions come from `[package].version`.

`package` builds a release `wasm32-wasip2` component and includes declared assets
with generated checksums. Asset paths stay relative to the plugin directory;
symlinks and junctions are rejected. `cargo loki dev` runs an isolated LOKI
installation with watched rebuilding and persistent local app data. No binding
generation command, separate HTTP server, SQLite library, or Python packager is
needed in an app repository.

## Identity, queries, and forms

```rust,ignore
use loki_sdk::{Database, Request, Response, Result, params};
use serde::Deserialize;

#[derive(Deserialize)]
struct Note { id: String, title: String }

fn notes(request: Request) -> Result<Response> {
    let user = request.context().require_user()?;
    let notes: Vec<Note> = Database.fetch_all(
        "SELECT id,title FROM notes WHERE owner=? ORDER BY title LIMIT 100",
        params![user.id],
    )?;
    // Serialize your own view model or render an Askama template here.
    Ok(Response::text(format!("{} notes", notes.len())))
}

fn create_note(request: Request) -> Result<Response> {
    let user = request.context().require_user()?;
    let form = request.form()?;
    let title = form.required("title")?.trim();
    Database.execute(
        "INSERT INTO notes(id,owner,title) VALUES(?,?,?)",
        params![loki_sdk::random_id(), user.id, title],
    )?;
    Response::redirect("/notes")
}
```

Initialize your schema idempotently from the health handler with
`Database::transaction(&[Statement::new(SQL, params![])?, ...])`. The health
request has no signed-in user. Queries use bound scalar parameters and typed
row decoding; use pagination for large result sets. Stable `user.id` identifies
a person; profile fields can change. `require_role("admin")` checks current app
roles rather than treating every member as an administrator.

Every browser form includes a hidden `csrf_token` input with
`request.context().csrf_token()`. LOKI parses and validates opted-in forms
before your handler. `Form::decode<T>()` decodes string/numeric fields with Serde.
For multipart uploads, validate `form.required_file("file")?` metadata, then call
`retain("relative/path")`. `StoredFile::download` and `inline` ask LOKI to stream
the file, including range requests. These calls return an empty response marker;
return that response directly. `asset("static/style.css")` works the same way.
File bytes stay outside Wasm memory. Roll back retained uploads yourself if a
later application database operation fails.

Each request gets a fresh guest instance. Store persistent state in the app
database/uploads, and declare any needed limits in Cargo metadata. Arbitrary
outbound networking, native library loading, and background threads are not
provided by the current host. Use `request.body(MAX)` / `json(MAX)` only for
bounded small bodies rather than reading multipart media into memory.

## Native tests

Enable `loki-sdk`'s `testing` feature in dev-dependencies. Use
`testing::request(Method::Get, "/path", context)` to exercise a plain handler.
`testing::with_host(fake, test)` temporarily installs a thread-local host closure
whose inputs and outputs follow the [host protocol](../../docs/wasi.md).
The previous fake is restored after the test, including after a panic. This
allows checking SQL parameter binding and upload decisions without Wasmtime;
the real LOKI host still enforces access and capability limits in production.
