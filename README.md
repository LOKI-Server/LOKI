# LOKI Server

**Your services, held together.**

By [Aspenini](https://github.com/Aspenini) · [GitHub](https://github.com/Aspenini/LOKI) · [MIT license](LICENSE)

LOKI is a self-hosted Rust account server, application host, Caddy controller, and
read-only file-sharing portal. Each installation owns its accounts. Your apps
remain independent packages with their own repositories and data. Portable WASI
components are the primary app format: LOKI supplies their HTTP server, identity,
SQLite, uploads, and file streaming. Existing native process packages remain
supported for compatibility.

The SDK also provides app-scoped JSON values and generated files, stored under
`app-data/APP_ID/storage` in the configured LOKI data directory. These participate
in backups and app rollback. See the [storage API](crates/loki-sdk/README.md#persistent-app-storage)
and complete [Branch Notes example](examples/storage-notes/README.md).

## Start here

For the portable Windows bundle, extract the ZIP and run `loki-server.exe`.
Choose **Open LOKI** from its system tray icon to begin setup. Rust and the Visual
C++ runtime do not need to be installed. Keep the bundled `caddy.exe` next to LOKI.

From source, install Rust and run:

```powershell
cargo run -p loki-server -- --no-tray
```

Open the local setup URL printed in the terminal. Its one-time token is also
stored in `setup-token.txt` inside LOKI's data directory. Create the owner and
choose your canonical public URL **before importing apps**. The setup token stops
working as soon as the owner exists. On Windows, **Open LOKI** in the tray includes
the setup token automatically before initialization.

For an isolated development installation:

```powershell
cargo run -p loki-server -- --data-dir ./dev-data --bind 127.0.0.1:7070 --no-tray
```

For public hosting, choose an HTTPS origin such as `https://loki.example.com`.
Use **Server settings** to configure Caddy, and give each app or forwarded service
its own hostname. DNS and router port forwarding belong to your network: point
the hostnames at your public address and forward TCP 80/443. Diagnostics reports
DNS and local listeners; it cannot prove reachability from another network.

Release bundles include Caddy next to LOKI. Source builds use `caddy` from PATH
unless you select another executable in settings. You can run locally without
Caddy using loopback HTTP URLs.

## What you can manage

- **Accounts:** open, approval-required, or owner-created registration; profile,
  avatar, optional unverified email, password, TOTP, recovery codes, and revocable
  browser sessions. Approval-required is the default.
- **People and groups:** grant individual or group access to apps, file shares,
  and gated routes. Members join the default Members group when approved. New
  resources are restricted to the owner. App administrator access is separate
  from LOKI ownership.
- **Applications:** import a local `.loki-app`, configure it, enable/disable,
  restart, inspect logs, update, roll back, or uninstall while retaining data.
  One instance per app is supported. WASI components have restricted host APIs
  and bounded guest memory, CPU fuel, and request time. Native apps run as your
  server user and are not sandboxed.
- **Routes and services:** manage Caddy hostnames/upstreams and control existing
  Windows services, systemd units, or Homebrew services with the permissions of
  the user running LOKI. Jellyfin keeps its own authentication. The optional LOKI
  browser gate is intended for browser services, not native media clients.
- **File shares:** choose server folders remotely, browse and sort listings,
  preview safe formats, and download files. Files stay in place; browser uploads,
  edits, and guest bearer links are not part of this release.
- **Storage:** create/download backups; move data or restore a local backup into
  an empty directory and restart from the web UI. The original installation is
  retained. External shared folders keep their original absolute paths.

The green dashboard is served entirely by LOKI. It uses embedded Askama HTML,
CSS, and framework-free JavaScript, with no CDN or Node.js runtime dependency.

## Install Bigshot Voter

Install the development tool from this checkout first:

```powershell
cargo install --path crates/cargo-loki --locked
```

In the separate `bigshot-voter` repository (a sibling of this checkout):

```powershell
rustup target add wasm32-wasip2
cargo loki package
```

Import the resulting `.loki-app` in **Applications**, grant access to Members or
selected people, and enable it. The same package works on all supported LOKI
hosts. The owner automatically receives app administrator access. Without a
public hostname, Bigshot uses `http://bigshot-voter.localhost:7070` (with your
actual LOKI port), sharing LOKI's listener while retaining a separate browser
origin. For public hosting, set a hostname and enable Caddy.

Bigshot 0.5 uses LOKI exclusively. Accounts, panels, and files from older
standalone installations are not imported by this fresh-install release.

## Build a plugin

The workspace contains three crates:

- [loki-server](crates/loki-server): the server and embedded dashboard.
- [loki-sdk](crates/loki-sdk): typed HTTP handlers, identity, configuration, SQLite, forms, uploads, streaming, and optional Askama support. The SDK owns the WIT bindings.
- [cargo-loki](crates/cargo-loki): scaffolding, builds, checksummed packages, and a local development loop.

```powershell
cargo build -p loki-server -p cargo-loki
cargo install --path crates/cargo-loki --locked
rustup target add wasm32-wasip2
cargo loki new my-app --path ../my-app --sdk-path ./crates/loki-sdk
cd ../my-app
cargo loki build
cargo loki package
cargo loki dev --server ../LOKI/target/debug/loki-server.exe
```

These commands assume the checkout is named `LOKI`; omit `.exe` on Linux/macOS.
These crates are local source packages;
they have not been published to crates.io. `--sdk-path` (or `LOKI_SDK_PATH`)
selects the local SDK until publication. The default scaffold names SDK version
`0.1.0` for eventual registry use. Plugin metadata lives in `Cargo.toml`; no
app-specific packaging script or handwritten bindings are needed.

`cargo loki dev` runs a separate loopback-only installation in the plugin's
`target/loki/dev/APP_ID` directory, creates development credentials, installs the
app, and watches source/templates/assets for changes. It prints the sign-in and
URLs. Ctrl+C stops its server. It requires a built LOKI server; it can find one
next to `cargo-loki` or on PATH. Caddy and sign-in startup remain disabled in this
profile. `--port 0` chooses an available port; `--no-watch` skips rebuilding.

See the [SDK guide](crates/loki-sdk/README.md), [tool commands](crates/cargo-loki/README.md),
[WASI host API](docs/wasi.md), and [app protocol](docs/app-protocol.md).
The native [Branch example](crates/loki-server/examples/branch.rs) documents
process compatibility.

## Data and recovery

Defaults use the platform's per-user local application data directory:
`%LOCALAPPDATA%/LOKI/LOKI/data` on Windows, the equivalent Application Support directory
on macOS, and the XDG data directory on Linux. The dashboard displays the actual
resolved path. `--data-dir` or `LOKI_HOME` overrides it. A UI relocation also saves
a default-location pointer in the user's configuration directory and updates
sign-in startup when enabled.

SQLite uses WAL and a small connection pool. Identity signing keys, account
credentials, app credentials, packages, avatars, Caddy state, and app data stay
under this directory. Backups contain private credentials: keep copies private.

Stop the server before owner recovery:

```powershell
loki-server --data-dir PATH recover-owner
```

This revokes the owner's sessions and TOTP and prints a one-time recovery link.
Ordinary member resets are issued from **People & groups**, expire after 24 hours,
and keep the member's TOTP enabled. Reset links store the token in the URL fragment
so it does not appear in HTTP request logs or referrers.

Offline restore preserves the current installation by requiring an empty target:

```powershell
loki-server --data-dir NEW_EMPTY_DIRECTORY restore BACKUP.zip
loki-server --data-dir NEW_EMPTY_DIRECTORY
```

Automatic startup is per-user sign-in, not a privileged system service. Windows
uses the current user's Run key, Linux an XDG autostart entry, and macOS a
LaunchAgent. Existing OS services may require independently configured access.

When adopting an existing Caddy, preview and back up its live configuration,
then stop Patty or any other configuration writer. LOKI preserves that base and
adds its own routes. Never expose Caddy's management endpoint publicly.

To replace an existing instance's site configuration, select Existing Caddy
instance and leave Caddy base JSON blank. Preview and save to apply only LOKI's
routes. Existing mode preserves the running instance's administration settings,
including its default listener, so a reload does not rebind its management port.

## Development and verification

```powershell
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
node --check crates/loki-server/static/app.js
node --test crates/loki-server/tests/setup-ui.cjs
python scripts/sdk-smoke.py --tool target/debug/cargo-loki.exe --server target/debug/loki-server.exe
```

The Caddy integration test uses the local Caddy CLI, or `CADDY` if set. Tests that
use mock identity/Caddy APIs need loopback networking. Run verification commands
locally; this repository has no CI workflows.

Run the real WASI workflow after building LOKI and packaging Bigshot:

```powershell
python scripts/wasi-smoke.py --loki target/debug/loki-server.exe --package ../bigshot-voter/target/loki/bigshot-voter-0.5.0.loki-app
```

This uses disposable data under `target/wasi-smoke-*`, not installed services or real
accounts. It exercises component readiness, host-managed sign-in, voting,
permission revocation, failed-update rollback, and backup/resume.

For the first-run setup and data-relocation regression check:

```powershell
python scripts/setup-smoke.py target/release/loki-server.exe
```

If a setup submission was interrupted after the owner was saved, open `/login`
and use the credentials you chose. Setup links stop working once an owner exists.

Measure the core separately from apps and Caddy:

```powershell
cargo build --release
python scripts/benchmark.py target/release/loki-server.exe
```

The engineering target is less than 75 MiB idle resident memory and less than
1% of one CPU core. See [verification notes](docs/verification.md) for measured
results and what has actually been exercised on this machine.
