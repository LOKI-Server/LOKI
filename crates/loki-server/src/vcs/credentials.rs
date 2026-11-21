use super::{Access, Repo, require, valid_id};
use crate::{Error, Result, State, auth, web::account::Proof};
use axum::{
    Extension, Json,
    extract::{Path, Request, State as ExtractState},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) fn peer(request: &Request) -> Option<std::net::SocketAddr> {
    request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|peer| peer.0)
}

pub(super) fn identity<'a>(
    state: &'a State,
    request: &Request,
) -> impl Future<Output = Result<(String, Option<String>, bool)>> + use<'a> {
    let headers = request.headers().clone();
    let peer = peer(request);
    async move { identity_headers(state, &headers, peer).await }
}

async fn identity_headers(
    state: &State,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> Result<(String, Option<String>, bool)> {
    if peer.is_some_and(|peer| !peer.ip().is_loopback())
        || headers.contains_key(header::ORIGIN)
        || headers.contains_key("sec-fetch-site")
        || (state
            .0
            .settings
            .read()
            .await
            .public_url
            .starts_with("https://")
            && headers
                .get("x-forwarded-proto")
                .is_none_or(|proto| proto != "https"))
    {
        return Err(Error::forbidden());
    }
    let secret = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split_once(' '))
        .filter(|(scheme, raw)| {
            scheme.eq_ignore_ascii_case("Bearer") && raw.starts_with("skald_") && raw.len() <= 128
        })
        .map(|(_, raw)| auth::digest(raw))
        .ok_or_else(Error::unauthorized)?;
    sqlx::query_as(
        "WITH candidates AS (
            SELECT user_id,NULL AS repo_id,write,expires,auth_version FROM vcs_tokens WHERE digest=?1
            UNION ALL
            SELECT d.user_id,t.repo_id,t.write,min(t.expires,g.expires),d.auth_version
            FROM vcs_device_tokens t JOIN api_device_grants g ON g.device_id=t.device_id
            JOIN devices d ON d.id=g.device_id WHERE t.digest=?1
        )
        SELECT t.user_id,t.repo_id,t.write FROM candidates t JOIN users u ON u.id=t.user_id
        WHERE t.expires>?2 AND t.auth_version=u.auth_version AND u.status='approved'"
    ).bind(secret).bind(auth::now()).fetch_optional(&state.0.db).await?.ok_or_else(Error::unauthorized)
}

pub(super) async fn authenticate(
    state: &State,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    id: Option<&str>,
    minimum: Access,
) -> Result<(Repo, Access, String)> {
    let id = id.ok_or_else(Error::not_found)?;
    let (user, scope, write) = identity_headers(state, headers, peer).await?;
    if scope.as_deref().is_some_and(|repo| repo != id) {
        return Err(Error::not_found());
    }
    if minimum > Access::Read && !write {
        return Err(Error::forbidden());
    }
    let (repo, access) = require(state, id, &user, minimum).await?;
    let access = if write { access } else { Access::Read };
    Ok((repo, access, user))
}

pub(super) fn authorize<'a>(
    state: &'a State,
    id: &'a str,
    request: &Request,
    minimum: Access,
) -> impl Future<Output = Result<(Repo, Access, String)>> + use<'a> {
    let headers = request.headers().clone();
    let peer = peer(request);
    async move { authenticate(state, &headers, peer, Some(id), minimum).await }
}

pub(super) async fn list(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.approved()?;
    let records: Vec<(String, String, bool, i64, i64)> = sqlx::query_as(
        "SELECT id,label,write,created,expires FROM vcs_tokens WHERE user_id=?1 AND expires>?2 AND auth_version=?3 ORDER BY created DESC"
    ).bind(&user.id).bind(auth::now()).bind(user.auth_version).fetch_all(&state.0.db).await?;
    Ok(Json(json!(records.into_iter().map(|(id,label,write,created,expires)|
        json!({"id":id,"label":label,"write":write,"created":created,"expires":expires})).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
pub(super) struct NewToken {
    #[serde(flatten)]
    proof: Proof,
    label: String,
    write: bool,
}

pub(super) async fn issue(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Json(input): Json<NewToken>,
) -> Result<Response> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.approved()?;
    let label = input.label.trim();
    if label.is_empty() || label.chars().count() > 80 || label.chars().any(char::is_control) {
        return Err(Error::bad("Enter a token name of up to 80 characters."));
    }
    auth::rate(&state, format!("vcs-token:{}", user.id), 20, 3600).await?;
    input.proof.check(&state, user).await?;
    let now = auth::now();
    let expires = now + 90 * 86400;
    let id = auth::id();
    let secret = format!("skald_{}", auth::random());
    let mut tx = state.0.db.begin().await?;
    sqlx::query("DELETE FROM vcs_tokens WHERE user_id=? AND (expires<=? OR auth_version!=?)")
        .bind(&user.id)
        .bind(now)
        .bind(user.auth_version)
        .execute(&mut *tx)
        .await?;
    let inserted = sqlx::query(
        "INSERT INTO vcs_tokens(id,digest,user_id,label,write,created,expires,auth_version)
         SELECT ?1,?2,?3,?4,?5,?6,?7,?8 WHERE
         EXISTS(SELECT 1 FROM users WHERE id=?3 AND auth_version=?8 AND status='approved')
         AND (SELECT count(*) FROM vcs_tokens WHERE user_id=?3)<50",
    )
    .bind(&id)
    .bind(auth::digest(&secret))
    .bind(&user.id)
    .bind(label)
    .bind(input.write)
    .bind(now)
    .bind(expires)
    .bind(user.auth_version)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "Sign in again or revoke an existing Skald token.",
        ));
    }
    tx.commit().await?;
    state.audit(&user.id, "vcs-token-issued", &id).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "token":secret,"username":user.username,"expires":expires
        })),
    )
        .into_response())
}

pub(super) async fn revoke(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.approved()?;
    if !valid_id(&id) {
        return Err(Error::not_found());
    }
    sqlx::query("DELETE FROM vcs_tokens WHERE user_id=? AND id=?")
        .bind(&user.id)
        .bind(&id)
        .execute(&state.0.db)
        .await?;
    state.audit(&user.id, "vcs-token-revoked", &id).await?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
pub(super) struct Transfer {
    write: bool,
}

pub(super) async fn issue_transfer(
    ExtractState(state): ExtractState<State>,
    Path(repo): Path<String>,
    Extension(user): Extension<auth::User>,
    Extension(device): Extension<crate::devices::ApiDevice>,
    Json(input): Json<Transfer>,
) -> Result<Json<Value>> {
    let _guard = state.0.vcs_files.read().await;
    super::require(
        &state,
        &repo,
        &user.id,
        if input.write {
            Access::Write
        } else {
            Access::Read
        },
    )
    .await?;
    auth::rate(&state, format!("api-vcs-token:{}", device.0), 120, 60).await?;
    let now = auth::now();
    let expires = now + 3600;
    let secret = format!("skald_{}", auth::random());
    let mut tx = state.0.db.begin().await?;
    sqlx::query("DELETE FROM vcs_device_tokens WHERE device_id=? AND expires<=?")
        .bind(&device.0)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    let issued: Option<i64> = sqlx::query_scalar(
        "INSERT INTO vcs_device_tokens(id,digest,device_id,repo_id,write,expires)
         SELECT ?1,?2,d.id,?4,?5,min(?6,g.expires)
         FROM devices d JOIN api_device_grants g ON g.device_id=d.id JOIN users u ON u.id=d.user_id
         WHERE d.id=?3 AND d.user_id=?7 AND d.auth_version=u.auth_version AND u.status='approved' AND g.expires>?8 RETURNING expires",
    ).bind(auth::id()).bind(auth::digest(&secret)).bind(&device.0).bind(&repo)
        .bind(input.write).bind(expires).bind(&user.id).bind(now).fetch_optional(&mut *tx).await?;
    let expires = issued.ok_or_else(Error::unauthorized)?;
    tx.commit().await?;
    let public = state.0.settings.read().await.public_url.clone();
    Ok(Json(
        json!({"token":secret,"expires":expires,"remote_url":super::web::remote_url(&public, &repo)}),
    ))
}
