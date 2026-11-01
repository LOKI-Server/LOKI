# Frozen test apps

Each `sdk-VERSION` folder holds the test apps built with that `loki-sdk` release,
committed once and **never rebuilt**. The `components` and `realtime` tests run
every folder here, so a change to LOKI that would break apps made with an
older SDK fails the tests before it ships.

| File | Built from |
| --- | --- |
| `wasi_probe.wasm` | [`../wasi-probe`](../wasi-probe) |
| `realtime_echo.wasm` | [`../realtime-echo`](../realtime-echo) |

- **Releasing an SDK with new host features:** add a folder for it with fresh
  release builds (`cargo build --release --target wasm32-wasip2` in each test
  app). Tests for those features should check the build's SDK version, since
  older frozen builds don't have them.
- **Ending support for an SDK:** delete its folder in the same change that raises
  `OLDEST_SDK` in `src/apps.rs`, and say so in the release notes.
- **Testing a fresh build** instead: set `LOKI_COMPONENT_FIXTURE` or
  `LOKI_REALTIME_FIXTURE` to its `.wasm`.
