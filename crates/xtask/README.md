# LOKI repository tasks

Run `cargo xtask --help` from this checkout. Cargo builds this private workspace
crate automatically; Python is not required. Tasks use native process and file
APIs and pass arguments directly, without invoking a shell.

| Command | Purpose |
| --- | --- |
| `cargo xtask bundle` | Assemble LOKI, official Caddy, and license/source notices into a runtime directory. |
| `cargo xtask benchmark SERVER` | Measure an initialized server's idle resident memory and one-core CPU usage. With `--package APP.loki-app --requests N`, also measure the app's request rate and latency (`--path`, `--concurrency`). |
| `cargo xtask setup-smoke SERVER` | Exercise owner setup, local cookies with an HTTPS public URL, data relocation, and restart. |
| `cargo xtask sdk-smoke --tool TOOL --server SERVER --offline` | Scaffold/check/package an SDK plugin; verify preview SSO, assets, watched rebuilds, failed-build preservation, stable ports, and Ctrl+C cleanup. |
| `cargo xtask storage-smoke --loki SERVER --package PACKAGE` | Exercise Branch Notes JSON/SQL/files, user isolation, ranges, rollback, and backup/restore. |
| `cargo xtask wasi-smoke --loki SERVER --package PACKAGE` | Exercise Bigshot sign-in, voting, permissions, media, failed upgrades, 128 MiB streaming, and web restore/restart. |

`smoke` is an alias for `wasi-smoke`. The smoke tasks and benchmarks create
disposable installations under the checkout's `target/`, with random loopback
ports, isolated location pointers, startup disabled, and Caddy disabled. Their
test data/logs are retained for diagnosis. Child processes are stopped on errors
and normal completion. `wasi-smoke --keep-running` deliberately retains a
successful temporary preview and prints its PID, location, and test credentials.

Smoke tests run the actual server executable and need loopback networking and
normal filesystem access. SDK smoke also requires the `wasm32-wasip2` Rust target
and Cargo dependencies (cached when using `--offline`). Node remains necessary
for the dashboard's JavaScript tests. Native process measurements support
Windows/Linux/macOS; native verification should be performed on each OS.

## Portable bundle and Windows installer

From the repository root:

```powershell
cargo build --release --locked -p loki-server
cargo xtask bundle
ISCC.exe packaging/loki.iss
```

Omit `.exe` in server/tool paths on Linux/macOS. Bundling uses the host OS and
architecture and does not cross-compile binaries. `--binary` selects the built
server (default `target/release/loki-server[.exe]`). The runtime bundle and Windows
installer contain only LOKI, Caddy, and license/source notices. Neither requires
building or packaging the SDK/tool. Stop a development server before rebuilding
its executable if Windows has it locked.

For a separate developer bundle, build `cargo-loki` alongside the server, run
`cargo package -p loki-sdk`, and use
`cargo xtask bundle --developer-tools --output target/developer-bundle`. This
adds the tool, SDK sources/archive, developer docs, and example. The Windows
installer uses an explicit runtime file list even if pointed at such a bundle.
The SDK and cargo-loki remain local source packages until published to crates.io.

Repeated runs automatically replace the previous generated bundle, removing
obsolete files. The new bundle is assembled and verified in a temporary sibling
directory first; a failed build leaves the previous bundle available. Output
links/junctions, repository sources, input locations, and unrelated nonempty
folders are rejected. File links/reparse points are also rejected in sources
and SDK archives. Close programs running from the output if Windows prevents
replacement.
The installer reads the default `target/bundle` directory. If you select a
custom `--output`, update `BundleDir` in `packaging/loki.iss` to match it.

Caddy downloads come from its official GitHub release and are checked against
that release's SHA-256/SHA-512 checksums before extraction. The default is
`--caddy-version 2.11.4`. For an offline build, supply both
`--caddy-archive PATH` and `--caddy-checksums PATH` from that official release;
checksum verification still applies. Building the installer additionally needs
Inno Setup, which is separate from the Rust toolchain.
