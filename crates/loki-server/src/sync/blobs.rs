//! Content-addressed file bodies under `sync/blobs/ab/<sha256>`. Blobs are shared
//! on disk, but each user may only see or reference hashes they uploaded themselves.
use super::{Caller, caller, max_file, quota, usage, valid_hash};
use crate::{Error, Result, State, auth, files};
use axum::{
    body::Body,
    extract::{Path as RoutePath, State as ExtractState},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

pub fn blob_path(data: &Path, hash: &str) -> PathBuf {
    data.join("sync/blobs").join(&hash[..2]).join(hash)
}
fn unknown_blob() -> Error {
    Error::coded(
        StatusCode::NOT_FOUND,
        "unknown_blob",
        "Upload this blob before using it.",
    )
}
/// The size of a blob this user may read, if any.
pub async fn owned(state: &State, user: &str, hash: &str) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT size FROM sync_uploads WHERE user_id=? AND hash=?")
            .bind(user)
            .bind(hash)
            .fetch_optional(&state.0.db)
            .await?,
    )
}

pub async fn download(
    ExtractState(state): ExtractState<State>,
    RoutePath(hash): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let Caller { user, .. } = caller(&state, &headers).await?;
    if !valid_hash(&hash) || owned(&state, &user.id, &hash).await?.is_none() {
        return Err(unknown_blob());
    }
    stream(&state, &hash, &headers, None).await
}
/// Streams a blob with single-range support. Browsers always receive an attachment.
pub async fn stream(
    state: &State,
    hash: &str,
    headers: &HeaderMap,
    filename: Option<&str>,
) -> Result<Response> {
    let mut file = tokio::fs::File::open(blob_path(&state.0.data, hash))
        .await
        .map_err(|_| unknown_blob())?;
    let length = file.metadata().await?.len();
    let mut status = StatusCode::OK;
    let (start, end) = match headers.get(header::RANGE) {
        Some(value) => match value.to_str().ok().and_then(|s| files::range(s, length)) {
            Some(range) => {
                status = StatusCode::PARTIAL_CONTENT;
                range
            }
            None => {
                return Ok((
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{length}"))],
                )
                    .into_response());
            }
        },
        None => (0, length.saturating_sub(1)),
    };
    let count = if length == 0 { 0 } else { end - start + 1 };
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, count)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, format!("\"{hash}\""))
        .header(header::CACHE_CONTROL, "private, no-store")
        .header("Content-Security-Policy", "sandbox; default-src 'none'");
    if let Some(name) = filename {
        let encoded = url::form_urlencoded::byte_serialize(name.as_bytes())
            .collect::<String>()
            .replace('+', "%20");
        response = response.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"download\"; filename*=UTF-8''{encoded}"),
        );
    }
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{length}"),
        );
    }
    Ok(response.body(Body::from_stream(ReaderStream::with_capacity(
        file.take(count),
        65536,
    )))?)
}

pub async fn upload(
    ExtractState(state): ExtractState<State>,
    RoutePath(hash): RoutePath<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let Caller { user, .. } = caller(&state, &headers).await?;
    if !valid_hash(&hash) {
        return Err(Error::coded(
            StatusCode::BAD_REQUEST,
            "invalid_hash",
            "Name blobs by their lowercase SHA-256 hex digest.",
        ));
    }
    if owned(&state, &user.id, &hash).await?.is_some()
        && tokio::fs::try_exists(blob_path(&state.0.data, &hash)).await?
    {
        return Ok((
            StatusCode::OK,
            axum::Json(serde_json::json!({"ok":true,"existing":true})),
        )
            .into_response());
    }
    let max = max_file(&state).await;
    let room = quota(&state, &user.id).await? - usage(&state, &user.id).await?;
    let too_large = || {
        Error::coded(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!(
                "Files may be at most {} MiB on this server.",
                max / super::MIB
            ),
        )
    };
    let over_quota = || {
        Error::coded(
            StatusCode::PAYLOAD_TOO_LARGE,
            "quota_exceeded",
            "Your sync storage is full. Delete folders or ask the server owner for more space.",
        )
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    if declared.is_some_and(|n| n > max) {
        return Err(too_large());
    }
    if declared.is_some_and(|n| n > room) {
        return Err(over_quota());
    }
    let temporary = state.0.data.join("sync/staging").join(auth::id());
    let received = async {
        let mut out = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        let mut hasher = Sha256::new();
        let mut total = 0i64;
        let mut chunks = body.into_data_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|_| Error::bad("The upload was interrupted."))?;
            total += chunk.len() as i64;
            if total > max {
                return Err(too_large());
            }
            if total > room {
                return Err(over_quota());
            }
            hasher.update(&chunk);
            out.write_all(&chunk).await?;
        }
        out.sync_all().await?;
        if hex::encode(hasher.finalize()) != hash {
            return Err(Error::coded(
                StatusCode::BAD_REQUEST,
                "hash_mismatch",
                "The uploaded bytes do not match the blob name.",
            ));
        }
        Ok::<_, Error>(total)
    }
    .await;
    let size = match received {
        Ok(size) => size,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
    };
    // Garbage collection cannot remove the blob between placing and recording it.
    let _blobs = state.0.sync_blobs.read().await;
    let dest = blob_path(&state.0.data, &hash);
    if tokio::fs::try_exists(&dest).await? {
        let _ = tokio::fs::remove_file(&temporary).await;
    } else {
        tokio::fs::create_dir_all(dest.parent().unwrap_or(&dest)).await?;
        if let Err(error) = tokio::fs::rename(&temporary, &dest).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            // A concurrent upload of identical bytes may have won the rename.
            if !tokio::fs::try_exists(&dest).await? {
                return Err(error.into());
            }
        }
    }
    let quota = quota(&state, &user.id).await?;
    let recorded = sqlx::query("INSERT OR IGNORE INTO sync_uploads(user_id,hash,size,created) SELECT ?,?,?,? WHERE (SELECT coalesce(sum(size),0) FROM sync_uploads WHERE user_id=?)+?<=?")
        .bind(&user.id)
        .bind(&hash)
        .bind(size)
        .bind(auth::now())
        .bind(&user.id)
        .bind(size)
        .bind(quota)
        .execute(&state.0.db)
        .await?;
    if recorded.rows_affected() == 0 && owned(&state, &user.id, &hash).await?.is_none() {
        return Err(over_quota());
    }
    Ok((
        StatusCode::CREATED,
        axum::Json(serde_json::json!({"ok":true,"size":size})),
    )
        .into_response())
}

/// Removes expired pairings, uploads nothing references after a day, and blob
/// files no user references. Holds the operations lock so backups stay consistent.
pub async fn collect(state: &State) -> Result<u64> {
    let _operations = state.0.operations.lock().await;
    let _blobs = state.0.sync_blobs.write().await;
    sqlx::query("DELETE FROM sync_pairings WHERE expires<?")
        .bind(auth::now())
        .execute(&state.0.db)
        .await?;
    sqlx::query("DELETE FROM sync_uploads WHERE created<? AND NOT EXISTS(SELECT 1 FROM sync_files f JOIN sync_folders d ON d.id=f.folder WHERE d.user_id=sync_uploads.user_id AND f.hash=sync_uploads.hash) AND NOT EXISTS(SELECT 1 FROM sync_history h JOIN sync_folders d ON d.id=h.folder WHERE d.user_id=sync_uploads.user_id AND h.hash=sync_uploads.hash)")
        .bind(auth::now() - 86400)
        .execute(&state.0.db)
        .await?;
    let live: std::collections::HashSet<String> =
        sqlx::query_scalar("SELECT DISTINCT hash FROM sync_uploads")
            .fetch_all(&state.0.db)
            .await?
            .into_iter()
            .collect();
    let root = state.0.data.join("sync/blobs");
    Ok(tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let mut removed = 0;
        for shard in std::fs::read_dir(&root)?.flatten() {
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for blob in std::fs::read_dir(shard.path())?.flatten() {
                let name = blob.file_name().to_string_lossy().into_owned();
                if !live.contains(&name) && blob.file_type()?.is_file() {
                    std::fs::remove_file(blob.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    })
    .await??)
}
