//! Device sign-in: a short code approved in the dashboard (for handhelds and TVs),
//! or a username and password (for headless clients). Each grants one revocable token.
use super::{Caller, RESOURCE, caller, max_file, quota, usage, valid_name};
use crate::{Error, Result, State, auth, auth::User};
use axum::{
    Json,
    extract::State as ExtractState,
    http::{HeaderMap, StatusCode},
};
use rand::Rng;
use serde::Deserialize;
use serde_json::{Value, json};

const PAIRING_SECONDS: i64 = 600;
const POLL_SECONDS: i64 = 5;
/// Consonants only: no vowels to spell words, no look-alike digits.
const CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

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
/// Accepts `abcd efgh`, `ABCD-EFGH`, and similar spellings of the same code.
fn normalize_code(code: &str) -> Option<String> {
    let letters: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (letters.len() == 8 && letters.bytes().all(|b| CODE_ALPHABET.contains(&b)))
        .then(|| format!("{}-{}", &letters[..4], &letters[4..]))
}
fn user_code() -> String {
    let mut rng = rand::rngs::OsRng;
    let letters: String = (0..8)
        .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
        .collect();
    format!("{}-{}", &letters[..4], &letters[4..])
}

pub async fn pair(
    ExtractState(state): ExtractState<State>,
    Json(input): Json<DeviceInfo>,
) -> Result<Json<Value>> {
    auth::rate(&state, "sync-pairings".into(), 60, 900).await?;
    let (name, platform) = device_info(&input)?;
    sqlx::query("DELETE FROM sync_pairings WHERE expires<?")
        .bind(auth::now())
        .execute(&state.0.db)
        .await?;
    let device_code = auth::random();
    let mut code = user_code();
    // Collisions among a few live codes are unlikely; retry rather than fail.
    for _ in 0..5 {
        let inserted = sqlx::query("INSERT OR IGNORE INTO sync_pairings(id,user_code,name,platform,expires) VALUES(?,?,?,?,?)")
            .bind(auth::digest(&device_code))
            .bind(&code)
            .bind(&name)
            .bind(&platform)
            .bind(auth::now() + PAIRING_SECONDS)
            .execute(&state.0.db)
            .await?;
        if inserted.rows_affected() == 1 {
            let base = state.0.settings.read().await.public_url.clone();
            return Ok(Json(json!({
                "device_code": device_code,
                "user_code": code,
                "verification_uri": format!("{base}/pair"),
                "verification_uri_complete": format!("{base}/pair?code={code}"),
                "interval": POLL_SECONDS,
                "expires_in": PAIRING_SECONDS,
            })));
        }
        code = user_code();
    }
    Err(Error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Try pairing again.".into(),
    ))
}

/// user_id, auth_version, polled, name, platform
type PairingRow = (Option<String>, Option<i64>, i64, String, String);
#[derive(Deserialize)]
pub struct Poll {
    #[serde(default)]
    device_code: String,
}
pub async fn poll(
    ExtractState(state): ExtractState<State>,
    Json(input): Json<Poll>,
) -> Result<Json<Value>> {
    let id = auth::digest(&input.device_code);
    let expired = || {
        Error::coded(
            StatusCode::BAD_REQUEST,
            "expired_token",
            "This pairing code expired. Start pairing again.",
        )
    };
    let row: Option<PairingRow> = sqlx::query_as(
        "SELECT user_id,auth_version,polled,name,platform FROM sync_pairings WHERE id=? AND expires>?",
    )
    .bind(&id)
    .bind(auth::now())
    .fetch_optional(&state.0.db)
    .await?;
    let (user_id, version, polled, name, platform) = row.ok_or_else(expired)?;
    if version == Some(-1) {
        sqlx::query("DELETE FROM sync_pairings WHERE id=?")
            .bind(&id)
            .execute(&state.0.db)
            .await?;
        return Err(Error::coded(
            StatusCode::FORBIDDEN,
            "access_denied",
            "Pairing was declined in the dashboard.",
        ));
    }
    // One second of leeway absorbs network jitter in well-behaved clients.
    let slow = polled > auth::now() - (POLL_SECONDS - 1);
    sqlx::query("UPDATE sync_pairings SET polled=? WHERE id=?")
        .bind(auth::now())
        .bind(&id)
        .execute(&state.0.db)
        .await?;
    let (Some(user_id), Some(version)) = (user_id, version) else {
        return Err(if slow {
            Error::coded(
                StatusCode::TOO_MANY_REQUESTS,
                "slow_down",
                format!("Poll at most every {POLL_SECONDS} seconds."),
            )
        } else {
            Error::coded(
                StatusCode::PRECONDITION_REQUIRED,
                "authorization_pending",
                "Waiting for approval in the LOKI dashboard.",
            )
        });
    };
    let consumed = sqlx::query("DELETE FROM sync_pairings WHERE id=?")
        .bind(&id)
        .execute(&state.0.db)
        .await?;
    if consumed.rows_affected() != 1 {
        return Err(expired());
    }
    let user: User = sqlx::query_as("SELECT * FROM users WHERE id=? AND auth_version=?")
        .bind(&user_id)
        .bind(version)
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(expired)?;
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
    Json(input): Json<Login>,
) -> Result<Json<Value>> {
    // Shares the browser's per-account limit, so devices add no extra guesses.
    auth::rate(
        &state,
        format!("login:{}", input.username.to_lowercase()),
        8,
        900,
    )
    .await?;
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

/// What the dashboard shows before a user approves a code.
pub async fn lookup(state: &State, user: &User, code: &str) -> Result<Value> {
    auth::rate(state, format!("sync-approve:{}", user.id), 30, 900).await?;
    let code = normalize_code(code)
        .ok_or_else(|| Error::bad("Enter the 8-letter code shown on your device."))?;
    let (name, platform, expires): (String, String, i64) = sqlx::query_as("SELECT name,platform,expires FROM sync_pairings WHERE user_code=? AND user_id IS NULL AND auth_version IS NULL AND expires>?")
        .bind(&code)
        .bind(auth::now())
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(|| Error::bad("That code is unknown or expired. Check your device."))?;
    Ok(json!({"code":code,"name":name,"platform":platform,"expires":expires}))
}
pub async fn decide(state: &State, user: &User, code: &str, approve: bool) -> Result<Value> {
    auth::rate(state, format!("sync-approve:{}", user.id), 30, 900).await?;
    let code = normalize_code(code)
        .ok_or_else(|| Error::bad("Enter the 8-letter code shown on your device."))?;
    let updated = if approve {
        sqlx::query("UPDATE sync_pairings SET user_id=?,auth_version=? WHERE user_code=? AND user_id IS NULL AND auth_version IS NULL AND expires>?")
            .bind(&user.id)
            .bind(user.auth_version)
    } else {
        sqlx::query("UPDATE sync_pairings SET auth_version=-1 WHERE user_code=? AND user_id IS NULL AND auth_version IS NULL AND expires>?")
    }
    .bind(&code)
    .bind(auth::now())
    .execute(&state.0.db)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(Error::bad(
            "That code is unknown or expired. Check your device.",
        ));
    }
    Ok(json!({"ok":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codes_are_unambiguous_and_forgiving() {
        let code = user_code();
        assert_eq!(normalize_code(&code).as_deref(), Some(code.as_str()));
        assert_eq!(normalize_code(" bcdf ghjk ").as_deref(), Some("BCDF-GHJK"));
        assert_eq!(normalize_code("BCDFGHJ"), None);
        assert_eq!(normalize_code("ABCD-EFGH"), None);
    }
}
