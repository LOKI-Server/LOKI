//! Builds of the WASI test components, one per `loki-sdk` LOKI still supports.
use std::path::Path;

/// One build of a test component, and the `loki-sdk` version it was built with.
pub struct Build {
    pub sdk: String,
    pub wasm: Vec<u8>,
}

/// A fresh build from the `variable` path when it is set. Otherwise every frozen
/// build in `tests/fixtures/frozen/sdk-VERSION/`. Frozen builds are never rebuilt,
/// so passing tests prove that apps made with those SDKs still run on this LOKI.
pub fn builds(file: &str, variable: &str) -> Vec<Build> {
    if let Ok(path) = std::env::var(variable) {
        return vec![Build {
            sdk: env!("CARGO_PKG_VERSION").into(),
            wasm: std::fs::read(path).unwrap(),
        }];
    }
    let frozen = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/frozen");
    let mut builds: Vec<Build> = std::fs::read_dir(&frozen)
        .unwrap()
        .filter_map(|entry| {
            let dir = entry.ok()?.path();
            let sdk = dir.file_name()?.to_str()?.strip_prefix("sdk-")?.to_string();
            let wasm = std::fs::read(dir.join(file)).ok()?;
            Some(Build { sdk, wasm })
        })
        .collect();
    builds.sort_by_cached_key(|b| semver::Version::parse(&b.sdk).unwrap());
    assert!(
        !builds.is_empty(),
        "No frozen builds of {file} in {}",
        frozen.display()
    );
    builds
}
