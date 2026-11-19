use super::{Access, Repo, access, find, valid_id};
use crate::{Error, Result, State, auth, web::account::Proof};
use axum::{
    Extension, Json,
    extract::{Path, State as ExtractState},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, sqlx::FromRow)]
pub(super) struct Token {
    pub id: String,
    pub user_id: String,
    write: bool,
}

pub(super) async fn token(
    db: &sqlx::SqlitePool,
    repo: &str,
    key: &str,
    by_id: bool,
) -> Result<Option<Token>> {
    Ok(sqlx::query_as(
        "WITH candidates AS (
            SELECT id, user_id, write, expires, auth_version FROM git_account_tokens
            WHERE (?2 AND id=?3) OR (NOT ?2 AND digest=?3)
            UNION ALL
            SELECT id, user_id, write, expires, auth_version FROM git_tokens
            WHERE repo_id=?1 AND ((?2 AND id=?3) OR (NOT ?2 AND digest=?3))
            UNION ALL
            SELECT t.id,d.user_id,t.write,min(t.expires,g.expires),d.auth_version
            FROM api_git_tokens t JOIN api_device_grants g ON g.device_id=t.device_id
            JOIN devices d ON d.id=g.device_id
            WHERE t.repo_id=?1 AND ((?2 AND t.id=?3) OR (NOT ?2 AND t.digest=?3))
        )
        SELECT t.id,t.user_id,t.write FROM candidates t JOIN users u ON u.id=t.user_id
        WHERE t.expires>?4 AND t.auth_version=u.auth_version AND u.status='approved'",
    )
    .bind(repo)
    .bind(by_id)
    .bind(key)
    .bind(auth::now())
    .fetch_optional(db)
    .await?)
}

pub(super) fn challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, r#"Basic realm="LOKI Git""#),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "Use a Git token from your LOKI account as your password.",
    )
        .into_response()
}

fn credential(headers: &HeaderMap) -> Option<String> {
    let (scheme, raw) = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic") || raw.len() > 1024 {
        return None;
    }
    let text = String::from_utf8(STANDARD.decode(raw).ok()?).ok()?;
    let (_, password) = text.split_once(':')?;
    password
        .starts_with("loki_git_")
        .then(|| auth::digest(password))
}

pub(super) fn peer(request: &axum::extract::Request) -> Option<std::net::SocketAddr> {
    request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|peer| peer.0)
}

pub(super) async fn authenticate(
    state: &State,
    id: &str,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    write: bool,
) -> Result<(Repo, Token)> {
    if peer.is_some_and(|addr| !addr.ip().is_loopback())
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
                .is_none_or(|h| h != "https"))
    {
        return Err(Error::forbidden());
    }
    let key = credential(headers).ok_or_else(Error::unauthorized)?;
    let identity = token(&state.0.db, id, &key, false)
        .await?
        .ok_or_else(Error::unauthorized)?;
    let repo = find(state, id).await?;
    authorize(&state.0.db, &repo, &identity, write).await?;
    Ok((repo, identity))
}

pub(super) async fn authorize(
    db: &sqlx::SqlitePool,
    repo: &Repo,
    token: &Token,
    write: bool,
) -> Result<Access> {
    let role = access(db, repo, &token.user_id)
        .await?
        .ok_or_else(Error::not_found)?;
    if write && (!token.write || role < Access::Write) {
        return Err(Error::forbidden());
    }
    Ok(role)
}

pub(super) async fn list(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.approved()?;
    let records: Vec<(String, String, bool, i64, i64, bool)> = sqlx::query_as(
        "SELECT id,label,write,created,expires,0 FROM git_account_tokens
         WHERE user_id=?1 AND expires>?2 AND auth_version=?3
         UNION ALL
         SELECT id,'Legacy repository token',write,created,expires,1 FROM git_tokens
         WHERE user_id=?1 AND expires>?2 AND auth_version=?3 ORDER BY created DESC",
    )
    .bind(&user.id)
    .bind(auth::now())
    .bind(user.auth_version)
    .fetch_all(&state.0.db)
    .await?;
    Ok(Json(json!(records.into_iter().map(|(id,label,write,created,expires,legacy)|
        json!({"id":id,"label":label,"write":write,"created":created,"expires":expires,"legacy":legacy})
    ).collect::<Vec<_>>())))
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
    auth::rate(&state, format!("git-token:{}", user.id), 20, 3600).await?;
    input.proof.check(&state, user).await?;
    let now = auth::now();
    let expires = now + 90 * 86400;
    let id = auth::id();
    let secret = format!("loki_git_{}", auth::random());
    let mut tx = state.0.db.begin().await?;
    sqlx::query(
        "DELETE FROM git_account_tokens WHERE user_id=? AND (expires<=? OR auth_version!=?)",
    )
    .bind(&user.id)
    .bind(now)
    .bind(user.auth_version)
    .execute(&mut *tx)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO git_account_tokens(id,digest,user_id,label,write,created,expires,auth_version)
         SELECT ?1,?2,?3,?4,?5,?6,?7,?8 WHERE
         EXISTS(SELECT 1 FROM users WHERE id=?3 AND auth_version=?8 AND status='approved')
         AND (SELECT count(*) FROM git_account_tokens WHERE user_id=?3)<50",
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
            "Sign in again or revoke an existing Git token.",
        ));
    }
    tx.commit().await?;
    state.audit(&user.id, "git-token-issued", &id).await?;
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
    let mut tx = state.0.db.begin().await?;
    for table in ["git_account_tokens", "git_tokens"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE user_id=? AND id=?"))
            .bind(&user.id)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    state.audit(&user.id, "git-token-revoked", &id).await?;
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
    let _guard = state.0.git_files.read().await;
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
    auth::rate(&state, format!("api-git-token:{}", device.0), 120, 60).await?;
    let now = auth::now();
    let expires = now + 3600;
    let secret = format!("loki_git_{}", auth::random());
    let mut tx = state.0.db.begin().await?;
    sqlx::query("DELETE FROM api_git_tokens WHERE device_id=? AND expires<=?")
        .bind(&device.0)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    let issued = sqlx::query(
        "INSERT INTO api_git_tokens(id,digest,device_id,repo_id,write,expires)
         SELECT ?1,?2,d.id,?4,?5,min(?6,g.expires)
         FROM devices d JOIN api_device_grants g ON g.device_id=d.id JOIN users u ON u.id=d.user_id
         WHERE d.id=?3 AND d.user_id=?7 AND d.auth_version=u.auth_version AND u.status='approved' AND g.expires>?8",
    ).bind(auth::id()).bind(auth::digest(&secret)).bind(&device.0).bind(&repo)
        .bind(input.write).bind(expires).bind(&user.id).bind(now).execute(&mut *tx).await?;
    if issued.rows_affected() != 1 {
        return Err(Error::unauthorized());
    }
    tx.commit().await?;
    Ok(Json(json!({"username":user.username,"token":secret})))
}
