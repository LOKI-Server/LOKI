//! The LOKI app registry (github.com/LOKI-Server/apps): a public, reviewed list of
//! apps that LOKI browses, installs from, and checks for updates. Each entry pins
//! its package's SHA-256, so LOKI installs only the file that was reviewed.
use crate::{Error, Result, State, apps, auth};
use loki_sdk::registry::{Entry, FORMAT, Index, MAX_PACKAGE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

pub const DEFAULT_URL: &str = "https://loki-server.github.io/apps/";
/// How long a fetched index is used before LOKI asks for it again.
const FRESH: Duration = Duration::from_secs(3600);
/// How often LOKI checks for app updates in the background.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const MAX_INDEX: u64 = 4 * 1024 * 1024;
const MAX_ICON: u64 = 256 * 1024;

/// The registry index as LOKI last read it.
pub struct Cache {
    /// The `registry_url` it came from.
    pub url: String,
    pub apps: Vec<Entry>,
    etag: Option<String>,
    generated: String,
    fetched: Option<Instant>,
    /// Why the last refresh failed, while LOKI shows the copy it has.
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    url: String,
    etag: Option<String>,
    index: Index,
}

/// Checks the registry setting: empty, or an `https://` base URL. Plain `http://`
/// is accepted only on this computer, for testing a local registry.
pub fn validate_url(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    let url = url::Url::parse(value)
        .ok()
        .filter(|u| allowed(u) && u.query().is_none() && u.fragment().is_none())
        .filter(|u| u.username().is_empty() && u.password().is_none())
        .ok_or_else(|| {
            Error::bad(
                "The app registry must be an https:// address, or left empty to turn it off.",
            )
        })?;
    let mut url = url.to_string();
    if !url.ends_with('/') {
        url.push('/');
    }
    Ok(url)
}

fn allowed(url: &url::Url) -> bool {
    match (url.scheme(), url.host()) {
        ("https", Some(_)) => true,
        ("http", Some(url::Host::Domain(host))) => host == "localhost",
        ("http", Some(url::Host::Ipv4(ip))) => ip.is_loopback(),
        ("http", Some(url::Host::Ipv6(ip))) => ip.is_loopback(),
        _ => false,
    }
}

/// Follows a few redirects, such as GitHub's release downloads, but never to plain
/// HTTP on another computer.
fn client(timeout: Option<Duration>) -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .user_agent(concat!("LOKI/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if allowed(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }));
    match timeout {
        Some(timeout) => builder.timeout(timeout),
        None => builder,
    }
    .build()
    .expect("TLS client")
}

fn saved_path(state: &State) -> PathBuf {
    state.0.data.join("registry").join("index.json")
}

fn load_saved(state: &State, url: &str) -> Option<Cache> {
    let saved: Saved = serde_json::from_slice(&std::fs::read(saved_path(state)).ok()?).ok()?;
    (saved.url == url).then(|| Cache {
        url: saved.url,
        apps: valid(saved.index.apps),
        etag: saved.etag,
        generated: saved.index.generated,
        fetched: None,
        error: None,
    })
}

/// Entries LOKI can show. One bad entry never hides the rest.
fn valid(apps: Vec<Entry>) -> Vec<Entry> {
    let mut seen = std::collections::HashSet::new();
    apps.into_iter()
        .filter(|e| e.validate().is_ok() && seen.insert(e.id.clone()))
        .collect()
}

/// The registry index, fetched again when it is older than an hour or `force` is
/// set. When the registry can't be reached, the last copy is returned with its
/// `error` set.
pub async fn index(state: &State, force: bool) -> Result<Arc<Cache>> {
    let url = state.0.settings.read().await.registry_url.clone();
    if url.is_empty() {
        return Err(Error::bad(
            "The app registry is turned off. Set its address in Server settings.",
        ));
    }
    let mut slot = state.0.registry.lock().await;
    if slot.as_ref().is_none_or(|c| c.url != url) {
        let state = state.clone();
        let saved_url = url.clone();
        *slot = tokio::task::spawn_blocking(move || load_saved(&state, &saved_url))
            .await?
            .map(Arc::new);
    }
    if let Some(cache) = slot.as_ref()
        && !force
        && cache.fetched.is_some_and(|f| f.elapsed() < FRESH)
    {
        return Ok(cache.clone());
    }
    let etag = slot.as_ref().and_then(|c| c.etag.clone());
    let cache = match fetch(&url, etag.as_deref()).await {
        Ok(None) => {
            let old = slot.as_ref().expect("an ETag comes from a cached index");
            Cache {
                url: url.clone(),
                apps: old.apps.clone(),
                etag,
                generated: old.generated.clone(),
                fetched: Some(Instant::now()),
                error: None,
            }
        }
        Ok(Some((index, etag))) => {
            let saved = Saved {
                url: url.clone(),
                etag: etag.clone(),
                index,
            };
            let path = saved_path(state);
            tokio::fs::create_dir_all(path.parent().unwrap()).await?;
            tokio::fs::write(&path, serde_json::to_vec(&saved)?).await?;
            Cache {
                url: url.clone(),
                apps: valid(saved.index.apps),
                etag,
                generated: saved.index.generated,
                fetched: Some(Instant::now()),
                error: None,
            }
        }
        Err(error) => {
            tracing::warn!(error = %error.message, "app registry refresh failed");
            let Some(old) = slot.as_ref() else {
                return Err(error);
            };
            Cache {
                url: url.clone(),
                apps: old.apps.clone(),
                etag: old.etag.clone(),
                generated: old.generated.clone(),
                // Try again on the next request rather than in an hour.
                fetched: None,
                error: Some(error.message),
            }
        }
    };
    let cache = Arc::new(cache);
    *slot = Some(cache.clone());
    Ok(cache)
}

/// The index, or `None` when it hasn't changed since `etag`.
async fn fetch(base: &str, etag: Option<&str>) -> Result<Option<(Index, Option<String>)>> {
    let unreachable =
        |_| Error::bad("LOKI couldn't reach the app registry. Check the connection and try again.");
    let mut request = client(Some(Duration::from_secs(15))).get(format!("{base}index.json"));
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let response = request.send().await.map_err(unreachable)?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED && etag.is_some() {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(Error::bad(format!(
            "The app registry answered {}.",
            response.status()
        )));
    }
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = limited(response, MAX_INDEX)
        .await
        .map_err(|_| Error::bad("The app registry index is larger than LOKI accepts."))?;
    let index: Index = serde_json::from_slice(&body)
        .map_err(|_| Error::bad("The app registry index is not valid."))?;
    if index.registry != FORMAT {
        return Err(Error::bad(format!(
            "The app registry uses format {}, which this LOKI can't read. Update LOKI.",
            index.registry
        )));
    }
    Ok(Some((index, etag)))
}

async fn limited(mut response: reqwest::Response, max: u64) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(response.content_length().is_none_or(|l| l <= max));
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
        anyhow::ensure!(body.len() as u64 <= max);
    }
    Ok(body)
}

/// What LOKI can do with a registry entry here.
#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Install,
    /// A newer release of an app installed from the registry.
    Update,
    /// An uploaded app with this ID, which the registry release can replace.
    Replace,
    Installed,
    Unsupported(String),
}

pub fn status(entry: &Entry, installed: Option<(&str, &str)>) -> Status {
    if let Some(reason) = apps::unsupported(Some("wasi"), Some(&entry.sdk)) {
        return Status::Unsupported(reason);
    }
    let Some((version, source)) = installed else {
        return Status::Install;
    };
    let (Ok(listed), Ok(version)) = (
        semver::Version::parse(&entry.version),
        semver::Version::parse(version),
    ) else {
        return Status::Installed;
    };
    match (source == "registry", listed.cmp(&version)) {
        (true, std::cmp::Ordering::Greater) => Status::Update,
        (false, std::cmp::Ordering::Greater | std::cmp::Ordering::Equal) => Status::Replace,
        _ => Status::Installed,
    }
}

/// Installed apps' versions and sources, by ID.
async fn installed(state: &State) -> Result<std::collections::HashMap<String, (String, String)>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT id,manifest,source FROM apps")
        .fetch_all(&state.0.db)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, manifest, source)| {
            let manifest: apps::Manifest = serde_json::from_str(&manifest).ok()?;
            Some((id, (manifest.version, source)))
        })
        .collect())
}

/// The registry as the owner's Browse page shows it.
pub async fn list(state: &State, force: bool) -> Result<Value> {
    let cache = index(state, force).await?;
    let installed = installed(state).await?;
    let apps: Vec<Value> = cache
        .apps
        .iter()
        .map(|entry| {
            let here = installed.get(&entry.id);
            let (status, reason) = match status(entry, here.map(|(v, s)| (v.as_str(), s.as_str())))
            {
                Status::Install => ("install", String::new()),
                Status::Update => ("update", String::new()),
                Status::Replace => ("replace", String::new()),
                Status::Installed => ("installed", String::new()),
                Status::Unsupported(reason) => ("unsupported", reason),
            };
            let mut value = json!(entry);
            value["status"] = json!(status);
            value["reason"] = json!(reason);
            value["installed"] = json!(here.map(|(v, _)| v));
            value["icon"] = json!(entry.icon.is_some());
            value
        })
        .collect();
    Ok(json!({"url": cache.url, "error": cache.error, "apps": apps}))
}

/// The newer registry release of an app installed from the registry, from the
/// cached index only, so the dashboard never waits on the network.
pub async fn update_for(state: &State, app: &apps::App, version: &str) -> Option<String> {
    if app.source != "registry" {
        return None;
    }
    let slot = state.0.registry.lock().await;
    let entry = slot.as_ref()?.apps.iter().find(|e| e.id == app.id)?;
    (status(entry, Some((version, "registry"))) == Status::Update).then(|| entry.version.clone())
}

/// Downloads an app's listed release, checks it is the reviewed file, and
/// installs or updates it.
pub async fn install(state: &State, id: &str, actor: &str) -> Result<apps::Manifest> {
    let cache = index(state, false).await?;
    let entry = cache
        .apps
        .iter()
        .find(|e| e.id == id)
        .cloned()
        .ok_or_else(|| Error::bad("The app registry doesn't list this app."))?;
    let installed = installed(state).await?;
    match status(
        &entry,
        installed.get(id).map(|(v, s)| (v.as_str(), s.as_str())),
    ) {
        Status::Install | Status::Update | Status::Replace => {}
        Status::Installed => {
            return Err(Error::bad(format!(
                "{} {} is already installed.",
                entry.name, entry.version
            )));
        }
        Status::Unsupported(reason) => return Err(Error::bad(reason)),
    }
    let path = state
        .0
        .data
        .join("staging")
        .join(format!("{}.download", auth::id()));
    let result = async {
        download(&entry, &path).await?;
        let check = entry.clone();
        let package = path.clone();
        tokio::task::spawn_blocking(move || {
            check
                .matches(&package_manifest(&package)?)
                .map_err(|e| Error::bad(e.message()))
        })
        .await??;
        apps::import(state, path.clone(), actor, "registry").await
    }
    .await;
    let _ = tokio::fs::remove_file(&path).await;
    result
}

async fn download(entry: &Entry, path: &Path) -> Result<()> {
    let failed = |_| {
        Error::bad(format!(
            "LOKI couldn't download {} {}. Try again later.",
            entry.name, entry.version
        ))
    };
    let mut response = client(None).get(&entry.url).send().await.map_err(failed)?;
    if !response.status().is_success() {
        return Err(Error::bad(format!(
            "The download for {} {} answered {}.",
            entry.name,
            entry.version,
            response.status()
        )));
    }
    let mismatch = || {
        Error::bad(format!(
            "The download for {} {} is not the package the registry reviewed, so LOKI didn't install it.",
            entry.name, entry.version
        ))
    };
    if response.content_length().is_some_and(|l| l != entry.size) {
        return Err(mismatch());
    }
    let mut file = tokio::fs::File::create_new(path).await?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(failed)? {
        total += chunk.len() as u64;
        if total > entry.size || total > MAX_PACKAGE {
            return Err(mismatch());
        }
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    if total != entry.size || hex::encode(hash.finalize()) != entry.sha256 {
        return Err(mismatch());
    }
    Ok(())
}

fn package_manifest(path: &Path) -> Result<apps::Manifest> {
    let invalid = || Error::bad("The downloaded package is not a valid LOKI app.");
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path)?).map_err(|_| invalid())?;
    let mut data = String::new();
    archive
        .by_name("manifest.json")
        .map_err(|_| invalid())?
        .take(131072)
        .read_to_string(&mut data)
        .map_err(|_| invalid())?;
    serde_json::from_str(&data).map_err(|_| invalid())
}

/// An app's registry icon as a 128-pixel PNG, cached until the index changes.
pub async fn icon(state: &State, id: &str) -> Result<Vec<u8>> {
    let cache = index(state, false).await?;
    let entry = cache
        .apps
        .iter()
        .find(|e| e.id == id)
        .ok_or_else(Error::not_found)?;
    let icon = entry.icon.as_deref().ok_or_else(Error::not_found)?;
    let source = url::Url::parse(&cache.url)?.join(icon)?;
    let key = hex::encode(&Sha256::digest(format!("{source} {}", cache.generated))[..4]);
    let dir = state.0.data.join("registry").join("icons");
    let path = dir.join(format!("{id}-{key}.png"));
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return Ok(bytes);
    }
    let response = client(Some(Duration::from_secs(15)))
        .get(source)
        .send()
        .await
        .ok()
        .filter(|r| r.status().is_success())
        .ok_or_else(Error::not_found)?;
    let bytes = limited(response, MAX_ICON)
        .await
        .map_err(|_| Error::not_found())?;
    let png = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(1024);
        limits.max_image_height = Some(1024);
        limits.max_alloc = Some(16 * 1024 * 1024);
        reader.limits(limits);
        let image = reader.decode().map_err(|_| Error::not_found())?;
        let mut out = std::io::Cursor::new(Vec::new());
        image
            .thumbnail(128, 128)
            .write_to(&mut out, image::ImageFormat::Png)?;
        Ok(out.into_inner())
    })
    .await??;
    tokio::fs::create_dir_all(&dir).await?;
    let prefix = format!("{id}-");
    if let Ok(mut old) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(file)) = old.next_entry().await {
            if file.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = tokio::fs::remove_file(file.path()).await;
            }
        }
    }
    tokio::fs::write(&path, &png).await?;
    Ok(png)
}

/// Checks the registry for app updates now and then, so the dashboard can offer them.
pub async fn maintain(state: State) {
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        if !state.0.settings.read().await.registry_url.is_empty() {
            let _ = index(&state, true).await;
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str, sdk: &str) -> Entry {
        Entry {
            id: "notes".into(),
            name: "Notes".into(),
            description: String::new(),
            source: String::new(),
            version: version.into(),
            url: "https://example.com/notes.loki-app".into(),
            sha256: "a".repeat(64),
            size: 1,
            sdk: sdk.into(),
            capabilities: vec![],
            icon: None,
        }
    }

    #[test]
    fn statuses() {
        let sdk = env!("CARGO_PKG_VERSION");
        assert_eq!(status(&entry("1.0.0", sdk), None), Status::Install);
        assert_eq!(
            status(&entry("1.1.0", sdk), Some(("1.0.0", "registry"))),
            Status::Update
        );
        assert_eq!(
            status(&entry("1.0.0", sdk), Some(("1.0.0", "registry"))),
            Status::Installed
        );
        assert_eq!(
            status(&entry("1.0.0", sdk), Some(("1.1.0", "registry"))),
            Status::Installed
        );
        assert_eq!(
            status(&entry("1.0.0", sdk), Some(("1.0.0", "upload"))),
            Status::Replace
        );
        assert_eq!(
            status(&entry("1.0.0", sdk), Some(("2.0.0", "upload"))),
            Status::Installed
        );
        assert!(matches!(
            status(&entry("1.0.0", "99.0.0"), None),
            Status::Unsupported(_)
        ));
    }

    #[test]
    fn registry_addresses() {
        assert_eq!(validate_url("").unwrap(), "");
        assert_eq!(
            validate_url("https://loki-server.github.io/apps").unwrap(),
            "https://loki-server.github.io/apps/"
        );
        assert!(validate_url("http://127.0.0.1:8080/").is_ok());
        assert!(validate_url("http://localhost/").is_ok());
        for bad in [
            "http://example.com/",
            "ftp://example.com/",
            "https://example.com/?x=1",
            "https://user:pw@example.com/",
            "not a url",
        ] {
            assert!(validate_url(bad).is_err(), "{bad}");
        }
    }
}
