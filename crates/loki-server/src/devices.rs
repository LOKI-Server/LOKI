//! Device sign-in: a short code approved in the dashboard (for handhelds and TVs),
//! or a username and password (for headless clients). Each grants one revocable token.
use crate::{
    Error, Result, State, auth,
    auth::User,
    device_codes::{self, Waiting},
};
use crate::{sync, sync::valid_name};
use axum::{
    Json, Router,
    extract::State as ExtractState,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Account-wide native sign-in, shared by sync and file sharing.
pub fn router() -> Router<State> {
    Router::new()
        .route("/auth/v1", get(discovery))
        .route("/auth/v1/pair", post(pair))
        .route("/auth/v1/pair/poll", post(poll))
        .route("/auth/v1/login", post(login))
        .route("/auth/v1/me", get(me))
        .route("/auth/v1/device", axum::routing::delete(sign_out))
}
async fn discovery(ExtractState(state): ExtractState<State>) -> Json<Value> {
    Json(
        json!({"protocol":1,"server":env!("CARGO_PKG_VERSION"),"name":state.0.settings.read().await.name,"pairing":true,"services":["sync","files"]}),
    )
}
/// An authenticated account, before checking any service's resource permissions.
pub(crate) struct Caller {
    pub user: User,
    pub device: String,
}
pub(crate) async fn caller(state: &State, headers: &HeaderMap) -> Result<Caller> {
    let unauthorized = || {
        Error::coded(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "This device is signed out. Sign in again.",
        )
    };
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|h| !h.trim().is_empty())
        .ok_or_else(unauthorized)?;
    let row: Option<(String, String, i64, i64)> =
        sqlx::query_as("SELECT id,user_id,auth_version,last_seen FROM devices WHERE token=?")
            .bind(auth::digest(token.trim()))
            .fetch_optional(&state.0.db)
            .await?;
    let (device, user_id, version, seen) = row.ok_or_else(unauthorized)?;
    let user =
        sqlx::query_as("SELECT * FROM users WHERE id=? AND auth_version=? AND status='approved'")
            .bind(user_id)
            .bind(version)
            .fetch_optional(&state.0.db)
            .await?
            .ok_or_else(unauthorized)?;
    if seen < auth::now() - 60 {
        sqlx::query("UPDATE devices SET last_seen=? WHERE id=?")
            .bind(auth::now())
            .bind(&device)
            .execute(&state.0.db)
            .await?;
    }
    Ok(Caller { user, device })
}
pub(crate) async fn list(state: &State, user: &User) -> Result<Value> {
    use sqlx::Row;
    let rows = sqlx::query("SELECT id,name,platform,created,last_seen FROM devices WHERE user_id=? ORDER BY created,id")
        .bind(&user.id).fetch_all(&state.0.db).await?;
    Ok(json!(rows.into_iter().map(|r| json!({"id":r.get::<String,_>("id"),"name":r.get::<String,_>("name"),"platform":r.get::<String,_>("platform"),"created":r.get::<i64,_>("created"),"last_seen":r.get::<i64,_>("last_seen")})).collect::<Vec<_>>()))
}
pub(crate) async fn revoke(state: &State, user: &User, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM devices WHERE id=? AND user_id=?")
        .bind(id)
        .bind(&user.id)
        .execute(&state.0.db)
        .await?;
    Ok(())
}
pub(crate) async fn rename(state: &State, user: &User, id: &str, name: &str) -> Result<()> {
    sqlx::query("UPDATE devices SET name=? WHERE id=? AND user_id=?")
        .bind(valid_name(name, 80)?)
        .bind(id)
        .bind(&user.id)
        .execute(&state.0.db)
        .await?;
    Ok(())
}

#[derive(Deserialize)]
pub struct DeviceInfo {
    #[serde(default)]
    name: String,
    #[serde(default)]
    platform: String,
}
pub(crate) fn device_info(input: &DeviceInfo) -> Result<(String, String)> {
    let name = valid_name(&input.name, 80)?.to_string();
    let platform = input.platform.trim();
    if platform.chars().count() > 40 || platform.chars().any(char::is_control) {
        return Err(Error::bad("Platform must be at most 40 characters."));
    }
    Ok((name, platform.to_string()))
}
pub async fn pair(
    ExtractState(state): ExtractState<State>,
    axum::Extension(ip): axum::Extension<auth::ClientIp>,
    Json(input): Json<DeviceInfo>,
) -> Result<Json<Value>> {
    pair_for(&state, &ip, input, device_codes::ACCOUNT).await
}
pub(crate) async fn pair_for(
    state: &State,
    ip: &auth::ClientIp,
    input: DeviceInfo,
    kind: &str,
) -> Result<Json<Value>> {
    let (name, platform) = device_info(&input)?;
    let request = device_codes::Request {
        kind,
        client: "",
        scope: "",
        name: &name,
        platform: &platform,
    };
    Ok(Json(device_codes::create(&state, &ip, request).await?))
}

#[derive(Deserialize)]
pub struct Poll {
    #[serde(default)]
    pub(crate) device_code: String,
}
pub async fn poll(
    ExtractState(state): ExtractState<State>,
    Json(input): Json<Poll>,
) -> Result<Json<Value>> {
    poll_for(&state, device_codes::ACCOUNT, &input.device_code).await
}
pub(crate) async fn poll_for(state: &State, kind: &str, code: &str) -> Result<Json<Value>> {
    let approved = device_codes::poll(state, kind, "", code).await?;
    let device_codes::Approved {
        user,
        name,
        platform,
        ..
    } = approved.map_err(|waiting| {
        let (status, message) = match waiting {
            Waiting::Pending => (
                StatusCode::PRECONDITION_REQUIRED,
                "Waiting for approval in Account → Devices in the LOKI dashboard.".to_string(),
            ),
            Waiting::SlowDown => (
                StatusCode::TOO_MANY_REQUESTS,
                format!("Poll at most every {} seconds.", device_codes::INTERVAL),
            ),
            Waiting::Denied => (
                StatusCode::FORBIDDEN,
                "Pairing was declined in the dashboard.".to_string(),
            ),
            Waiting::Expired => (
                StatusCode::BAD_REQUEST,
                "This pairing code expired. Start pairing again.".to_string(),
            ),
        };
        Error::coded(status, waiting.code(), message)
    })?;
    if kind == device_codes::SYNC
        && !auth::allowed(state, Some(&user), sync::RESOURCE, "access").await?
    {
        return Err(Error::forbidden());
    }
    let issued = issue(&state, &user, &name, &platform).await?;
    state
        .audit(&user.id, "device-paired", &format!("{name} ({platform})"))
        .await?;
    Ok(Json(issued))
}

#[derive(Deserialize)]
pub struct Login {
    username: String,
    password: String,
    #[serde(default)]
    code: String,
    #[serde(flatten)]
    device: DeviceInfo,
}
pub async fn login(
    ExtractState(state): ExtractState<State>,
    axum::Extension(ip): axum::Extension<auth::ClientIp>,
    Json(input): Json<Login>,
) -> Result<Json<Value>> {
    login_for(&state, &ip, input, false).await
}
pub(crate) async fn login_for(
    state: &State,
    ip: &auth::ClientIp,
    input: Login,
    require_sync: bool,
) -> Result<Json<Value>> {
    // Shares the browser's limits, so devices add no extra guesses.
    auth::login_rate(&state, &input.username, &ip).await?;
    let (name, platform) = device_info(&input.device)?;
    let user =
        auth::device_credentials(&state, &input.username, input.password, &input.code).await?;
    if user.status != "approved"
        || (require_sync && !auth::allowed(state, Some(&user), sync::RESOURCE, "access").await?)
    {
        return Err(Error::coded(
            StatusCode::FORBIDDEN,
            "forbidden",
            "This account is not approved. Ask the server owner.",
        ));
    }
    let issued = issue(&state, &user, &name, &platform).await?;
    state
        .audit(&user.id, "device-login", &format!("{name} ({platform})"))
        .await?;
    Ok(Json(issued))
}

pub(crate) async fn issue(state: &State, user: &User, name: &str, platform: &str) -> Result<Value> {
    let token = auth::random();
    let device = auth::id();
    let inserted = sqlx::query("INSERT INTO devices(id,user_id,name,platform,token,auth_version,created,last_seen) SELECT ?,?,?,?,?,?,?,? WHERE EXISTS(SELECT 1 FROM users WHERE id=? AND auth_version=? AND status='approved')")
        .bind(&device)
        .bind(&user.id)
        .bind(name)
        .bind(platform)
        .bind(auth::digest(&token))
        .bind(user.auth_version)
        .bind(auth::now())
        .bind(auth::now())
        .bind(&user.id)
        .bind(user.auth_version)
        .execute(&state.0.db)
        .await?;
    if inserted.rows_affected() != 1 {
        return Err(Error::unauthorized());
    }
    Ok(json!({
        "token": token,
        "device": {"id": device, "name": name, "platform": platform},
        "user": {"id": user.id, "username": user.username, "display_name": user.display_name},
        "access": {"sync": auth::allowed(state, Some(user), sync::RESOURCE, "access").await?},
    }))
}

pub async fn me(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let Caller { user, device } = caller(&state, &headers).await?;
    let (name, platform): (String, String) =
        sqlx::query_as("SELECT name,platform FROM devices WHERE id=?")
            .bind(&device)
            .fetch_one(&state.0.db)
            .await?;
    Ok(Json(json!({
        "user": {"id": user.id, "username": user.username, "display_name": user.display_name},
        "device": {"id": device, "name": name, "platform": platform},
        "access": {"sync": auth::allowed(&state, Some(&user), sync::RESOURCE, "access").await?},
    })))
}
pub async fn sign_out(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let Caller { device, .. } = caller(&state, &headers).await?;
    sqlx::query("DELETE FROM devices WHERE id=?")
        .bind(device)
        .execute(&state.0.db)
        .await?;
    Ok(Json(json!({"ok":true})))
}
