use crate::{Error, Result, State, auth, caddy};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use std::{
    collections::HashSet,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tokio::io::{AsyncRead, AsyncReadExt};

pub use loki_sdk::manifest::{Field, Manifest, safe_relative, valid_id};
#[derive(FromRow, Serialize)]
pub struct App {
    pub id: String,
    pub manifest: String,
    pub enabled: bool,
    pub hostname: String,
    pub config: String,
    pub port: i64,
    #[serde(skip_serializing)]
    pub secret: String,
    pub previous: Option<String>,
    #[serde(skip_serializing)]
    pub previous_config: Option<String>,
    pub error: String,
    /// An extra listener, such as `0.0.0.0:4767`, for a realtime app's sockets on the LAN.
    pub lan_bind: String,
}
pub struct Runtime {
    pub ready: bool,
    pub failures: u32,
    pub next_attempt: i64,
    pub component: Option<std::sync::Arc<crate::components::Runtime>>,
}
pub fn target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}
pub fn validate_manifest(m: &Manifest) -> Result<()> {
    loki_sdk::manifest::validate_manifest(m).map_err(|error| Error::bad(error.to_string()))
}
/// The oldest `loki-sdk` whose apps this LOKI runs. Apps built with it or any
/// later SDK keep working, so raise it only to deliberately end support for older
/// apps: delete their frozen test builds (`tests/fixtures/frozen`) in the same
/// change and announce it in the release notes. Those apps are then asked to
/// rebuild instead of failing.
pub const OLDEST_SDK: semver::Version = semver::Version::new(0, 3, 0);
/// Why this LOKI can't run an app with this runtime, built with this `loki-sdk`.
/// Checked on the raw manifest at import, before fields a newer SDK may add are
/// rejected, and on installed apps before they start.
pub fn unsupported(runtime: Option<&str>, sdk: Option<&str>) -> Option<String> {
    if runtime != Some("wasi") {
        return Some("This is a native app. LOKI 0.3 and later run WASI components only; import a WASI build of the app.".into());
    }
    let oldest = OLDEST_SDK;
    let Some(sdk) = sdk else {
        return Some(format!(
            "This app was built for a LOKI older than {}.{}. Rebuild it with loki-sdk {oldest} or newer (update its loki-sdk dependency and run cargo loki package), then import the new package.",
            oldest.major, oldest.minor
        ));
    };
    let Ok(sdk) = semver::Version::parse(sdk) else {
        return Some("The package's loki-sdk version is not a valid version.".into());
    };
    let loki = semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("LOKI's own version");
    if sdk < oldest {
        Some(format!(
            "This app was built with loki-sdk {sdk}, which LOKI {loki} no longer runs. Rebuild it with loki-sdk {oldest} or newer, then import the new package."
        ))
    } else if (sdk.major, sdk.minor) > (loki.major, loki.minor) {
        Some(format!(
            "This app was built with loki-sdk {sdk}, which needs LOKI {}.{} or newer. Update LOKI to run it.",
            sdk.major, sdk.minor
        ))
    } else {
        None
    }
}
fn unsupported_manifest(m: &Manifest) -> Option<String> {
    unsupported(
        Some(&m.runtime),
        m.built_with.as_ref().map(|b| b.sdk.as_str()),
    )
}
pub fn extract(package: &Path, dest: &Path) -> Result<Manifest> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(package)?)?;
    if archive.len() > 2048 {
        return Err(Error::bad("Too many package files."));
    }
    let manifest: Manifest = {
        let mut file = archive.by_name("manifest.json")?;
        if file.size() > 131072 {
            return Err(Error::bad("Manifest is too large."));
        }
        let mut data = String::new();
        file.read_to_string(&mut data)?;
        let raw: Value = serde_json::from_str(&data)
            .map_err(|_| Error::bad("The package manifest is not valid JSON."))?;
        if let Some(reason) = unsupported(
            Some(raw["runtime"].as_str().unwrap_or("native")),
            raw.pointer("/built_with/sdk").and_then(Value::as_str),
        ) {
            return Err(Error::bad(reason));
        }
        serde_json::from_value(raw)
            .map_err(|e| Error::bad(format!("Invalid app manifest: {e}.")))?
    };
    validate_manifest(&manifest)?;
    let mut seen = HashSet::new();
    let mut total = 0u64;
    std::fs::create_dir_all(dest)?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        // A name zip can't decode is empty, which is unsafe below.
        let name = file.name().unwrap_or_default().into_owned();
        if !safe_relative(&name)
            || !seen.insert(name.clone())
            || file.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
            || file.is_dir()
        {
            return Err(Error::bad(
                "Packages must contain unique regular files with safe relative paths.",
            ));
        }
        total = total
            .checked_add(file.size())
            .ok_or_else(|| Error::bad("Package is too large."))?;
        if total > 536870912 {
            return Err(Error::bad("Expanded package exceeds 512 MiB."));
        }
        if name == "manifest.json" {
            continue;
        }
        let expected = manifest
            .files
            .get(&name)
            .ok_or_else(|| Error::bad("Package contains an undeclared file."))?;
        if expected.len() != 64 {
            return Err(Error::bad("Invalid SHA-256 checksum."));
        }
        let path = dest.join(&name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?;
        let mut hash = Sha256::new();
        let mut bytes = [0u8; 65536];
        let mut count = 0u64;
        loop {
            let n = file.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            count += n as u64;
            if count > file.size() || count > 536870912 {
                return Err(Error::bad("Package exceeds declared size."));
            }
            hash.update(&bytes[..n]);
            output.write_all(&bytes[..n])?;
        }
        if hex::encode(hash.finalize()) != expected.to_ascii_lowercase() {
            return Err(Error::bad("Package checksum mismatch."));
        }
    }
    if manifest
        .files
        .keys()
        .any(|p| !seen.contains(p) || !safe_relative(p))
    {
        return Err(Error::bad("Package is missing a declared file."));
    }
    std::fs::write(
        dest.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let mut bytes = [0; 8];
    std::fs::File::open(dest.join(&manifest.entry))?.read_exact(&mut bytes)?;
    if bytes != [0, 97, 115, 109, 13, 0, 1, 0] {
        return Err(Error::bad(
            "The WASI entry must be a WebAssembly component, not a core module or native executable.",
        ));
    }
    Ok(manifest)
}
pub async fn get(state: &State, id: &str) -> Result<App> {
    sqlx::query_as("SELECT * FROM apps WHERE id=?")
        .bind(id)
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(Error::not_found)
}
pub fn public_url(app: &App) -> String {
    origin(&app.id, app.port, &app.hostname)
}
/// Without a hostname, an app shares LOKI's listener on its own `.localhost` origin.
pub fn origin(id: &str, port: i64, hostname: &str) -> String {
    if hostname.is_empty() {
        format!("http://{id}.localhost:{port}")
    } else {
        format!("https://{hostname}")
    }
}
pub async fn import(state: &State, package: PathBuf, actor: &str) -> Result<Manifest> {
    let _guard = state.0.operations.lock().await;
    let staged = state.0.data.join("staging").join(auth::id());
    let dest = staged.clone();
    let result = tokio::task::spawn_blocking(move || extract(&package, &dest)).await?;
    let m = match result {
        Ok(m) => m,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&staged).await;
            return Err(e);
        }
    };
    let existing = get(state, &m.id).await.ok();
    if let Some(app) = &existing {
        let old: Manifest = serde_json::from_str(&app.manifest)?;
        if semver::Version::parse(&m.version)? < semver::Version::parse(&old.version)? {
            let _ = tokio::fs::remove_dir_all(staged).await;
            return Err(Error::bad(
                "Import the same or a newer version of this app.",
            ));
        }
        quiesce(state, &m.id).await?;
        stop_inner(state, &m.id).await?;
        let snapshot = state
            .0
            .data
            .join("backups")
            .join(format!("app-{}-{}", m.id, auth::id()));
        let source = state.0.data.join("app-data").join(&m.id);
        let output = snapshot.clone();
        tokio::task::spawn_blocking(move || copy_tree(&source, &output)).await??;
        sqlx::query("UPDATE apps SET previous=?,previous_config=config WHERE id=?")
            .bind(snapshot.to_string_lossy().as_ref())
            .bind(&m.id)
            .execute(&state.0.db)
            .await?;
    }
    let install = state.0.data.join("apps").join(&m.id);
    let previous = state.0.data.join("apps").join(format!("{}.previous", m.id));
    if previous.exists() {
        tokio::fs::remove_dir_all(&previous).await?;
    }
    if install.exists() {
        tokio::fs::rename(&install, &previous).await?;
    }
    tokio::fs::rename(&staged, &install).await?;
    tokio::fs::create_dir_all(state.0.data.join("app-data").join(&m.id)).await?;
    if existing.is_none() {
        let port = state.0.bind.port();
        let secret = auth::random();
        let mut tx = state.0.db.begin().await?;
        sqlx::query("INSERT INTO resources(id,kind,name) VALUES(?,'app',?)")
            .bind(&m.id)
            .bind(&m.name)
            .execute(&mut *tx)
            .await?;
        let config: serde_json::Map<String, Value> = m
            .fields
            .iter()
            .map(|f| (f.key.clone(), f.default.clone()))
            .collect();
        sqlx::query("INSERT INTO apps(id,manifest,port,secret,config) VALUES(?,?,?,?,?)")
            .bind(&m.id)
            .bind(serde_json::to_string(&m)?)
            .bind(i64::from(port))
            .bind(&secret)
            .bind(serde_json::to_string(&config)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    } else if let Some(previous) = &existing {
        let old_config: Value = serde_json::from_str(&previous.config)?;
        let config: serde_json::Map<String, Value> = m
            .fields
            .iter()
            .map(|f| {
                (
                    f.key.clone(),
                    old_config.get(&f.key).unwrap_or(&f.default).clone(),
                )
            })
            .collect();
        sqlx::query("UPDATE apps SET manifest=?,config=?,error='' WHERE id=?")
            .bind(serde_json::to_string(&m)?)
            .bind(serde_json::to_string(&config)?)
            .bind(&m.id)
            .execute(&state.0.db)
            .await?;
        sqlx::query("UPDATE resources SET name=? WHERE id=?")
            .bind(&m.name)
            .bind(&m.id)
            .execute(&state.0.db)
            .await?;
        if existing.as_ref().is_some_and(|a| a.enabled)
            && let Err(error) = start_inner(state, &m.id).await
        {
            let detail = error.message;
            if let Err(restore) = rollback_inner(state, &m.id).await {
                return Err(Error::bad(format!(
                    "Update failed: {detail} {}",
                    restore.message
                )));
            }
            caddy::apply(state).await?;
            return Err(Error::bad(format!(
                "Update failed and the prior version/data were restored. {detail}"
            )));
        }
    }
    if let Err(error) = caddy::apply(state).await {
        if existing.is_some() {
            rollback_inner(state, &m.id).await?;
            caddy::apply(state).await?;
        }
        return Err(error);
    }
    state
        .audit(actor, "app-import", &format!("{} {}", m.id, m.version))
        .await?;
    Ok(m)
}
pub fn copy_tree(source: &Path, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    if !source.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let meta = entry.path().symlink_metadata()?;
        if meta.file_type().is_symlink() {
            anyhow::bail!("Snapshots cannot contain symlinks");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                anyhow::bail!("Snapshots cannot contain reparse points");
            }
        }
        let target = dest.join(entry.file_name());
        if meta.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if meta.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else {
            anyhow::bail!("Snapshots require regular files");
        }
    }
    Ok(())
}
pub(crate) async fn capture<R: AsyncRead + Unpin>(mut stream: R, path: PathBuf) {
    let mut buffer = [0u8; 8192];
    while let Ok(n) = stream.read(&mut buffer).await {
        if n == 0 {
            break;
        }
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1048576) {
            let previous = path.with_extension("previous.log");
            let _ = std::fs::remove_file(&previous);
            let _ = std::fs::rename(&path, previous);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(&buffer[..n]);
        }
    }
}
pub async fn start_inner(state: &State, id: &str) -> Result<()> {
    quiesce(state, id).await?;
    stop_inner(state, id).await?;
    let app = get(state, id).await?;
    let m: Manifest = serde_json::from_str(&app.manifest)?;
    validate_config(&m, &serde_json::from_str(&app.config)?)?;
    if let Some(reason) = unsupported_manifest(&m) {
        return Err(Error::bad(reason));
    }
    sqlx::query("UPDATE apps SET port=? WHERE id=?")
        .bind(i64::from(state.0.bind.port()))
        .bind(id)
        .execute(&state.0.db)
        .await?;
    let app = get(state, id).await?;
    let component = crate::components::start(state, &app, &m).await?;
    state.0.runtimes.lock().await.insert(
        id.into(),
        Runtime {
            component: Some(component),
            ready: true,
            failures: 0,
            next_attempt: 0,
        },
    );
    sqlx::query("UPDATE apps SET error='' WHERE id=?")
        .bind(id)
        .execute(&state.0.db)
        .await?;
    Ok(())
}
pub async fn stop_inner(state: &State, id: &str) -> Result<()> {
    let runtime = state.0.runtimes.lock().await.remove(id);
    if let Some(component) = runtime.as_ref().and_then(|r| r.component.as_ref()) {
        component.stop().await;
    }
    Ok(())
}
pub async fn action(state: &State, id: &str, action: &str) -> Result<()> {
    let _guard = state.0.operations.lock().await;
    let app = get(state, id).await?;
    match action {
        "enable" | "restart" => {
            start_inner(state, id).await?;
            sqlx::query("UPDATE apps SET enabled=1 WHERE id=?")
                .bind(id)
                .execute(&state.0.db)
                .await?;
            if let Err(e) = caddy::apply(state).await {
                sqlx::query("UPDATE apps SET enabled=? WHERE id=?")
                    .bind(app.enabled)
                    .bind(id)
                    .execute(&state.0.db)
                    .await?;
                stop_inner(state, id).await?;
                return Err(e);
            }
        }
        "disable" | "uninstall" => {
            sqlx::query("UPDATE apps SET enabled=0 WHERE id=?")
                .bind(id)
                .execute(&state.0.db)
                .await?;
            if let Err(e) = caddy::apply(state).await {
                sqlx::query("UPDATE apps SET enabled=? WHERE id=?")
                    .bind(app.enabled)
                    .bind(id)
                    .execute(&state.0.db)
                    .await?;
                return Err(e);
            }
            stop_inner(state, id).await?;
            if action == "uninstall" {
                sqlx::query("DELETE FROM resources WHERE id=?")
                    .bind(id)
                    .execute(&state.0.db)
                    .await?;
                tokio::fs::remove_dir_all(state.0.data.join("apps").join(id)).await?;
            }
        }
        "rollback" => {
            rollback_inner(state, id).await?;
            caddy::apply(state).await?;
        }
        _ => return Err(Error::bad("Unknown app action.")),
    }
    Ok(())
}
async fn rollback_inner(state: &State, id: &str) -> Result<()> {
    let app = get(state, id).await?;
    let snapshot = app
        .previous
        .ok_or_else(|| Error::bad("No prior app/data snapshot is available."))?;
    let install = state.0.data.join("apps").join(id);
    let previous = state.0.data.join("apps").join(format!("{id}.previous"));
    if !previous.exists() {
        return Err(Error::bad("Previous package is unavailable."));
    }
    let old: Manifest =
        serde_json::from_str(&tokio::fs::read_to_string(previous.join("manifest.json")).await?)?;
    if let Some(reason) = unsupported_manifest(&old) {
        return Err(Error::bad(format!(
            "The previous version can't run on this LOKI. {reason}"
        )));
    }
    stop_inner(state, id).await?;
    if install.exists() {
        tokio::fs::remove_dir_all(&install).await?;
    }
    tokio::fs::rename(&previous, &install).await?;
    let data = state.0.data.join("app-data").join(id);
    if data.exists() {
        tokio::fs::remove_dir_all(&data).await?;
    }
    let snapshot = PathBuf::from(snapshot);
    tokio::task::spawn_blocking(move || copy_tree(&snapshot, &data)).await??;
    let manifest = tokio::fs::read_to_string(install.join("manifest.json")).await?;
    let old: Manifest = serde_json::from_str(&manifest)?;
    sqlx::query("UPDATE resources SET name=? WHERE id=?")
        .bind(old.name)
        .bind(id)
        .execute(&state.0.db)
        .await?;
    sqlx::query("UPDATE apps SET manifest=?,config=COALESCE(previous_config,config),previous=NULL,previous_config=NULL,error='' WHERE id=?")
        .bind(manifest)
        .bind(id)
        .execute(&state.0.db)
        .await?;
    if app.enabled {
        start_inner(state, id).await?;
    }
    Ok(())
}
pub fn validate_config(manifest: &Manifest, value: &Value) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::bad("App settings must be an object."))?;
    if object
        .keys()
        .any(|k| !manifest.fields.iter().any(|f| &f.key == k))
    {
        return Err(Error::bad("Unknown app setting."));
    }
    for field in &manifest.fields {
        let value = object.get(&field.key).unwrap_or(&field.default);
        if value.is_null() && !field.required {
            continue;
        }
        let valid = match field.kind.as_str() {
            "text" | "secret" => value
                .as_str()
                .is_some_and(|s| s.len() <= 4096 && (!field.required || !s.is_empty())),
            "number" => value.is_number(),
            "bool" => value.is_boolean(),
            _ => false,
        };
        if !valid {
            return Err(Error::bad(format!("Invalid value for {}.", field.label)));
        }
    }
    Ok(())
}
pub async fn quiesce(state: &State, id: &str) -> Result<()> {
    let had_ready = {
        let mut runtimes = state.0.runtimes.lock().await;
        if let Some(r) = runtimes.get_mut(id) {
            let ready = r.ready;
            r.ready = false;
            ready
        } else {
            false
        }
    };
    if had_ready && let Err(e) = caddy::apply(state).await {
        if let Some(r) = state.0.runtimes.lock().await.get_mut(id) {
            r.ready = true;
        }
        return Err(e);
    }
    Ok(())
}

pub async fn monitor(state: State) {
    let mut ticks = 0u32;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        if state
            .0
            .maintenance
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            continue;
        }
        let Ok(_guard) = state.0.operations.try_lock() else {
            continue;
        };
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(20) {
            for table in [
                "sessions",
                "codes",
                "tokens",
                "token_families",
                "gateway_flows",
                "gateway_sessions",
                "resets",
            ] {
                let _ = sqlx::query(&format!("DELETE FROM {table} WHERE expires<?"))
                    .bind(auth::now())
                    .execute(&state.0.db)
                    .await;
            }
            let _=sqlx::query("DELETE FROM audit WHERE id NOT IN(SELECT id FROM audit ORDER BY id DESC LIMIT 10000)").execute(&state.0.db).await;
        }
        if ticks.is_multiple_of(5) {
            let dead = {
                let mut c = state.0.caddy.lock().await;
                c.as_mut()
                    .is_some_and(|p| p.child.try_wait().ok().flatten().is_some())
            };
            if dead && let Err(e) = caddy::apply(&state).await {
                tracing::warn!(error=%e.message,"could not restart Caddy");
            }
        }
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM apps WHERE enabled=1")
            .fetch_all(&state.0.db)
            .await
            .unwrap_or_default();
        for id in ids {
            // Enabled apps without a running component start here, with backoff.
            let (mut restart, mut failures) = (true, 0);
            {
                let runtimes = state.0.runtimes.lock().await;
                if let Some(r) = runtimes.get(&id) {
                    if r.component.is_some() {
                        continue;
                    }
                    failures = r.failures;
                    if r.next_attempt > auth::now() {
                        restart = false;
                    }
                }
            }
            if restart {
                match start_inner(&state, &id).await {
                    Ok(()) => {
                        if let Some(r) = state.0.runtimes.lock().await.get_mut(&id) {
                            r.failures = failures;
                        }
                        let _ = caddy::apply(&state).await;
                    }
                    Err(e) => {
                        failures += 1;
                        state.0.runtimes.lock().await.insert(
                            id.clone(),
                            Runtime {
                                ready: false,
                                failures,
                                next_attempt: auth::now() + 2i64.pow(failures.min(8)),
                                component: None,
                            },
                        );
                        let _ = sqlx::query("UPDATE apps SET error=? WHERE id=?")
                            .bind(e.message)
                            .bind(&id)
                            .execute(&state.0.db)
                            .await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn package_paths() {
        for s in ["../evil", "/root", "C:/x", "a\\b", "a/../b", "./x", "a//b"] {
            assert!(!safe_relative(s), "{s}");
        }
        assert!(safe_relative("bin/app.exe"));
    }
    #[test]
    fn apps_must_be_wasi_built_with_a_supported_sdk() {
        let wasi = Some("wasi");
        assert!(
            unsupported(Some("native"), Some("0.3.0"))
                .unwrap()
                .contains("native app")
        );
        assert!(unsupported(wasi, None).unwrap().contains("older than 0.3"));
        assert!(
            unsupported(wasi, Some("soon"))
                .unwrap()
                .contains("not a valid")
        );
        assert!(
            unsupported(wasi, Some("0.2.9"))
                .unwrap()
                .contains("no longer runs")
        );
        assert_eq!(unsupported(wasi, Some("0.3.0")), None);
        let loki = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let patch = format!("{}.{}.{}", loki.major, loki.minor, loki.patch + 7);
        assert_eq!(unsupported(wasi, Some(&patch)), None);
        let next = format!("{}.{}.0", loki.major, loki.minor + 1);
        assert!(
            unsupported(wasi, Some(&next))
                .unwrap()
                .contains("Update LOKI")
        );
    }
    #[tokio::test]
    async fn native_apps_from_earlier_versions_say_why_they_cannot_start() {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::open(tmp.path().into(), "127.0.0.1:7070".parse().unwrap())
            .await
            .unwrap();
        let manifest = serde_json::json!({"protocol":1,"id":"branch","name":"Branch","version":"1.0.0","target":"x86_64-windows","entry":"branch.exe","health":"/health","files":{"branch.exe":"00"}});
        sqlx::query("INSERT INTO resources(id,kind,name) VALUES('branch','app','Branch')")
            .execute(&state.0.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO apps(id,manifest,port,secret) VALUES('branch',?,40000,'')")
            .bind(manifest.to_string())
            .execute(&state.0.db)
            .await
            .unwrap();
        let error = action(&state, "branch", "enable").await.unwrap_err();
        assert!(
            error.message.contains("WASI components only"),
            "{}",
            error.message
        );
        assert!(!get(&state, "branch").await.unwrap().enabled);
        state.0.db.close().await;
    }
    #[test]
    fn snapshot_copies_regular_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("data"), "hi").unwrap();
        let dest = temp.path().join("dest");
        copy_tree(&source, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("data")).unwrap(), "hi");
    }
}
