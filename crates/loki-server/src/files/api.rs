//! Read-only native client API, independent of Save sync and browser sessions.
use super::{Browse, Share};
use crate::{Error, Result, State, auth, auth::User};
use axum::{
    Json, Router,
    extract::{Path, Query, State as ExtractState},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::io::AsyncReadExt;

pub fn router() -> Router<State> {
    Router::new()
        .route("/files/v1", get(discovery))
        .route("/files/v1/login", post(login))
        .route("/files/v1/pair", post(crate::devices::pair))
        .route("/files/v1/pair/poll", post(crate::devices::poll))
        .route("/files/v1/me", get(me))
        .route("/files/v1/device", delete(sign_out))
        .route("/files/v1/devices", get(devices))
        .route("/files/v1/devices/{id}", delete(revoke))
        .route("/files/v1/shares", get(shares))
        .route("/files/v1/shares/{id}", get(info))
        .route("/files/v1/shares/{id}/entries", get(entries))
        .route("/files/v1/shares/{id}/metadata", get(metadata))
        .route("/files/v1/shares/{id}/file", get(download))
        // Account settings can manage download credentials without holding one.
        .route("/api/file-devices", get(browser_devices))
        .route("/api/file-devices/{id}", delete(browser_revoke))
}

async fn discovery(ExtractState(state): ExtractState<State>) -> Json<Value> {
    Json(json!({"protocol":1,"server":env!("CARGO_PKG_VERSION"),
        "name":state.0.settings.read().await.name,"read_only":true,
        "authentication":"bearer","checksum":"sha256","page_size":100,
        "max_page":999,"ranges":true,"pairing":true,"auth":"/auth/v1"}))
}

// Original Files API login stays available as an alias for account sign-in.
use crate::devices::login;
async fn caller(state: &State, headers: &HeaderMap) -> Result<(User, String)> {
    let caller = crate::devices::caller(state, headers).await?;
    Ok((caller.user, caller.device))
}
fn user_info(user: &User) -> Value {
    json!({"id":user.id,"username":user.username,"display_name":user.display_name})
}

async fn me(ExtractState(state): ExtractState<State>, headers: HeaderMap) -> Result<Json<Value>> {
    let (user, id) = caller(&state, &headers).await?;
    let (name, platform): (String, String) =
        sqlx::query_as("SELECT name,platform FROM devices WHERE id=?")
            .bind(&id)
            .fetch_one(&state.0.db)
            .await?;
    Ok(Json(
        json!({"user":user_info(&user),"device":{"id":id,"name":name,"platform":platform}}),
    ))
}

async fn sign_out(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let (user, id) = caller(&state, &headers).await?;
    revoke_device(&state, &user, &id).await
}

async fn device_list(state: &State, user: &User) -> Result<Json<Value>> {
    let devices = sqlx::query("SELECT id,name,platform,created,last_seen FROM devices WHERE user_id=? ORDER BY created,id")
        .bind(&user.id).fetch_all(&state.0.db).await?.into_iter()
        .map(|r| json!({"id":r.get::<String,_>("id"),"name":r.get::<String,_>("name"),
            "platform":r.get::<String,_>("platform"),"created":r.get::<i64,_>("created"),"last_seen":r.get::<i64,_>("last_seen")})).collect::<Vec<_>>();
    Ok(Json(json!({"devices":devices})))
}

async fn revoke_device(state: &State, user: &User, id: &str) -> Result<Json<Value>> {
    sqlx::query("DELETE FROM devices WHERE id=? AND user_id=?")
        .bind(id)
        .bind(&user.id)
        .execute(&state.0.db)
        .await?;
    Ok(Json(json!({"ok":true})))
}

async fn devices(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let (user, _) = caller(&state, &headers).await?;
    device_list(&state, &user).await
}

async fn revoke(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let (user, _) = caller(&state, &headers).await?;
    revoke_device(&state, &user, &id).await
}

async fn browser_devices(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    device_list(&state, ctx.approved()?).await
}

async fn browser_revoke(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    revoke_device(&state, ctx.approved()?, &id).await
}

async fn shares(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let (user, _) = caller(&state, &headers).await?;
    let rows = sqlx::query("SELECT r.id,r.name,r.visibility FROM resources r JOIN shares s ON s.id=r.id WHERE r.kind='share' ORDER BY r.name COLLATE NOCASE,r.id")
        .fetch_all(&state.0.db).await?;
    let mut shares = Vec::new();
    for row in rows {
        let id: String = row.get("id");
        if auth::allowed(&state, Some(&user), &id, "access").await? {
            shares.push(json!({"id":id,"name":row.get::<String,_>("name"),"visibility":row.get::<String,_>("visibility"),"kind":"share"}));
        }
    }
    Ok(Json(json!({"shares":shares})))
}

async fn share(state: &State, headers: &HeaderMap, id: &str) -> Result<Share> {
    let (user, _) = caller(state, headers).await?;
    let row: Option<(String, String)> = sqlx::query_as("SELECT s.root,s.excluded_extensions FROM shares s JOIN resources r ON r.id=s.id WHERE s.id=? AND r.kind='share'")
        .bind(id).fetch_optional(&state.0.db).await?;
    let (root, extensions) = row.ok_or_else(Error::not_found)?;
    if !auth::allowed(state, Some(&user), id, "access").await? {
        return Err(Error::coded(
            StatusCode::FORBIDDEN,
            "forbidden",
            "You do not have access to this share.",
        ));
    }
    Ok(Share {
        root,
        excluded_extensions: serde_json::from_str(&extensions)?,
    })
}

async fn info(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    share(&state, &headers, &id).await?;
    let (name, visibility): (String, String) =
        sqlx::query_as("SELECT name,visibility FROM resources WHERE id=?")
            .bind(&id)
            .fetch_one(&state.0.db)
            .await?;
    Ok(Json(
        json!({"id":id,"name":name,"visibility":visibility,"kind":"share"}),
    ))
}

async fn entries(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<Browse>,
) -> Result<Json<Value>> {
    let share = share(&state, &headers, &id).await?;
    if query.page > 999 || !matches!(query.sort.as_str(), "" | "name" | "size" | "modified") {
        return Err(Error::bad(
            "Use a page from 0–999 and sort by name, size, or modified.",
        ));
    }
    super::list_share(share, query).await
}

#[derive(Deserialize)]
struct Metadata {
    path: String,
    #[serde(default)]
    checksum: bool,
}

async fn metadata(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<Metadata>,
) -> Result<Json<Value>> {
    let share = share(&state, &headers, &id).await?;
    let (file, path) = super::open_file(share, query.path.clone()).await?;
    let before = file.metadata()?;
    let modified = before.modified().ok();
    let mut value = json!({"path":query.path,"name":path.file_name().unwrap_or_default().to_string_lossy(),
        "size":before.len(),"modified":modified.and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0),
        "last_modified":modified.and_then(super::http_date)});
    if query.checksum {
        let mut file = tokio::fs::File::from_std(file);
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 65536];
        let mut remaining = before.len();
        while remaining > 0 {
            let limit = remaining.min(buffer.len() as u64) as usize;
            let count = file.read(&mut buffer[..limit]).await?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            remaining -= count as u64;
        }
        let after = file.metadata().await?;
        if remaining != 0 || after.len() != before.len() || after.modified().ok() != modified {
            return Err(Error::coded(
                StatusCode::CONFLICT,
                "file_changed",
                "The file changed while calculating its checksum. Retry.",
            ));
        }
        value["sha256"] = json!(hex::encode(hash.finalize()));
    }
    Ok(Json(value))
}

async fn download(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    method: Method,
    Path(id): Path<String>,
    Query(mut query): Query<Browse>,
) -> Result<Response> {
    let share = share(&state, &headers, &id).await?;
    query.download = true;
    super::serve_file(share, &headers, query, method == Method::HEAD).await
}
