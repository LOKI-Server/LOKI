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
    /// Include cargo-loki, SDK sources, examples and developer documentation.
    #[arg(long)]
    pub developer_tools: bool,
    #[arg(long, default_value = "2.11.4")]
    pub caddy_version: String,
    /// Use a previously downloaded official archive without network access.
    #[arg(long, requires = "caddy_checksums")]
    pub caddy_archive: Option<PathBuf>,
    #[arg(long, requires = "caddy_archive")]
    pub caddy_checksums: Option<PathBuf>,
}

struct DeveloperInputs {
    tool: PathBuf,
    sdk: PathBuf,
    version: String,
}
fn developer_inputs(binary: &Path, enabled: bool, root: &Path) -> Result<Option<DeveloperInputs>> {
    if !enabled {
        return Ok(None);
    }
    let tool = binary.with_file_name(if cfg!(windows) {
        "cargo-loki.exe"
    } else {
        "cargo-loki"
    });
    ensure!(
        tool.is_file(),
        "Build cargo-loki alongside the server for --developer-tools"
    );
    let workspace: toml::Value = toml::from_str(&fs::read_to_string(root.join("Cargo.toml"))?)?;
    let version = workspace["workspace"]["package"]["version"]
        .as_str()
        .context("Missing workspace version")?
        .to_owned();
    let sdk = root
        .join("target/package")
        .join(format!("loki-sdk-{version}.crate"));
    ensure!(
        sdk.is_file(),
        "Build the SDK archive for --developer-tools: cargo package -p loki-sdk"
    );
    Ok(Some(DeveloperInputs { tool, sdk, version }))
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

fn ordinary_directory(path: &Path) -> Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "Bundle output must be a directory, not a file or link: {}",
        path.display()
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            meta.file_attributes() & 0x400 == 0,
            "Bundle output cannot be a junction or reparse point: {}",
            path.display()
        );
    }
    Ok(true)
}

fn validate_existing_bundle(output: &Path) -> Result<bool> {
    if !ordinary_directory(output)? {
        return Ok(false);
    }
    if fs::read_dir(output)?.next().is_none() {
        return Ok(true);
    }
    for name in [
        if cfg!(windows) {
            "loki-server.exe"
        } else {
            "loki-server"
        },
        if cfg!(windows) { "caddy.exe" } else { "caddy" },
        "LICENSE",
        "CADDY-LICENSE.txt",
        "CADDY-SOURCE.txt",
    ] {
        let valid = fs::symlink_metadata(output.join(name))
            .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink());
        ensure!(
            valid,
            "Refusing to replace a non-bundle directory: {} (missing regular {name})",
            output.display()
        );
    }
    Ok(true)
}

fn validate_output(output: &Path, inputs: &[&Path]) -> Result<PathBuf> {
    ensure!(
        !output
            .components()
            .any(|c| matches!(c, Component::ParentDir)),
        "Bundle output must not contain '..'"
    );
    let name = output
        .file_name()
        .context("Bundle output needs a directory name")?;
    let parent = output
        .parent()
        .context("Bundle output needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let parent = fs::canonicalize(parent)?;
    let candidate = parent.join(name);
    let resolved = if ordinary_directory(&candidate)? {
        fs::canonicalize(&candidate)?
    } else {
        candidate
    };
    let root = fs::canonicalize(crate::root())?;
    ensure!(
        !root.starts_with(&resolved) && resolved != root.join("target"),
        "Bundle output cannot replace the checkout or target root"
    );
    for name in ["crates", "docs", "examples", "packaging", ".git", ".cargo"] {
        ensure!(
            !resolved.starts_with(root.join(name)),
            "Bundle output cannot replace repository sources"
        );
    }
    if output.starts_with(crate::root()) {
        ensure!(
            resolved.starts_with(&root),
            "Bundle output parent resolves outside the checkout"
        );
    }
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        let home = fs::canonicalize(home)?;
        ensure!(
            !home.starts_with(&resolved),
            "Bundle output cannot replace the user home directory or its ancestors"
        );
    }
    for input in inputs {
        let input = fs::canonicalize(input)?;
        ensure!(
            !input.starts_with(&resolved),
            "Bundle output would replace an input file: {}",
            input.display()
        );
    }
    validate_existing_bundle(&resolved)?;
    Ok(resolved)
}

fn publish(stage: &Path, output: &Path) -> Result<()> {
    if !validate_existing_bundle(output)? {
        fs::rename(stage, output)?;
        return Ok(());
    }
    let parent = output.parent().context("Bundle output needs a parent")?;
    let previous = tempfile::Builder::new()
        .prefix(".loki-previous-bundle-")
        .tempdir_in(parent)?;
    let old = previous.path().join("bundle");
    // Resolve/check both move destinations before touching the existing output.
    let parent = fs::canonicalize(parent)?;
    let previous_path = fs::canonicalize(previous.path())?;
    ensure!(
        previous_path.starts_with(&parent) && fs::canonicalize(output)? == output,
        "Bundle paths changed during assembly"
    );
    fs::rename(output, &old).with_context(|| {
        format!(
            "Could not replace {}. Close programs running from that bundle and retry.",
            output.display()
        )
    })?;
    if let Err(error) = fs::rename(stage, output) {
        if let Err(rollback) = fs::rename(&old, output) {
            let saved = previous.keep();
            bail!(
                "Bundle publish failed: {error}; restoring the old bundle also failed: {rollback}. Previous bundle retained at {}",
                saved.join("bundle").display()
            );
        }
        return Err(error).context("Could not publish new bundle; previous bundle restored");
    }
    // TempDir removes only this verified private sibling, never the output itself.
    if let Err(error) = previous.close() {
        eprintln!(
            "New bundle is ready; could not remove the previous bundle at {}: {error}",
            old.display()
        );
    }
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
    let binary = crate::absolute(&options.binary)?;
    let developer = developer_inputs(&binary, options.developer_tools, &crate::root())?;
    let mut inputs = vec![binary.as_path()];
    if let Some(developer) = &developer {
        inputs.extend([developer.tool.as_path(), developer.sdk.as_path()]);
    }
    if let Some(archive) = &options.caddy_archive {
        inputs.push(archive);
    }
    if let Some(checksums) = &options.caddy_checksums {
        inputs.push(checksums);
    }
    let output = validate_output(&output, &inputs)?;
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
    fs::copy(crate::root().join("LICENSE"), dest.join("LICENSE"))?;
    if let Some(DeveloperInputs { tool, sdk, version }) = developer {
        fs::copy(&tool, dest.join(tool.file_name().unwrap()))?;
        fs::copy(crate::root().join("README.md"), dest.join("README.md"))?;
        copy_tree(&crate::root().join("docs"), &dest.join("docs"))?;
        copy_tree(
            &crate::root().join("crates/loki-sdk/wit"),
            &dest.join("wit"),
        )?;
        fs::create_dir_all(dest.join("crates/cargo-loki"))?;
        fs::copy(
            crate::root().join("crates/cargo-loki/README.md"),
            dest.join("crates/cargo-loki/README.md"),
        )?;
        fs::create_dir_all(dest.join("crates/xtask"))?;
        fs::copy(
            crate::root().join("crates/xtask/README.md"),
            dest.join("crates/xtask/README.md"),
        )?;
        fs::copy(&sdk, dest.join(sdk.file_name().unwrap()))?;
        extract_sdk(&sdk, &dest.join("crates/loki-sdk"), &version)?;
        copy_tree(
            &crate::root().join("examples/storage-notes"),
            &dest.join("examples/storage-notes"),
        )?;
    }
    publish(dest, &output)?;
    println!("{}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_bundle_requires_no_sdk_or_tool_and_contains_only_runtime_files() {
        use std::io::Write;
        let directory = tempfile::tempdir().unwrap();
        let server = directory.path().join(if cfg!(windows) {
            "loki-server.exe"
        } else {
            "loki-server"
        });
        fs::write(&server, b"server fixture").unwrap();
        assert!(
            developer_inputs(&server, false, directory.path())
                .unwrap()
                .is_none()
        );
        assert!(developer_inputs(&server, true, directory.path()).is_err());
        let executable = if cfg!(windows) { "caddy.exe" } else { "caddy" };
        let mut bytes = Vec::new();
        if cfg!(windows) {
            let mut archive = zip::ZipWriter::new(Cursor::new(&mut bytes));
            for (name, data) in [
                (executable, b"caddy fixture".as_slice()),
                ("LICENSE", b"license fixture".as_slice()),
            ] {
                archive
                    .start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                archive.write_all(data).unwrap();
            }
            archive.finish().unwrap();
        } else {
            let gzip = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            for (name, data) in [
                (executable, b"caddy fixture".as_slice()),
                ("LICENSE", b"license fixture".as_slice()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                archive.append_data(&mut header, name, data).unwrap();
            }
            archive.into_inner().unwrap().finish().unwrap();
        }
        let os = if cfg!(target_os = "macos") {
            "mac"
        } else {
            std::env::consts::OS
        };
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "amd64"
        };
        let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
        let archive = directory.path().join("caddy-archive");
        fs::write(&archive, &bytes).unwrap();
        let sums = directory.path().join("checksums.txt");
        fs::write(
            &sums,
            format!(
                "{}  caddy_2.11.4_{os}_{arch}.{extension}\n",
                hex::encode(Sha256::digest(&bytes))
            ),
        )
        .unwrap();
        let output = directory.path().join("runtime");
        let options = || Options {
            binary: server.clone(),
            output: output.clone(),
            developer_tools: false,
            caddy_version: "2.11.4".into(),
            caddy_archive: Some(archive.clone()),
            caddy_checksums: Some(sums.clone()),
        };
        run(options()).unwrap();
        let mut names: Vec<_> = fs::read_dir(&output)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let mut expected = vec![
            server.file_name().unwrap().to_str().unwrap(),
            executable,
            "LICENSE",
            "CADDY-LICENSE.txt",
            "CADDY-SOURCE.txt",
        ];
        expected.sort();
        assert_eq!(names, expected);
        assert_eq!(fs::read(output.join(executable)).unwrap(), b"caddy fixture");
        fs::write(output.join("obsolete.txt"), b"stale file").unwrap();
        fs::create_dir(output.join("old-docs")).unwrap();
        fs::write(
            output.join("old-docs/stale.md"),
            b"obsolete developer asset",
        )
        .unwrap();
        fs::write(&server, b"updated server fixture").unwrap();
        run(options()).unwrap();
        assert!(!output.join("obsolete.txt").exists());
        assert!(!output.join("old-docs").exists());
        assert_eq!(
            fs::read(output.join(server.file_name().unwrap())).unwrap(),
            b"updated server fixture"
        );
        fs::write(&archive, b"corrupted download").unwrap();
        assert!(run(options()).is_err());
        assert_eq!(
            fs::read(output.join(server.file_name().unwrap())).unwrap(),
            b"updated server fixture"
        );
    }
    #[test]
    fn bundle_publish_failure_restores_previous_directory() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("bundle");
        fs::create_dir(&output).unwrap();
        // An empty existing output is also supported; failed publication must
        // leave it in place rather than lose the original directory.
        let output = fs::canonicalize(output).unwrap();
        assert!(publish(&directory.path().join("missing-stage"), &output).is_err());
        assert!(output.is_dir());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    #[test]
    fn bundle_output_cannot_replace_inputs_sources_or_unrelated_directories() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        fs::create_dir(&input).unwrap();
        let binary = input.join("server");
        fs::write(&binary, b"input file").unwrap();
        assert!(validate_output(&input, &[&binary]).is_err());
        let unrelated = directory.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("keep.txt"), b"keep me").unwrap();
        assert!(validate_output(&unrelated, &[]).is_err());
        assert_eq!(fs::read(unrelated.join("keep.txt")).unwrap(), b"keep me");
        for protected in [
            crate::root(),
            crate::root().join("target"),
            crate::root().join("crates"),
        ] {
            assert!(validate_output(&protected, &[]).is_err());
        }
    }
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

    #[test]
    fn sdk_sources_are_standalone_and_archive_links_are_rejected() {
        use std::io::Write;
        let directory = tempfile::tempdir().unwrap();
        let build = |path: &Path, linked: bool| {
            let file = fs::File::create(path).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut archive = tar::Builder::new(gzip);
            let contents = b"[package]\nname='loki-sdk'\nversion='0.1.0'\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "loki-sdk-0.1.0/Cargo.toml", &contents[..])
                .unwrap();
            if linked {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                header.set_link_name("../../outside").unwrap();
                header.set_cksum();
                archive
                    .append_data(&mut header, "loki-sdk-0.1.0/link", std::io::empty())
                    .unwrap();
            }
            let mut gzip = archive.into_inner().unwrap();
            gzip.flush().unwrap();
            gzip.finish().unwrap();
        };
        let safe = directory.path().join("safe.crate");
        build(&safe, false);
        let output = directory.path().join("safe-output");
        extract_sdk(&safe, &output, "0.1.0").unwrap();
        let manifest: toml::Value =
            toml::from_str(&fs::read_to_string(output.join("Cargo.toml")).unwrap()).unwrap();
        assert!(
            manifest
                .get("workspace")
                .unwrap()
                .as_table()
                .unwrap()
                .is_empty()
        );
        let unsafe_archive = directory.path().join("unsafe.crate");
        build(&unsafe_archive, true);
        let output = directory.path().join("unsafe-output");
        assert!(extract_sdk(&unsafe_archive, &output, "0.1.0").is_err());
        assert!(!output.join("link").exists());
        assert!(!directory.path().join("outside").exists());
    }
}
