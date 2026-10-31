//! Per-user file sync. The server is authoritative: devices upload content-addressed
//! blobs, then commit path changes against the version they last saw. Clients speak
//! the bearer-only `/sync/v1` API; the dashboard uses cookie routes under `/api/sync`.
mod api;
mod blobs;
mod devices;

use crate::{Error, Result, State, apps, auth, auth::User};
use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::Row;

pub const MIB: i64 = 1024 * 1024;
/// The resource whose grants decide who may sync.
pub const RESOURCE: &str = "sync";

pub fn router() -> Router<State> {
    Router::new()
        .route("/sync/v1", get(api::discovery))
        .route("/sync/v1/pair", post(devices::pair))
        .route("/sync/v1/pair/poll", post(devices::poll))
        .route("/sync/v1/login", post(devices::login))
        .route("/sync/v1/me", get(devices::me))
        .route("/sync/v1/device", axum::routing::delete(devices::sign_out))
        .route(
            "/sync/v1/folders",
            get(api::folders).post(api::create_folder),
        )
        .route(
            "/sync/v1/folders/{id}",
            axum::routing::patch(api::update_folder),
        )
        .route("/sync/v1/folders/{id}/changes", get(api::changes))
        .route(
            "/sync/v1/folders/{id}/commit",
            post(api::commit).layer(DefaultBodyLimit::max(4 * MIB as usize)),
        )
        .route("/sync/v1/folders/{id}/history", get(api::history))
        .route("/sync/v1/folders/{id}/restore", post(api::restore))
        .route(
            "/sync/v1/blobs/{hash}",
            get(blobs::download).put(blobs::upload),
        )
        .route("/api/sync/folders/{id}", get(api::browse))
        .route("/api/sync/folders/{id}/file", get(api::browser_file))
        .route("/api/sync/folders/{id}/history", get(api::browser_history))
}

/// A device acting for its user. Built only from a bearer token, never a cookie,
/// so these routes need no CSRF protection.
pub(crate) struct Caller {
    pub user: User,
    pub device: String,
}
pub(crate) async fn caller(state: &State, headers: &HeaderMap) -> Result<Caller> {
    let signed_out = || {
        Error::coded(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "This device is signed out. Pair it again.",
        )
    };
    let raw = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(signed_out)?;
    let device: Option<(String, String, i64, i64)> =
        sqlx::query_as("SELECT id,user_id,auth_version,last_seen FROM sync_devices WHERE token=?")
            .bind(auth::digest(raw.trim()))
            .fetch_optional(&state.0.db)
            .await?;
    let (device, user_id, version, last_seen) = device.ok_or_else(signed_out)?;
    let user: User =
        sqlx::query_as("SELECT * FROM users WHERE id=? AND auth_version=? AND status='approved'")
            .bind(&user_id)
            .bind(version)
            .fetch_optional(&state.0.db)
            .await?
            .ok_or_else(signed_out)?;
    if !auth::allowed(state, Some(&user), RESOURCE, "access").await? {
        return Err(Error::coded(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Your account no longer has access to sync. Ask the server owner.",
        ));
    }
    if last_seen < auth::now() - 60 {
        sqlx::query("UPDATE sync_devices SET last_seen=? WHERE id=?")
            .bind(auth::now())
            .bind(&device)
            .execute(&state.0.db)
            .await?;
    }
    Ok(Caller { user, device })
}
/// The signed-in browser user, when they may use sync.
pub(crate) async fn browser(state: &State, headers: &HeaderMap) -> Result<User> {
    let ctx = auth::context(state, headers).await?;
    let user = ctx.approved()?.clone();
    if !auth::allowed(state, Some(&user), RESOURCE, "access").await? {
        return Err(Error::forbidden());
    }
    Ok(user)
}

pub(crate) async fn quota(state: &State, user: &str) -> Result<i64> {
    let custom: Option<i64> = sqlx::query_scalar("SELECT bytes FROM sync_quotas WHERE user_id=?")
        .bind(user)
        .fetch_optional(&state.0.db)
        .await?;
    Ok(match custom {
        Some(bytes) => bytes,
        None => (state.0.settings.read().await.sync_quota_mb as i64).saturating_mul(MIB),
    })
}
pub(crate) async fn usage(state: &State, user: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT coalesce(sum(size),0) FROM sync_uploads WHERE user_id=?")
            .bind(user)
            .fetch_one(&state.0.db)
            .await?,
    )
}
pub(crate) async fn max_file(state: &State) -> i64 {
    (state.0.settings.read().await.sync_max_file_mb as i64).saturating_mul(MIB)
}

/// Logical paths are shared by Windows, Android, macOS, and Linux devices, so they
/// must be valid, NFC-normalized relative paths on all of them.
pub fn valid_path(path: &str) -> Result<()> {
    use unicode_normalization::UnicodeNormalization;
    if path.len() > 1024
        || !apps::safe_relative(path)
        || path.chars().any(char::is_control)
        || path.nfc().ne(path.chars())
    {
        return Err(Error::coded(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            format!(
                "Unsupported path: {path}. Use NFC, forward slashes, and names valid on Windows."
            ),
        ));
    }
    Ok(())
}
pub fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn valid_name(name: &str, max: usize) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > max || name.chars().any(char::is_control) {
        return Err(Error::bad(format!("Enter a name of 1–{max} characters.")));
    }
    Ok(name)
}

/// Sync details embedded in the dashboard state for users who may sync.
pub async fn summary(state: &State, user: &User) -> Result<Option<Value>> {
    if user.status != "approved" || !auth::allowed(state, Some(user), RESOURCE, "access").await? {
        return Ok(None);
    }
    let devices = sqlx::query("SELECT id,name,platform,created,last_seen FROM sync_devices WHERE user_id=? ORDER BY last_seen DESC")
        .bind(&user.id)
        .fetch_all(&state.0.db)
        .await?
        .into_iter()
        .map(|r| json!({"id":r.get::<String,_>("id"),"name":r.get::<String,_>("name"),"platform":r.get::<String,_>("platform"),"created":r.get::<i64,_>("created"),"last_seen":r.get::<i64,_>("last_seen")}))
        .collect::<Vec<_>>();
    let folders = sqlx::query("SELECT d.id,d.name,d.kind,d.keep_versions,d.seq,(SELECT count(*) FROM sync_files f WHERE f.folder=d.id AND f.hash IS NOT NULL) files,(SELECT coalesce(sum(size),0) FROM sync_files f WHERE f.folder=d.id AND f.hash IS NOT NULL) bytes,(SELECT count(*) FROM sync_history h WHERE h.folder=d.id AND h.resolved=0) conflicts FROM sync_folders d WHERE d.user_id=? ORDER BY d.name COLLATE NOCASE")
        .bind(&user.id)
        .fetch_all(&state.0.db)
        .await?
        .into_iter()
        .map(|r| json!({"id":r.get::<String,_>("id"),"name":r.get::<String,_>("name"),"kind":r.get::<String,_>("kind"),"keep_versions":r.get::<i64,_>("keep_versions"),"seq":r.get::<i64,_>("seq"),"files":r.get::<i64,_>("files"),"bytes":r.get::<i64,_>("bytes"),"conflicts":r.get::<i64,_>("conflicts")}))
        .collect::<Vec<_>>();
    let mut value = json!({"devices":devices,"folders":folders,"usage":usage(state,&user.id).await?,"quota":quota(state,&user.id).await?,"max_file_bytes":max_file(state).await});
    if user.owner {
        value["quotas"] = json!(
            sqlx::query_as::<_, (String, i64)>("SELECT user_id,bytes FROM sync_quotas")
                .fetch_all(&state.0.db)
                .await?
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>()
        );
    }
    Ok(Some(value))
}

/// Dashboard actions named `sync-*`. The caller is an approved, signed-in user.
pub async fn action(
    state: &State,
    user: &User,
    name: &str,
    id: &str,
    data: &Value,
) -> Result<Value> {
    if name == "sync-quota" {
        if !user.owner {
            return Err(Error::forbidden());
        }
        match data["mb"].as_i64() {
            Some(mb) if (0..=100_000_000).contains(&mb) => {
                sqlx::query("INSERT INTO sync_quotas(user_id,bytes) VALUES(?,?) ON CONFLICT(user_id) DO UPDATE SET bytes=excluded.bytes")
                    .bind(id)
                    .bind(mb * MIB)
                    .execute(&state.0.db)
                    .await?;
            }
            None if data["mb"].is_null() => {
                sqlx::query("DELETE FROM sync_quotas WHERE user_id=?")
                    .bind(id)
                    .execute(&state.0.db)
                    .await?;
            }
            _ => {
                return Err(Error::bad(
                    "Enter a quota in MiB, or leave it blank for the default.",
                ));
            }
        }
        return Ok(json!({"ok":true}));
    }
    if !auth::allowed(state, Some(user), RESOURCE, "access").await? {
        return Err(Error::forbidden());
    }
    let text = |key: &str| data[key].as_str().unwrap_or("");
    match name {
        "sync-pair-lookup" => devices::lookup(state, user, text("code")).await,
        "sync-pair-approve" => devices::decide(state, user, text("code"), true).await,
        "sync-pair-deny" => devices::decide(state, user, text("code"), false).await,
        "sync-device-revoke" => {
            sqlx::query("DELETE FROM sync_devices WHERE id=? AND user_id=?")
                .bind(id)
                .bind(&user.id)
                .execute(&state.0.db)
                .await?;
            Ok(json!({"ok":true}))
        }
        "sync-device-rename" => {
            sqlx::query("UPDATE sync_devices SET name=? WHERE id=? AND user_id=?")
                .bind(valid_name(text("name"), 80)?)
                .bind(id)
                .bind(&user.id)
                .execute(&state.0.db)
                .await?;
            Ok(json!({"ok":true}))
        }
        "sync-folder-save" => {
            let folder = if id.is_empty() {
                api::new_folder(
                    state,
                    &user.id,
                    text("name"),
                    data["kind"].as_str().unwrap_or("generic"),
                )
                .await?
            } else {
                api::change_folder(
                    state,
                    &user.id,
                    id,
                    data["name"].as_str(),
                    data["keep_versions"].as_i64(),
                )
                .await?
            };
            Ok(json!({"ok":true,"id":folder["id"]}))
        }
        "sync-folder-delete" => {
            let _guard = state.0.operations.lock().await;
            let folder = api::folder(state, &user.id, id).await?;
            if text("confirm") != folder.name {
                return Err(Error::bad("Type the folder name to confirm deletion."));
            }
            sqlx::query("DELETE FROM sync_folders WHERE id=? AND user_id=?")
                .bind(id)
                .bind(&user.id)
                .execute(&state.0.db)
                .await?;
            state.wake_sync();
            Ok(json!({"ok":true}))
        }
        "sync-restore" => {
            api::restore_version(
                state,
                &user.id,
                id,
                data["history_id"].as_i64().unwrap_or(0),
                "web",
            )
            .await
        }
        "sync-conflict-resolve" => {
            let history = data["history_id"].as_i64().unwrap_or(0);
            if text("keep") == "this" {
                api::restore_version(state, &user.id, id, history, "web").await
            } else {
                api::dismiss_conflict(state, &user.id, id, history).await
            }
        }
        _ => Err(Error::bad("Unknown action.")),
    }
}

/// Hourly housekeeping: expired pairings, abandoned uploads, and unreferenced blobs.
pub async fn maintain(state: State) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
    interval.tick().await;
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    loop {
        if let Err(error) = blobs::collect(&state).await {
            tracing::error!(error = %error.1, "sync garbage collection failed");
        }
        interval.tick().await;
    }
}
pub use blobs::collect;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_must_be_portable() {
        for ok in [
            "saves/Pokémon Emerald.srm",
            "a",
            "deep/nested/file.sav",
            "名前.sav",
        ] {
            assert!(valid_path(ok).is_ok(), "{ok}");
        }
        let long = "a/".repeat(600);
        for bad in [
            "",
            "/abs",
            "../escape",
            "a/../b",
            "a\\b",
            "c:/x",
            "CON",
            "saves/nul.txt",
            "trailing.",
            "a//b",
            "tab\there",
            "Poke\u{301}mon.srm",
            long.as_str(),
        ] {
            assert!(valid_path(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn hashes_are_lowercase_sha256_hex() {
        assert!(valid_hash(&"a".repeat(64)));
        assert!(!valid_hash(&"A".repeat(64)));
        assert!(!valid_hash(&"a".repeat(63)));
        assert!(!valid_hash(&"g".repeat(64)));
    }
}
