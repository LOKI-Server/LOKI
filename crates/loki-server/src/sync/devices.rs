//! Device sign-in: a short code approved in the dashboard (for handhelds and TVs),
//! or a username and password (for headless clients). Each grants one revocable token.
use super::{Caller, RESOURCE, caller, max_file, quota, usage, valid_name};
use crate::{
    Error, Result, State, auth,
    auth::User,
    device_codes::{self, Waiting},
};
use axum::{
    Json,
    extract::State as ExtractState,
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
pub struct DeviceInfo {
    #[serde(default)]
    name: String,
    #[serde(default)]
    platform: String,
}
fn device_info(input: &DeviceInfo) -> Result<(String, String)> {
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
    let (name, platform) = device_info(&input)?;
    let request = device_codes::Request {
        kind: device_codes::SYNC,
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
    device_code: String,
}
pub async fn poll(
    ExtractState(state): ExtractState<State>,
    Json(input): Json<Poll>,
) -> Result<Json<Value>> {
    let approved = device_codes::poll(&state, device_codes::SYNC, "", &input.device_code).await?;
    let device_codes::Approved {
        user,
        name,
        platform,
        ..
    } = approved.map_err(|waiting| {
        let (status, message) = match waiting {
            Waiting::Pending => (
                StatusCode::PRECONDITION_REQUIRED,
                "Waiting for approval in the LOKI dashboard.".to_string(),
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
    let issued = issue(&state, &user, &name, &platform).await?;
    state
        .audit(
            &user.id,
            "sync-device-paired",
            &format!("{name} ({platform})"),
        )
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
    // Shares the browser's limits, so devices add no extra guesses.
    auth::login_rate(&state, &input.username, &ip).await?;
    let (name, platform) = device_info(&input.device)?;
    let user: Option<User> = sqlx::query_as("SELECT * FROM users WHERE username=? COLLATE NOCASE OR (email!='' AND email=? COLLATE NOCASE)")
        .bind(input.username.trim())
        .bind(input.username.trim())
        .fetch_optional(&state.0.db)
        .await?;
    let stored = user.as_ref().map(|u| u.password.clone()).unwrap_or_else(|| {
        "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into()
    });
    let valid = auth::verify(&state, input.password, stored).await?;
    let incorrect = || {
        Error::coded(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "Incorrect credentials or authentication code.",
        )
    };
    let user = user.filter(|_| valid).ok_or_else(incorrect)?;
    if user.totp.is_some() && !auth::second_factor(&state, &user, &input.code).await? {
        return Err(incorrect());
    }
    if user.status != "approved" || !auth::allowed(&state, Some(&user), RESOURCE, "access").await? {
        return Err(Error::coded(
            StatusCode::FORBIDDEN,
            "forbidden",
            "This account cannot use sync. Ask the server owner for access.",
        ));
    }
    let issued = issue(&state, &user, &name, &platform).await?;
    state
        .audit(
            &user.id,
            "sync-device-login",
            &format!("{name} ({platform})"),
        )
        .await?;
    Ok(Json(issued))
}

async fn issue(state: &State, user: &User, name: &str, platform: &str) -> Result<Value> {
    let token = auth::random();
    let device = auth::id();
    let inserted = sqlx::query("INSERT INTO sync_devices(id,user_id,name,platform,token,auth_version,created,last_seen) SELECT ?,?,?,?,?,?,?,? WHERE EXISTS(SELECT 1 FROM users WHERE id=? AND auth_version=? AND status='approved')")
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
    }))
}

pub async fn me(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let Caller { user, device } = caller(&state, &headers).await?;
    let (name, platform): (String, String) =
        sqlx::query_as("SELECT name,platform FROM sync_devices WHERE id=?")
            .bind(&device)
            .fetch_one(&state.0.db)
            .await?;
    Ok(Json(json!({
        "user": {"id": user.id, "username": user.username, "display_name": user.display_name},
        "device": {"id": device, "name": name, "platform": platform},
        "quota": {"used": usage(&state, &user.id).await?, "limit": quota(&state, &user.id).await?},
        "max_file_bytes": max_file(&state).await,
    })))
}
pub async fn sign_out(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let Caller { device, .. } = caller(&state, &headers).await?;
    sqlx::query("DELETE FROM sync_devices WHERE id=?")
        .bind(device)
        .execute(&state.0.db)
        .await?;
    Ok(Json(json!({"ok":true})))
}
