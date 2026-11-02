//! Writes and checks entries in a clone of the LOKI app registry
//! (github.com/LOKI-Server/apps): `apps/<id>/app.json` and an optional `icon.png`.
use anyhow::{Context, Result, bail, ensure};
use loki_sdk::{
    manifest::{Manifest, validate_manifest},
    registry::{Entry, MAX_PACKAGE},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_ICON: u64 = 256 * 1024;

/// Adds an app to the registry, or points its entry at a new release.
pub fn add(
    registry: &Path,
    url: &str,
    source: Option<String>,
    icon: Option<&Path>,
) -> Result<PathBuf> {
    let apps = apps_dir(registry)?;
    let package = download(url)?;
    let entry = describe(&package, url);
    let _ = std::fs::remove_file(&package);
    let mut entry = entry?;
    let dir = apps.join(&entry.id);
    let file = dir.join("app.json");
    if let Ok(text) = std::fs::read_to_string(&file) {
        let old: Entry = serde_json::from_str(&text)
            .with_context(|| format!("{} is not a valid registry entry", file.display()))?;
        entry.source = old.source;
        ensure!(
            semver::Version::parse(&entry.version)? >= semver::Version::parse(&old.version)?,
            "The registry lists {} {}, which is newer than this package ({}).",
            entry.id,
            old.version,
            entry.version
        );
    }
    if let Some(source) = source {
        entry.source = source;
    }
    entry.validate()?;
    if let Some(icon) = icon {
        check_icon(icon)?;
    }
    std::fs::create_dir_all(&dir)?;
    if let Some(icon) = icon {
        std::fs::copy(icon, dir.join("icon.png"))?;
    }
    write_entry(&file, &entry)?;
    Ok(file)
}

/// Checks every entry. Unless `offline`, also downloads each package and checks
/// that it is the file the entry describes.
pub fn check(registry: &Path, offline: bool) -> Result<()> {
    let apps = apps_dir(registry)?;
    let mut dirs: Vec<_> = std::fs::read_dir(&apps)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    dirs.sort();
    let mut failed = 0;
    for dir in &dirs {
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        match check_app(dir, offline) {
            Ok(entry) => println!("ok      {} {}", entry.id, entry.version),
            Err(error) => {
                failed += 1;
                println!("FAILED  {name}: {error:#}");
            }
        }
    }
    ensure!(failed == 0, "{failed} of {} apps failed", dirs.len());
    println!("{} apps checked", dirs.len());
    Ok(())
}

fn check_app(dir: &Path, offline: bool) -> Result<Entry> {
    ensure!(dir.is_dir(), "apps/ may contain only app folders");
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    for file in std::fs::read_dir(dir)? {
        let file = file?.file_name();
        ensure!(
            file == "app.json" || file == "icon.png",
            "unexpected file {}; an app folder holds only app.json and icon.png",
            file.to_string_lossy()
        );
    }
    let text = std::fs::read_to_string(dir.join("app.json")).context("missing app.json")?;
    let entry: Entry = serde_json::from_str(&text).context("app.json is not a valid entry")?;
    entry.validate()?;
    ensure!(entry.id == name, "the folder must be named {}", entry.id);
    ensure!(
        entry.icon.is_none(),
        "app.json must not set icon; add icon.png instead"
    );
    let icon = dir.join("icon.png");
    if icon.exists() {
        check_icon(&icon)?;
    }
    if !offline {
        let package = download(&entry.url)?;
        let result = verify(&entry, &package);
        let _ = std::fs::remove_file(&package);
        result?;
    }
    Ok(entry)
}

fn apps_dir(registry: &Path) -> Result<PathBuf> {
    let apps = registry.join("apps");
    ensure!(
        apps.is_dir(),
        "{} has no apps/ folder. Run this in a clone of github.com/LOKI-Server/apps or pass --registry.",
        registry.display()
    );
    Ok(apps)
}

fn write_entry(file: &Path, entry: &Entry) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(entry)?;
    bytes.push(b'\n');
    std::fs::write(file, bytes)?;
    Ok(())
}

/// The entry for a package file hosted at `url`.
pub fn describe(package: &Path, url: &str) -> Result<Entry> {
    let manifest = read_package(package)?;
    let (sha256, size) = hash(package)?;
    Ok(Entry::for_package(&manifest, url, &sha256, size)?)
}

/// Whether a package file is the release `entry` describes.
pub fn verify(entry: &Entry, package: &Path) -> Result<()> {
    let (sha256, size) = hash(package)?;
    ensure!(
        size == entry.size,
        "the package is {size} bytes, but the entry says {}",
        entry.size
    );
    ensure!(
        sha256 == entry.sha256,
        "the package's sha256 is {sha256}, but the entry says {}",
        entry.sha256
    );
    entry.matches(&read_package(package)?)?;
    Ok(())
}

fn hash(path: &Path) -> Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let size = std::io::copy(&mut file, &mut hash)?;
    Ok((hex::encode(hash.finalize()), size))
}

/// Reads and checks a package the way LOKI does at import, without extracting it.
fn read_package(path: &Path) -> Result<Manifest> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)
        .context("the file is not a .loki-app package")?;
    let manifest: Manifest = {
        let file = zip
            .by_name("manifest.json")
            .context("the package has no manifest.json")?;
        ensure!(file.size() <= 131072, "the manifest exceeds 128 KiB");
        serde_json::from_reader(file).context("the package manifest is not valid")?
    };
    validate_manifest(&manifest)?;
    let mut seen = HashSet::new();
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        let name = file.name().unwrap_or_default().into_owned();
        if name == "manifest.json" {
            continue;
        }
        let expected = manifest
            .files
            .get(&name)
            .with_context(|| format!("the package contains undeclared file {name}"))?;
        let mut hash = Sha256::new();
        std::io::copy(&mut (&mut file).take(MAX_PACKAGE), &mut hash)?;
        ensure!(
            hex::encode(hash.finalize()) == expected.to_ascii_lowercase(),
            "checksum mismatch for {name}"
        );
        seen.insert(name);
    }
    ensure!(
        manifest.files.keys().all(|f| seen.contains(f)),
        "the package is missing a declared file"
    );
    Ok(manifest)
}

/// A square PNG, 64 to 1024 pixels on a side, up to 256 KiB.
fn check_icon(path: &Path) -> Result<()> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    ensure!(size <= MAX_ICON, "the icon exceeds 256 KiB");
    let mut header = [0u8; 24];
    std::fs::File::open(path)?
        .read_exact(&mut header)
        .context("the icon is not a PNG")?;
    ensure!(
        header.starts_with(b"\x89PNG\r\n\x1a\n") && &header[12..16] == b"IHDR",
        "the icon is not a PNG"
    );
    let width = u32::from_be_bytes(header[16..20].try_into()?);
    let height = u32::from_be_bytes(header[20..24].try_into()?);
    ensure!(
        width == height && (64..=1024).contains(&width),
        "the icon must be square, 64 to 1024 pixels (it is {width}×{height})"
    );
    Ok(())
}

/// Saves a download to a temporary file, refusing anything over 512 MiB.
fn download(url: &str) -> Result<PathBuf> {
    ensure!(
        url.starts_with("https://"),
        "the download must be an https:// link"
    );
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(std::time::Duration::from_secs(600))
        .build()?;
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("cannot download {url}"))?;
    if !response.status().is_success() {
        bail!("{url} answered {}", response.status());
    }
    let path = std::env::temp_dir().join(format!(
        "cargo-loki-{}-{:016x}.loki-app",
        std::process::id(),
        rand::random::<u64>()
    ));
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create_new(&path)?;
        let mut buffer = [0; 65536];
        let mut total = 0u64;
        loop {
            let count = response.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            ensure!(total <= MAX_PACKAGE, "the package exceeds 512 MiB");
            file.write_all(&buffer[..count])?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&path);
        return Err(error.context(format!("cannot download {url}")));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package;

    #[test]
    fn entries_describe_their_package_and_catch_tampering() {
        let (_temp, project, wasm) = package::tests::fixture();
        let output = package::create(&project, &wasm, None).unwrap();
        let entry = describe(&output, "https://example.com/demo.loki-app").unwrap();
        assert_eq!(entry.id, "demo");
        assert_eq!(entry.version, "0.1.0");
        assert_eq!(entry.sdk, "0.3.0");
        assert_eq!(entry.size, std::fs::metadata(&output).unwrap().len());
        verify(&entry, &output).unwrap();
        let mut wrong = entry.clone();
        wrong.sha256 = "0".repeat(64);
        assert!(verify(&wrong, &output).is_err());
        let mut wrong = entry.clone();
        wrong.version = "0.2.0".into();
        assert!(verify(&wrong, &output).is_err());
    }

    #[test]
    fn folders_are_checked_offline() {
        let (temp, project, wasm) = package::tests::fixture();
        let output = package::create(&project, &wasm, None).unwrap();
        let registry = temp.path().join("registry");
        let dir = registry.join("apps/demo");
        std::fs::create_dir_all(&dir).unwrap();
        let entry = describe(&output, "https://example.com/demo.loki-app").unwrap();
        write_entry(&dir.join("app.json"), &entry).unwrap();
        check(&registry, true).unwrap();
        std::fs::write(dir.join("icon.png"), b"not a png").unwrap();
        assert!(check(&registry, true).is_err());
        std::fs::remove_file(dir.join("icon.png")).unwrap();
        std::fs::rename(&dir, registry.join("apps/other")).unwrap();
        assert!(check(&registry, true).is_err());
    }
}
