use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256, Sha512};
use std::{
    fs,
    io::{Cursor, Read},
    path::{Component, Path, PathBuf},
    time::Duration,
};

#[derive(Args)]
pub struct Options {
    #[arg(long, default_value = if cfg!(windows) { "target/release/loki-server.exe" } else { "target/release/loki-server" })]
    pub binary: PathBuf,
    #[arg(long, default_value = "target/bundle")]
    pub output: PathBuf,
    #[arg(long, default_value = "2.11.4")]
    pub caddy_version: String,
    /// Use a previously downloaded official archive without network access.
    #[arg(long, requires = "caddy_checksums")]
    pub caddy_archive: Option<PathBuf>,
    #[arg(long, requires = "caddy_archive")]
    pub caddy_checksums: Option<PathBuf>,
}

fn relative(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['\\', ':', '\0'])
        && !name.starts_with('/')
        && name
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
        && Path::new(name)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
fn checksum(text: &str, filename: &str, bytes: &[u8]) -> Result<String> {
    let mut matches = text.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let hash = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        (name == filename && fields.next().is_none()).then_some(hash)
    });
    let expected = matches
        .next()
        .context("Archive is missing from official Caddy checksum list")?;
    ensure!(matches.next().is_none(), "Duplicate archive checksum entry");
    let actual = match expected.len() {
        64 => hex::encode(Sha256::digest(bytes)),
        128 => hex::encode(Sha512::digest(bytes)),
        _ => bail!("Unsupported Caddy checksum format"),
    };
    ensure!(
        expected.eq_ignore_ascii_case(&actual),
        "Caddy archive checksum mismatch"
    );
    Ok(expected.to_ascii_lowercase())
}
fn download(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::blocking::Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(60))
        .user_agent("LOKI-release-builder")
        .build()?
        .get(url)
        .send()?
        .error_for_status()?;
    let mut bytes = Vec::new();
    response
        .take(128 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 128 * 1024 * 1024,
        "Download exceeded 128 MiB"
    );
    Ok(bytes)
}
fn write_executable(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}
fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for item in fs::read_dir(source)? {
        let item = item?;
        let name = item.file_name();
        if name == "target"
            || name == ".git"
            || item.path().extension().is_some_and(|e| e == "loki-app")
        {
            continue;
        }
        let meta = item.path().symlink_metadata()?;
        ensure!(
            !meta.file_type().is_symlink(),
            "Bundle sources cannot contain symlinks: {}",
            item.path().display()
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                meta.file_attributes() & 0x400 == 0,
                "Bundle sources cannot contain reparse points"
            );
        }
        if meta.is_dir() {
            copy_tree(&item.path(), &dest.join(&name))?;
        } else {
            ensure!(meta.is_file(), "Bundle source is not a regular file");
            fs::copy(item.path(), dest.join(name))?;
        }
    }
    Ok(())
}
fn caddy_files(
    archive: &[u8],
    extension: &str,
    executable: &str,
    destination: &Path,
) -> Result<()> {
    let mut binary = None;
    let mut license = None;
    if extension == "zip" {
        let mut archive = zip::ZipArchive::new(Cursor::new(archive))?;
        for name in [executable, "LICENSE"] {
            if let Ok(mut entry) = archive.by_name(name) {
                ensure!(
                    entry.is_file() && entry.size() <= 128 * 1024 * 1024,
                    "Invalid Caddy archive member"
                );
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                if name == executable {
                    binary = Some(bytes)
                } else {
                    license = Some(bytes)
                }
            }
        }
    } else {
        let mut archive = tar::Archive::new(GzDecoder::new(archive));
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path == Path::new(executable) || path == Path::new("LICENSE") {
                ensure!(
                    entry.header().entry_type().is_file() && entry.size() <= 128 * 1024 * 1024,
                    "Invalid Caddy archive member"
                );
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                if path == Path::new(executable) {
                    binary = Some(bytes)
                } else {
                    license = Some(bytes)
                }
            }
        }
    }
    write_executable(
        &destination.join(executable),
        &binary.context("Missing Caddy executable")?,
    )?;
    fs::write(
        destination.join("CADDY-LICENSE.txt"),
        license.context("Missing Caddy license")?,
    )?;
    Ok(())
}
fn extract_sdk(source: &Path, dest: &Path, version: &str) -> Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(fs::File::open(source)?));
    let prefix = format!("loki-sdk-{version}/");
    let mut total = 0u64;
    let mut seen = std::collections::HashSet::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        ensure!(
            entry.header().entry_type().is_file(),
            "SDK archive contains non-regular entry"
        );
        let name = entry
            .path()?
            .to_str()
            .context("Non-UTF-8 SDK path")?
            .to_owned();
        let name = name
            .strip_prefix(&prefix)
            .context("Unexpected SDK archive root")?;
        ensure!(
            relative(name) && seen.insert(name.to_owned()),
            "Unsafe or duplicate SDK archive path"
        );
        total = total
            .checked_add(entry.size())
            .context("SDK size overflow")?;
        ensure!(
            total <= 64 * 1024 * 1024 && seen.len() <= 4096,
            "SDK archive exceeds bounds"
        );
        let path = dest.join(name);
        fs::create_dir_all(path.parent().unwrap())?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        std::io::copy(&mut entry, &mut output)?;
    }
    let manifest = dest.join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest)?;
    let parsed: toml::Value = toml::from_str(&text)?;
    ensure!(
        parsed.get("workspace").is_none(),
        "SDK package unexpectedly contains a workspace"
    );
    text.push_str("\n[workspace]\n");
    fs::write(manifest, text)?;
    Ok(())
}
pub fn run(options: Options) -> Result<()> {
    ensure!(
        options.caddy_version.split('.').count() >= 3
            && options
                .caddy_version
                .split('.')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
        "Use a numeric Caddy release version"
    );
    let os = match std::env::consts::OS {
        "macos" => "mac",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        arch => bail!("Unsupported release architecture: {arch}"),
    };
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    let filename = format!("caddy_{}_{os}_{arch}.{extension}", options.caddy_version);
    let output = if options.output.is_absolute() {
        options.output
    } else {
        crate::root().join(options.output)
    };
    ensure!(
        !output.exists() || (output.is_dir() && fs::read_dir(&output)?.next().is_none()),
        "Bundle output must be absent or empty: {} (use a fresh directory to avoid stale files)",
        output.display()
    );
    let binary = crate::absolute(&options.binary)?;
    let tool = binary.with_file_name(if cfg!(windows) {
        "cargo-loki.exe"
    } else {
        "cargo-loki"
    });
    ensure!(
        tool.is_file(),
        "Build cargo-loki alongside the server before bundling"
    );
    let workspace: toml::Value =
        toml::from_str(&fs::read_to_string(crate::root().join("Cargo.toml"))?)?;
    let version = workspace["workspace"]["package"]["version"]
        .as_str()
        .context("Missing workspace version")?;
    let sdk = crate::root()
        .join("target/package")
        .join(format!("loki-sdk-{version}.crate"));
    ensure!(
        sdk.is_file(),
        "Build the SDK archive first: cargo package -p loki-sdk"
    );
    let (bytes, checksums) = match (options.caddy_archive, options.caddy_checksums) {
        (Some(archive), Some(checksums)) => (fs::read(archive)?, fs::read_to_string(checksums)?),
        (None, None) => {
            let base = format!(
                "https://github.com/caddyserver/caddy/releases/download/v{}/",
                options.caddy_version
            );
            let sums = String::from_utf8(download(&format!(
                "{base}caddy_{}_checksums.txt",
                options.caddy_version
            ))?)?;
            (download(&format!("{base}{filename}"))?, sums)
        }
        _ => bail!("Supply both --caddy-archive and --caddy-checksums"),
    };
    let expected = checksum(&checksums, &filename, &bytes)?;
    let parent = output
        .parent()
        .context("Bundle output needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".loki-bundle-")
        .tempdir_in(parent)?;
    let dest = stage.path();
    caddy_files(
        &bytes,
        extension,
        if cfg!(windows) { "caddy.exe" } else { "caddy" },
        dest,
    )?;
    fs::write(
        dest.join("CADDY-SOURCE.txt"),
        format!(
            "Caddy v{}\nhttps://github.com/caddyserver/caddy\nArchive SHA{}: {expected}\n",
            options.caddy_version,
            expected.len() * 4
        ),
    )?;
    fs::copy(
        binary,
        dest.join(if cfg!(windows) {
            "loki-server.exe"
        } else {
            "loki-server"
        }),
    )?;
    fs::copy(&tool, dest.join(tool.file_name().unwrap()))?;
    for file in ["README.md", "LICENSE"] {
        fs::copy(crate::root().join(file), dest.join(file))?;
    }
    copy_tree(&crate::root().join("docs"), &dest.join("docs"))?;
    copy_tree(
        &crate::root().join("crates/loki-server/examples"),
        &dest.join("examples"),
    )?;
    copy_tree(
        &crate::root().join("crates/loki-server/examples"),
        &dest.join("crates/loki-server/examples"),
    )?;
    copy_tree(
        &crate::root().join("crates/loki-sdk/wit"),
        &dest.join("wit"),
    )?;
    fs::create_dir_all(dest.join("crates/cargo-loki"))?;
    fs::copy(
        crate::root().join("crates/cargo-loki/README.md"),
        dest.join("crates/cargo-loki/README.md"),
    )?;
    fs::copy(&sdk, dest.join(sdk.file_name().unwrap()))?;
    extract_sdk(&sdk, &dest.join("crates/loki-sdk"), version)?;
    copy_tree(
        &crate::root().join("examples/storage-notes"),
        &dest.join("examples/storage-notes"),
    )?;
    if output.exists() {
        fs::remove_dir(&output)?;
    }
    fs::rename(dest, &output)?;
    println!("{}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_checksums_fail_closed_and_accept_official_algorithms() {
        for digest in [
            hex::encode(Sha256::digest(b"caddy")),
            hex::encode(Sha512::digest(b"caddy")),
        ] {
            let text = format!("{digest}  caddy.zip\n");
            assert_eq!(checksum(&text, "caddy.zip", b"caddy").unwrap(), digest);
            assert!(checksum(&text, "caddy.zip", b"modified").is_err());
            assert!(checksum(&(text.clone() + &text), "caddy.zip", b"caddy").is_err());
        }
        for path in [
            "../escape",
            "/absolute",
            "C:/path",
            "a\\b",
            "a/../b",
            "a//b",
        ] {
            assert!(!relative(path));
        }
        assert!(relative("src/lib.rs"));
    }
}
