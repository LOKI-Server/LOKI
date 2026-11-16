use crate::{Error, Result, State, auth, sync};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, RawQuery, State as ExtractState},
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::{get as route_get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf, time::Duration};

pub(crate) fn router() -> Router<State> {
    Router::new()
        .route("/api/personal", route_get(summary))
        .route("/api/personal/export", route_get(export))
        .route("/api/personal/settings/apps/{app}", post(enable))
}

async fn summary(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.approved()?;
    let folders: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT app_id FROM personal_files WHERE user_id=? ORDER BY app_id",
    )
    .bind(&user.id)
    .fetch_all(&state.0.db)
    .await?;
    let saves: Vec<(String, String)> =
        sqlx::query_as("SELECT id,name FROM sync_folders WHERE user_id=? ORDER BY name")
            .bind(&user.id)
            .fetch_all(&state.0.db)
            .await?;
    Ok(Json(json!({"folders":folders,"saves":saves})))
}

#[derive(Deserialize)]
struct Enable {
    enabled: bool,
}

async fn enable(
    ExtractState(state): ExtractState<State>,
    Path(app): Path<String>,
    headers: HeaderMap,
    Json(input): Json<Enable>,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let owner = ctx.owner()?;
    let _guard = state.0.personal_files.write().await;
    let installed = crate::apps::get(&state, &app).await?;
    let manifest: crate::apps::Manifest = serde_json::from_str(&installed.manifest)?;
    if !manifest
        .capabilities
        .iter()
        .any(|name| name == "personal-storage")
    {
        return Err(Error::bad("This app does not declare personal storage."));
    }
    if input.enabled {
        sqlx::query("INSERT OR IGNORE INTO personal_apps(app_id) VALUES(?)")
            .bind(&app)
            .execute(&state.0.db)
            .await?;
    } else {
        sqlx::query("DELETE FROM personal_apps WHERE app_id=?")
            .bind(&app)
            .execute(&state.0.db)
            .await?;
    }
    state.audit(&owner.id, "personal-app-access", &app).await?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Default)]
struct Selection {
    apps: Vec<String>,
    saves: Vec<String>,
    profile: bool,
}

enum Entry {
    Personal(String),
    Save(String),
    Json(Value),
}

const MAX_EXPORT_ENTRIES: usize = 20_000;

async fn export(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response> {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| value != "same-origin" && value != "none")
    {
        return Err(Error::forbidden());
    }
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.approved()?;
    let mut input = Selection::default();
    for (key, value) in url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
    {
        match key.as_ref() {
            "app" => input.apps.push(value.into_owned()),
            "save" => input.saves.push(value.into_owned()),
            "profile" => input.profile = value == "1",
            _ => return Err(Error::bad("Unknown export selection.")),
        }
    }
    if input.apps.len() + input.saves.len() > 1000
        || (input.apps.is_empty() && input.saves.is_empty() && !input.profile)
    {
        return Err(Error::bad("Choose at least one item to export."));
    }
    let permit = state.0.exports.clone().try_acquire_owned().map_err(|_| {
        Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Try again when another export finishes.",
        )
    })?;
    let state_for_stream = state.clone();
    // One read transaction fixes the file list. Streaming holds no storage locks,
    // so a slow download never stalls app writes or Save Sync. Bodies are
    // immutable; one replaced or removed meanwhile is listed as skipped.
    let mut tx = state.0.db.begin().await?;
    let mut entries = Vec::new();
    let mut sum = 0u64;
    for app in input
        .apps
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
    {
        if !crate::apps::valid_id(&app) {
            return Err(Error::bad("Invalid app selection."));
        }
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT path,blob,size FROM personal_files WHERE user_id=? AND app_id=? ORDER BY path LIMIT ?",
        )
        .bind(&user.id)
        .bind(&app)
        .bind((MAX_EXPORT_ENTRIES - entries.len() + 1) as i64)
        .fetch_all(&mut *tx)
        .await?;
        export_limits(sum, entries.len() + rows.len())?;
        for (path, blob, size) in rows {
            super::checked_path(&path)?;
            sum = sum.saturating_add(size as u64);
            entries.push((format!("apps/{app}/{path}"), Entry::Personal(blob)));
        }
    }
    for folder in input
        .saves
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
    {
        let name: String =
            sqlx::query_scalar("SELECT name FROM sync_folders WHERE user_id=? AND id=?")
                .bind(&user.id)
                .bind(&folder)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(Error::not_found)?;
        if !crate::apps::safe_relative(&folder) || folder.contains('/') {
            return Err(Error::bad("Invalid save folder."));
        }
        let rows: Vec<(String, String, i64)> = sqlx::query_as("SELECT path,hash,size FROM sync_files WHERE folder=? AND hash IS NOT NULL ORDER BY path LIMIT 20001")
            .bind(&folder).fetch_all(&mut *tx).await?;
        export_limits(sum, entries.len() + rows.len())?;
        entries.push((
            format!("saves/{folder}/folder.json"),
            Entry::Json(json!({"name":name})),
        ));
        for (path, hash, size) in rows {
            sync::valid_path(&path)?;
            if !sync::valid_hash(&hash) {
                return Err(Error::not_found());
            }
            sum = sum.saturating_add(size as u64);
            entries.push((format!("saves/{folder}/files/{path}"), Entry::Save(hash)));
        }
    }
    if input.profile {
        entries.push(("profile.json".into(), Entry::Json(json!({"username":user.username,"display_name":user.display_name,"email":user.email,"bio":user.bio,"created":user.created}))));
    }
    export_limits(sum, entries.len())?;
    tx.commit().await?;
    let (write, read) = tokio::io::duplex(128 * 1024);
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let data = state_for_stream.0.data.clone();
        if let Err(error) = write_zip(
            data,
            entries,
            ExportWriter {
                write,
                handle,
                deadline: tokio::time::Instant::now() + Duration::from_secs(600),
            },
        ) {
            tracing::debug!(%error,"personal export ended before completion");
        }
    });
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"loki-personal-data.zip\"",
        )
        .body(Body::from_stream(tokio_util::io::ReaderStream::new(read)))?)
}

fn export_limits(bytes: u64, entries: usize) -> Result<()> {
    if bytes > 50 * 1024 * 1024 * 1024 || entries > MAX_EXPORT_ENTRIES {
        return Err(Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Select fewer folders. Each export supports up to 50 GiB and 20,000 files.",
        ));
    }
    Ok(())
}

struct ExportWriter {
    write: tokio::io::DuplexStream,
    handle: tokio::runtime::Handle,
    deadline: tokio::time::Instant,
}

impl Write for ExportWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        use tokio::io::AsyncWriteExt;
        self.handle.block_on(async {
            tokio::time::timeout_at(self.deadline, self.write.write(bytes)).await?
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_zip(
    data: PathBuf,
    entries: Vec<(String, Entry)>,
    write: ExportWriter,
) -> anyhow::Result<()> {
    let personal = super::directory_path(&data).map_err(|e| anyhow::anyhow!(e.message))?;
    let mut zip = zip::ZipWriter::new_stream(write);
    let (mut files, mut skipped) = (Vec::new(), Vec::new());
    let options = zip::write::SimpleFileOptions::default().large_file(true);
    for (name, entry) in entries {
        let source = match entry {
            Entry::Json(value) => {
                zip.start_file(name.as_str(), options)?;
                serde_json::to_writer(&mut zip, &value)?;
                files.push(name);
                continue;
            }
            Entry::Personal(blob) => super::valid_blob(&blob).then(|| personal.join(&blob)),
            Entry::Save(hash) => Some(sync::blobs::blob_path(&data, &hash)),
        };
        let mut body = match source.map(|path| super::open_body(&path)) {
            Some(Ok(file)) => file,
            Some(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                anyhow::bail!("could not read {name}: {error}");
            }
            _ => {
                skipped.push(name);
                continue;
            }
        };
        zip.start_file(name.as_str(), options)?;
        std::io::copy(&mut body, &mut zip)?;
        files.push(name);
    }
    zip.start_file("manifest.json", options)?;
    serde_json::to_writer(
        &mut zip,
        &json!({"version":1,"files":files,"skipped":skipped}),
    )?;
    zip.finish()?;
    Ok(())
}
