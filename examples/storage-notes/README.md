# Branch Notes

A complete Rust SDK example: LOKI sign-in, per-user JSON preferences, SQLite
notes, safe HTML forms, and generated text exports streamed by the host.

From the LOKI checkout:

```sh
cargo build -p loki-server -p cargo-loki
cargo install --path crates/cargo-loki --locked
rustup target add wasm32-wasip2
cargo loki dev --manifest-path examples/storage-notes/Cargo.toml --server target/debug/loki-server.exe
```

Omit `.exe` on Linux/macOS. The development profile is separate from installed
LOKI and its sign-in credentials are printed in the terminal. To install in a
normal LOKI instance, run `cargo loki package --manifest-path
examples/storage-notes/Cargo.toml`, import the resulting archive, and grant access.
Enable personal storage for Branch Notes from **Applications → Manage** before
exporting a note in either installation.

The health handler initializes the app schema. Approved users see their own
records; every SQL statement includes their stable user ID. Preferences use
`Storage::set_json` / `get_json` with user-specific keys. Exports use
`PersonalStorage.write` and `PersonalStorage.download`. LOKI scopes that file
to the signed-in user and the Branch Notes app.

Preferences and notes live under `app-data/storage-notes`. The export is a
user-owned file included in **Account → Export my data**. All three survive restarts
and participate in installation backups. App rollback restores the app's own
preferences and database.

End-to-end verification (after packaging, from the LOKI checkout):

```sh
cargo xtask storage-smoke --loki target/debug/loki-server.exe --package examples/storage-notes/target/loki/storage-notes-0.1.0.loki-app
```

This uses disposable accounts/directories, verifies separate users' data, range
downloads, missing-file errors, restart, failed-update rollback, backup/restore,
and immediate access revocation. It does not modify an installed LOKI instance.
