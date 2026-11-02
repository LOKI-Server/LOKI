use super::project::{Project, absolute};
use anyhow::{Context, Result, ensure};
use loki_sdk::manifest::{Manifest, safe_relative, validate_manifest};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub fn create(project: &Project, component: &Path, output: Option<&Path>) -> Result<PathBuf> {
    let mut metadata = project.config()?;
    let assets = metadata
        .as_object_mut()
        .unwrap()
        .remove("assets")
        .unwrap_or_else(|| json!([]));
    let assets = assets
        .as_array()
        .context("metadata.loki.assets must be an array of relative files/directories")?;
    let entry = component
        .file_name()
        .and_then(|s| s.to_str())
        .context("Invalid component filename")?
        .to_string();
    let mut files = BTreeMap::new();
    files.insert(entry.clone(), component.to_path_buf());
    for asset in assets {
        let path = asset.as_str().context("Asset paths must be strings")?;
        ensure!(safe_relative(path), "Unsafe asset path: {path}");
        collect(&project.root, Path::new(path), &mut files)?;
    }
    ensure!(files.len() < 2048, "Package exceeds 2,048 entries");
    let mut code = Vec::new();
    std::fs::File::open(component)?
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut code)?;
    ensure!(code.len() <= 32 * 1024 * 1024, "Component exceeds 32 MiB");
    ensure!(
        code.starts_with(b"\0asm\x0d\0\x01\0"),
        "Expected a wasm32-wasip2 component, not a native executable or core Wasm module"
    );
    wasmparser::Validator::new()
        .validate_all(&code)
        .map_err(|e| anyhow::anyhow!("Invalid WebAssembly component: {e}"))?;
    let object = metadata.as_object_mut().unwrap();
    object.entry("id").or_insert(json!(project.app_id()?));
    object
        .entry("name")
        .or_insert_with(|| json!(project.package.name));
    object
        .entry("description")
        .or_insert_with(|| json!(project.package.description.clone().unwrap_or_default()));
    object.insert("protocol".into(), json!(1));
    object.insert("runtime".into(), json!("wasi"));
    object.insert("target".into(), json!("wasm32-wasip2"));
    object.insert("version".into(), json!(project.package.version));
    object.insert("entry".into(), json!(entry));
    object.entry("health").or_insert_with(|| json!("/health"));
    let sdk = project.sdk.as_ref().context(
        "The plugin must depend on loki-sdk. Its package records the SDK version so LOKI can tell whether it can run the app.",
    )?;
    object.insert(
        "built_with".into(),
        json!({"sdk":sdk,"cargo_loki":env!("CARGO_PKG_VERSION")}),
    );
    let mut total = 0u64;
    let mut checksums = BTreeMap::new();
    for (name, path) in &files {
        ensure!(
            safe_relative(name) && name != "manifest.json",
            "Unsafe or reserved package path: {name}"
        );
        let size = std::fs::metadata(path)?.len();
        total = total.checked_add(size).context("Package size overflow")?;
        ensure!(
            total <= 512 * 1024 * 1024,
            "Expanded package exceeds 512 MiB"
        );
        let mut input = std::fs::File::open(path)?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        let mut read = 0u64;
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            read += count as u64;
            ensure!(
                read <= size,
                "Package input grew during hashing: {name}; retry the command"
            );
            hash.update(&buffer[..count]);
        }
        checksums.insert(name.clone(), hex::encode(hash.finalize()));
    }
    object.insert("files".into(), serde_json::to_value(checksums)?);
    let manifest: Manifest = serde_json::from_value(metadata)?;
    validate_manifest(&manifest)?;
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    ensure!(bytes.len() <= 131072, "Manifest exceeds 128 KiB");
    let output = absolute(
        output.unwrap_or(
            &project
                .target
                .join("loki")
                .join(format!("{}-{}.loki-app", manifest.id, manifest.version)),
        ),
    )?;
    ensure!(
        !files
            .values()
            .any(|source| absolute(source).is_ok_and(|p| p == output)),
        "Output would overwrite a package input"
    );
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = output.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut archive = zip::ZipWriter::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?,
        );
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o644);
        archive.start_file("manifest.json", options)?;
        archive.write_all(&bytes)?;
        for (name, path) in files {
            archive.start_file(&name, options)?;
            let mut file = std::fs::File::open(path)?;
            let mut hash = Sha256::new();
            let mut buffer = [0; 65536];
            let mut size = 0u64;
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                size += count as u64;
                ensure!(
                    size <= 512 * 1024 * 1024,
                    "Asset grew beyond package limits"
                );
                hash.update(&buffer[..count]);
                archive.write_all(&buffer[..count])?;
            }
            ensure!(
                manifest.files.get(&name) == Some(&hex::encode(hash.finalize())),
                "Package input changed during packaging: {name}; retry the command"
            );
        }
        let file = archive.finish()?;
        file.sync_all()?;
        drop(file);
        // rename atomically replaces on Unix; Windows needs the previous output removed.
        #[cfg(windows)]
        if output.exists() {
            std::fs::remove_file(&output)?;
        }
        std::fs::rename(&temporary, &output).with_context(|| {
            format!(
                "Cannot rename {} to {}",
                temporary.display(),
                output.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    Ok(output)
}
fn collect(root: &Path, relative: &Path, files: &mut BTreeMap<String, PathBuf>) -> Result<()> {
    let path = root.join(relative);
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("Missing package asset: {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "Symlinks cannot be packaged: {}",
        path.display()
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "Reparse points cannot be packaged: {}",
            path.display()
        );
    }
    let canonical = dunce::canonicalize(&path)?;
    ensure!(
        canonical.starts_with(dunce::canonicalize(root)?),
        "Asset escapes the plugin directory"
    );
    if metadata.is_dir() {
        let mut entries = std::fs::read_dir(&path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            collect(root, &relative.join(entry.file_name()), files)?;
        }
    } else {
        ensure!(metadata.is_file(), "Only regular assets can be packaged");
        let name = relative
            .to_str()
            .context("Asset names must be UTF-8")?
            .replace('\\', "/");
        ensure!(safe_relative(&name), "Unsafe asset path: {name}");
        ensure!(
            files.insert(name, path).is_none(),
            "Duplicate package asset"
        );
    }
    ensure!(files.len() < 2048, "Too many package entries");
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::project::{CargoPackage, Target};
    pub(crate) fn fixture() -> (tempfile::TempDir, Project, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::fs::create_dir(root.join("static")).unwrap();
        std::fs::write(root.join("static/枝.txt"), "green & gold").unwrap();
        let wasm = root.join("demo.wasm");
        // An empty, structurally valid component tests the archive layer. The live
        // SDK preview additionally verifies an actual incoming-handler export.
        std::fs::write(&wasm, b"\0asm\x0d\0\x01\0").unwrap();
        let project = Project {
            package: CargoPackage {
                id: "demo".into(),
                name: "demo".into(),
                version: "0.1.0".into(),
                description: None,
                manifest_path: root.join("Cargo.toml"),
                metadata: json!({"loki":{"assets":["static"]}}),
                targets: vec![Target {
                    name: "demo".into(),
                    crate_types: vec!["cdylib".into()],
                }],
            },
            root: root.clone(),
            target: root.join("target"),
            sdk: Some("0.3.0".into()),
        };
        (temp, project, wasm)
    }
    #[test]
    fn archive_has_matching_manifest_and_unicode_assets() {
        let (_temp, mut project, wasm) = fixture();
        project.package.name = "rust_style".into();
        let output = create(&project, &wasm, None).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(output).unwrap()).unwrap();
        let manifest: Manifest =
            serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap();
        assert_eq!(manifest.runtime, "wasi");
        assert_eq!(manifest.id, "rust-style");
        assert_eq!(manifest.version, "0.1.0");
        let built_with = manifest.built_with.as_ref().unwrap();
        assert_eq!(built_with.sdk, "0.3.0");
        assert_eq!(built_with.cargo_loki, env!("CARGO_PKG_VERSION"));
        for (name, expected) in manifest.files {
            let mut bytes = Vec::new();
            zip.by_name(&name).unwrap().read_to_end(&mut bytes).unwrap();
            assert_eq!(hex::encode(Sha256::digest(&bytes)), expected);
        }
        assert_eq!(zip.len(), 3);
    }
    #[test]
    fn rejects_unsafe_and_duplicate_assets_and_bad_components() {
        let (_temp, mut project, wasm) = fixture();
        for assets in [
            json!(["../outside"]),
            json!(["static", "static/枝.txt"]),
            json!(["C:/private"]),
            json!(["missing"]),
        ] {
            project.package.metadata["loki"]["assets"] = assets;
            assert!(create(&project, &wasm, None).is_err());
        }
        project.package.metadata["loki"]["assets"] = json!([]);
        std::fs::write(&wasm, b"\0asm\x01\0\0\0").unwrap();
        assert!(create(&project, &wasm, None).is_err());
        assert!(!project.target.join("loki/demo-0.1.0.loki-app").exists());
    }
    #[test]
    fn rejects_links_and_preserves_previous_output_on_validation_failure() {
        let (_temp, project, wasm) = fixture();
        let output = create(&project, &wasm, None).unwrap();
        let before = std::fs::read(&output).unwrap();
        std::fs::write(&wasm, b"not a component").unwrap();
        assert!(create(&project, &wasm, Some(&output)).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), before);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../demo.wasm", project.root.join("static/link")).unwrap();
            assert!(collect(&project.root, Path::new("static"), &mut BTreeMap::new()).is_err());
        }
        #[cfg(windows)]
        {
            let junction = project.root.join("static/junction");
            assert!(
                std::process::Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-Command",
                        "New-Item -ItemType Junction -Path $env:LOKI_TEST_JUNCTION -Target $env:LOKI_TEST_TARGET -ErrorAction Stop | Out-Null"
                    ])
                    .env("LOKI_TEST_JUNCTION", &junction)
                    .env("LOKI_TEST_TARGET", &project.target)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(collect(&project.root, Path::new("static"), &mut BTreeMap::new()).is_err());
            // Remove only the junction itself; never recurse through it.
            std::fs::remove_dir(junction).unwrap();
        }
    }
}
