# cargo-loki

Rust development tools for LOKI plugins. No Python or handwritten WIT bindings.

```sh
cargo install cargo-loki --locked
cargo loki new hello
cd hello
rustup target add wasm32-wasip2
cargo loki check
cargo loki build
cargo loki package
cargo loki dev --server /path/to/loki-server
```

`new` creates a plain Rust plugin that depends on `loki-sdk` from crates.io, at
the version matching the tool. To work against a local copy of the SDK, such as a
LOKI checkout, pass its path with `--sdk-path` (or `LOKI_SDK_PATH`). Developer
bundles created with `cargo xtask bundle --developer-tools` include the SDK
sources at `crates/loki-sdk`. The runtime bundle and Windows installer contain no
developer tools.

`build` and `check` target WASIp2 automatically. `package` defaults to release,
reads `[package.metadata.loki]`, includes declared assets, validates the component,
and writes a checksummed ZIP to `target/loki/ID-VERSION.loki-app`. Use `--output`
for another location, `--debug` for a debug package, or `--no-build` to package an
existing artifact. Cargo workspaces use `-p NAME`; `--manifest-path`, `--offline`,
and `--locked` are supported.

For example, `[package.metadata.loki]` can declare
`assets = ["static", "templates/help.txt"]`, `capabilities = ["database", "forms"]`,
and `roles = ["admin"]`. See the [SDK guide](../loki-sdk/README.md) for the full
Rust workflow. New projects are independent Cargo workspaces; remove their empty
`[workspace]` table to explicitly add them to an existing workspace.

`dev` starts the specified LOKI server with private development data under the
project target directory, creates a local developer account, imports/enables the
plugin, and prints its URL and development credentials. It watches source,
templates, metadata, and declared assets; a successful rebuild replaces the
plugin without changing its version. Build or readiness failures preserve the
last working plugin. Use `--no-watch` for a fixed preview and `--port` to choose
the loopback port. Ctrl+C stops the child server. It never deploys into an existing
LOKI installation, enables startup, or configures Caddy/public exposure.
The assigned port is saved with the profile to keep its identity issuer stable;
use `--port 0` again to resume it. To start with a fresh profile and another port,
use a different `CARGO_TARGET_DIR`.
