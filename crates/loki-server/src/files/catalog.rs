use super::{Share, excluded};
use crate::{Error, Result};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read, path::Path, time::UNIX_EPOCH};

pub(super) static SCANS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
const MAX_ENTRIES: usize = 2_000;
const PLATFORMS: &[&str] = &["windows", "macos", "linux", "android", "ios"];

#[derive(Serialize)]
pub(super) struct App {
    id: String,
    name: String,
    version: Option<String>,
    builds: BTreeMap<String, Build>,
}

#[derive(Serialize)]
struct Build {
    path: String,
    name: String,
    size: u64,
    modified: u64,
}

#[derive(Deserialize)]
struct Manifest {
    name: Option<String>,
    version: Option<String>,
    builds: Option<BTreeMap<String, String>>,
}

pub(super) fn scan(share: &Share) -> Result<Vec<App>> {
    let mut apps = Vec::new();
    let mut budget = 12_000;
    for (id, directory) in names(share, "", &mut budget)? {
        if !directory || share.open(&id).is_err() {
            continue;
        }
        let manifest = manifest(share, &id)?;
        let builds = match manifest.as_ref().and_then(|value| value.builds.as_ref()) {
            Some(paths) => explicit_builds(share, &id, paths)?,
            None => discover_builds(share, &id, &mut budget)?,
        };
        if builds.is_empty() {
            continue;
        }
        let (name, version) = match manifest {
            Some(value) => (value.name.unwrap_or_else(|| id.clone()), value.version),
            None => (id.clone(), None),
        };
        apps.push(App {
            id,
            name,
            version,
            builds,
        });
    }
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    Ok(apps)
}

fn names(share: &Share, path: &str, budget: &mut usize) -> Result<Vec<(String, bool)>> {
    let (dir, relative) = share.open(path)?;
    let mut names = Vec::new();
    for (count, item) in dir
        .read_dir(if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            &relative
        })?
        .enumerate()
    {
        if count >= MAX_ENTRIES || *budget == 0 {
            return Err(Error::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "An app repository directory has too many entries.",
            ));
        }
        *budget -= 1;
        let entry = item?;
        let Some(name) = entry
            .file_name()
            .into_string()
            .ok()
            .filter(|name| !name.starts_with('.') && !name.contains(['/', '\\', ':']))
        else {
            continue;
        };
        let kind = entry.file_type()?;
        if kind.is_dir() || kind.is_file() {
            names.push((name, kind.is_dir()));
        }
    }
    Ok(names)
}

fn manifest(share: &Share, id: &str) -> Result<Option<Manifest>> {
    let path = format!("{id}/app.json");
    if excluded(Path::new(&path), &share.excluded_extensions) {
        return Ok(None);
    }
    let (dir, relative) = match share.open(&path) {
        Ok(entry) => entry,
        Err(error) if error.status == StatusCode::NOT_FOUND => return Ok(None),
        Err(error) => return Err(error),
    };
    let file = dir.open(&relative)?.into_std();
    if !file.metadata()?.is_file() {
        return Err(invalid(id));
    }
    super::check_opened_file(&file, &relative, &share.excluded_extensions)?;
    let mut data = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut data)?;
    let value: Manifest = serde_json::from_slice(&data).map_err(|_| invalid(id))?;
    if data.len() > 16 * 1024
        || [&value.name, &value.version].iter().any(|text| {
            text.as_ref().is_some_and(|text| {
                text.trim().is_empty() || text.len() > 120 || text.chars().any(char::is_control)
            })
        })
    {
        return Err(invalid(id));
    }
    Ok(Some(value))
}

fn invalid(id: &str) -> Error {
    Error::bad(format!("Invalid app.json or build path in {id}."))
}

fn build(share: &Share, path: String) -> Option<Build> {
    let (dir, relative) = share.open(&path).ok()?;
    let name = relative.file_name()?.to_str()?.to_owned();
    if excluded(&relative, &share.excluded_extensions) {
        return None;
    }
    let file = dir.open(&relative).ok()?.into_std();
    super::check_opened_file(&file, &relative, &share.excluded_extensions).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some(Build {
        path,
        name,
        size: meta.len(),
        modified: modified.try_into().ok()?,
    })
}

fn explicit_builds(
    share: &Share,
    id: &str,
    paths: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Build>> {
    paths
        .iter()
        .map(|(platform, path)| {
            if !PLATFORMS.contains(&platform.as_str())
                || path.len() > 512
                || path.is_empty()
                || super::relative(path).is_err()
            {
                return Err(invalid(id));
            }
            let entry = build(share, format!("{id}/{path}")).ok_or_else(|| invalid(id))?;
            Ok((platform.clone(), entry))
        })
        .collect()
}

fn discover_builds(share: &Share, id: &str, budget: &mut usize) -> Result<BTreeMap<String, Build>> {
    let mut builds = BTreeMap::new();
    let mut directories = Vec::new();
    for (name, directory) in names(share, id, budget)? {
        let path = format!("{id}/{name}");
        if directory {
            let platform = name.to_ascii_lowercase();
            if PLATFORMS.contains(&platform.as_str()) && share.open(&path).is_ok() {
                directories.push((platform, path));
            }
        } else if let Some(platform) = root_platform(&name) {
            choose(&mut builds, platform, build(share, path));
        }
    }
    let mut by_platform = BTreeMap::new();
    for (platform, path) in directories {
        for (name, directory) in names(share, &path, budget)? {
            if !directory && is_package(&name) {
                choose(
                    &mut by_platform,
                    &platform,
                    build(share, format!("{path}/{name}")),
                );
            }
        }
    }
    builds.extend(by_platform);
    Ok(builds)
}

fn choose(builds: &mut BTreeMap<String, Build>, platform: &str, candidate: Option<Build>) {
    if let Some(candidate) = candidate
        && builds.get(platform).is_none_or(|previous| {
            (candidate.modified, &candidate.path) > (previous.modified, &previous.path)
        })
    {
        builds.insert(platform.into(), candidate);
    }
}

const ARCHIVES: &[&str] = &[
    ".zip", ".7z", ".tar", ".tar.gz", ".tar.xz", ".tar.zst", ".tgz",
];

fn root_platform(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    for (platform, extensions) in [
        (
            "windows",
            &[
                ".exe",
                ".msi",
                ".msix",
                ".msixbundle",
                ".appx",
                ".appxbundle",
                ".zip",
                ".7z",
            ][..],
        ),
        ("macos", &[".dmg", ".pkg"][..]),
        (
            "linux",
            &[
                ".deb",
                ".rpm",
                ".appimage",
                ".tar",
                ".tar.gz",
                ".tar.xz",
                ".tar.zst",
                ".tgz",
            ][..],
        ),
        ("android", &[".apk"][..]),
        ("ios", &[".ipa"][..]),
    ] {
        if extensions.iter().any(|suffix| name.ends_with(suffix)) {
            return Some(platform);
        }
    }
    None
}

fn is_package(name: &str) -> bool {
    root_platform(name).is_some()
        || ARCHIVES
            .iter()
            .any(|suffix| name.to_ascii_lowercase().ends_with(suffix))
}
