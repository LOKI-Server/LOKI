# Verification record

Verified on October 30, 2026, for LOKI **0.1.0** with the Rust workspace, `loki-sdk`, `cargo-loki`,
and Bigshot Voter's SDK-based portable 0.5.0 package. LOKI remains at 0.1.0 during initial
development; this work does not introduce a release-version bump.

## Reference environment and measured performance

Windows 11 build 26200, Intel Core i7-13700F, Rust 1.99.1. Release builds use thin
LTO and a statically linked MSVC runtime. Caddy and the tray were disabled during
measurement. Each installation had completed owner setup and sign-in.

| Configuration | Resident RAM | CPU, percent of one core | Sample |
| --- | --- | --- | --- |
| LOKI, no apps | 12.88 MiB | 0.078% measured | 20.09 seconds |
| LOKI including enabled Bigshot WASI component | 53.87 MiB total | 0.000% measured | 20.04 seconds |

CPU values are rounded process-time measurements, not a claim of literally zero
work. These are short idle samples, not a sustained load/concurrency benchmark.
The second row includes Wasmtime, compiled Bigshot code, and its SQLite pool
inside the LOKI process. There is no separate Bigshot process or HTTP listener.

A **128 MiB Bigshot media download** streamed directly from host-managed app
storage with **0.47 MiB** sampled resident growth. The WASI smoke task rejects
growth of 16 MiB or more. Uploads and downloads use host streaming rather than
copying complete media files into guest memory.

The Bigshot `.loki-app` is **295,141 bytes**, including its Wasm,
CSS, JavaScript, fonts, and font license. The prior native package was 3,783,561
bytes. Its component code is 658,330 bytes before ZIP compression. Package-size
savings do not imply the Wasmtime host runtime has no cost; it is included in
LOKI and in the total enabled-app memory measurement above.

Reproduce from the LOKI repository root:

```powershell
cargo xtask benchmark target/release/loki-server.exe
cargo xtask benchmark target/release/loki-server.exe --package target/dist/bigshot-voter-0.5.0-wasi.loki-app
```

These tasks use disposable directories below `target/`, never enable sign-in startup,
and stop their own test processes. JSON reports are `target/benchmark-last.json`
and `target/benchmark-wasi-last.json`.

## Automated checks

**65 Rust tests pass:** 29 server tests, 10 SDK tests, 5 tooling tests, and
21 Bigshot domain/UI tests. Four Node setup-UI tests also pass. Both repositories
pass formatting and Clippy with warnings denied, including Bigshot's actual
`wasm32-wasip2` guest target. LOKI's embedded JavaScript passes `node --check`.
The SDK's standalone Cargo package also passes `cargo package` verification,
including its embedded WIT. No crate has been published to a registry.

SDK tests cover current identity/roles, typed configuration, bound SQL parameters
and row decoding, numeric/Unicode forms, host-streamed uploads, route decoding,
header injection rejection, cryptographic IDs, and scoped test hosts. Tool tests
cover independent scaffolding, checksummed Unicode assets, malformed components,
unsafe/duplicate paths, junction rejection, workspace member selection, and preservation of a prior package
when validation fails.

`cargo xtask sdk-smoke --tool target/release/cargo-loki.exe --server target/release/loki-server.exe --offline`
creates an SDK-only project, checks/builds/packages it, starts a private dev
installation, signs in through the real host, and serves its assets. It verifies
a watched rebuild, a failed build retaining the last working component, a second
run with the saved identity origin, and Ctrl+C stopping the child server. The
Windows run passed. The task can also be run locally on Linux and macOS.

Bigshot is now plugin-only. Its old standalone-server/account tests were removed
with that implementation; its media, Markdown, voting-view tests and the live
hosted voting workflow remain. App package metadata is in Cargo.toml and all
packaging is handled by `cargo loki`.

LOKI's checks cover owner races, CSRF, registration, grants, credential epochs,
TOTP replay/recovery, OIDC PKCE/callback/code/refresh rejection, package paths,
file permissions/ranges/active formats/Unicode/Windows junctions, and actual
Caddy validation/apply/rollback with the official 2.11.4 binary.

A real compiled WASI probe verifies denied ambient filesystem and outgoing HTTP,
blocked SQLite ATTACH/PRAGMA/extension loading, missing capabilities, withheld
cookies/Authorization, stripped guest Set-Cookie, guest traps, CPU fuel and memory
limits, successful later requests, and disabling without stopping LOKI. SQLite
unit tests also verify atomic rollback and oversized-value rejection. Local
checks built and supplied this fixture explicitly.

Windows import-table inspection confirms that LOKI does not require a separately
installed Visual C++ runtime. Wasmtime is embedded; there is no external runtime
to install for Bigshot.

## App storage verification

The host and SDK now provide app-scoped JSON values and generated files, under
`app-data/APP_ID/storage`. LOKI stays at 0.1.0. The independent Branch Notes
example demonstrates per-user preferences, SQLite records, and generated text
exports, using SDK APIs without raw bindings or ambient filesystem access.

Seven host tests cover reopening, Unicode and case-sensitive names, traversal and
device-path rejection, size/quota/entry limits, bounded listing cursors, concurrent
writers, junction escapes, and waiting for filesystem work before snapshots.
Three additional SDK tests cover typed/missing/null JSON values, bounded generated
files and host-streamed response markers, and structured/legacy host errors.
The real WASI sandbox fixture checks storage capability admission, generated-file
streaming, active-format download disposition, persistence across disable/enable,
and preserving a later guest denial instead of serving an earlier file marker.

`cargo xtask storage-smoke --loki target/release/loki-server.exe --package target/dist/storage-notes-0.1.0.loki-app`
passed with actual component execution on Windows: sign-in, private per-user
preferences/SQL notes/exports, missing-file HTTP 404, byte ranges, app restart,
failed-update rollback, stopped-app backup, restoration into a fresh data
directory, and immediate download denial after grant revocation. The task
waits for the restored app's readiness separately from the core listener.

Run formatting, WASI Clippy, package generation, and this live workflow locally.
Only Windows has been executed here. Storage quotas and limits are documented
in the SDK guide.

## Real release workflow

`cargo xtask wasi-smoke --loki target/release/loki-server.exe --package target/dist/bigshot-voter-0.5.0-wasi.loki-app`
completed the following with the release binaries and disposable data:

1. Import Bigshot's portable component and pass its initialization/readiness check.
2. Complete host-managed authorization-code/PKCE sign-in across separate origins.
3. Create a panel, upload an entry and optional source file, download both, and
   serve an individual byte range.
4. Reject voting for one's own entry and deny member access to admin controls.
5. Approve/grant a member, sign in, vote, close the panel and reject changes,
   reopen it, and update the same vote without inserting a duplicate.
6. Revoke access and deny existing-session panel and direct media requests.
7. Attempt an update with an invalid health endpoint and new configuration field;
   restore the prior component, original configuration, and vote.
8. Stream a 128 MiB file from Bigshot's host-managed uploads with bounded memory.
9. Back up stopped component data, resume, import/validate the backup, restore
   into a new directory, restart, and verify the vote survives.

The source installation is retained after restore. External file-share folders
are not copied into backups. Test location pointers are isolated from real setup.

## Browser and platform scope

Windows startup migration has regression coverage for explicit replacement
confirmation, cancellation before owner/settings changes, stale confirmation
rejection, restored enabled settings, and rollback after failed saves. Native
registry round trips use random `Software\LOKI\Tests` subkeys rather than the
Windows Run key. The real sign-in startup registration is not modified by tests.
Dashboard tests cover confirm/cancel/dismiss behavior and escaped file paths.

The Rust `cargo xtask` automation was exercised on Windows on 2026-10-31 with
fresh debug server/tool builds. Setup relocation, SDK scaffolding/check/package,
watched rebuilds and failed-build preservation, Ctrl+C cleanup across two preview
runs, Branch Notes storage/rollback/restore, and the complete Bigshot workflow
all passed. The 128 MiB Bigshot stream showed about 0.14 MiB resident growth.
Rust developer-bundle generation downloaded and verified the official Caddy archive,
included the standalone SDK sources and example, and rejected overwriting a
nonempty output directory. Both benchmark modes ran with two-second debug
samples to check the measurement code; these short checks do not replace the
release performance measurements above. The task unit tests and Clippy passed.

The runtime-only default was subsequently verified to contain exactly the LOKI
and Caddy executables, LOKI's license, and Caddy's license/source notices. It
requires neither a cargo-loki executable nor an SDK archive. Developer files
are opt-in with `--developer-tools`; the Windows installer has an explicit
runtime file list. The runtime inventory regression test and Clippy passed.

The actual browser completed login from the LOKI dashboard to
`bigshot-voter.localhost` on the same listener port. The dashboard showed the
running WASI component and its controls. Bigshot's existing voting interface,
fonts, styles, and panel data loaded correctly from host-served assets.

- [WASI app management](screenshots/wasi-management.jpg)
- [Bigshot running as a component](screenshots/bigshot-wasi.jpg)

CI workflows have been removed from LOKI and Bigshot. `cargo xtask` checks and
regression tests remain available for local runs. Only Windows was executed
locally. Native Linux/macOS execution, real sign-in startup/service-control
changes, and public DNS/router/TLS issuance were not verified here. Testing did
not modify existing Patty/Caddy installations.

See [WASI host API and limits](wasi.md) for the exact execution and capability
contract. WASI guest memory limits do not replace the separately documented SQL,
HTTP, upload, and runtime limits; runtime and host-service correctness remain
part of the trusted implementation.
