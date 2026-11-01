use crate::{Error, Result, Settings, State};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use subtle::ConstantTimeEq;

pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
pub fn random() -> String {
    let mut b = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}
pub fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
pub fn equal(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub email: String,
    #[serde(skip_serializing)]
    pub password: String,
    pub owner: bool,
    pub status: String,
    pub avatar: bool,
    #[serde(skip_serializing)]
    pub totp: Option<String>,
    #[serde(skip_serializing)]
    pub totp_pending: Option<String>,
    #[serde(skip_serializing)]
    pub totp_step: i64,
    pub created: i64,
    #[serde(skip_serializing)]
    pub auth_version: i64,
    pub bio: String,
}
#[derive(FromRow)]
pub struct Session {
    pub id: String,
    pub user_id: Option<String>,
    pub csrf: String,
    pub auth_version: i64,
}
pub struct Context {
    pub session: Session,
    pub user: Option<User>,
}
impl Context {
    pub fn signed_in(&self) -> Result<&User> {
        self.user.as_ref().ok_or_else(Error::unauthorized)
    }
    pub fn approved(&self) -> Result<&User> {
        let u = self.signed_in()?;
        if u.status != "approved" {
            return Err(Error::forbidden());
        }
        Ok(u)
    }
    pub fn owner(&self) -> Result<&User> {
        let u = self.approved()?;
        if !u.owner {
            return Err(Error::forbidden());
        }
        Ok(u)
    }
}
pub fn cookie(value: String, secure: bool, days: i64) -> Cookie<'static> {
    Cookie::build((
        if secure {
            "__Host-loki_session"
        } else {
            "loki_session"
        },
        value,
    ))
    .http_only(true)
    .secure(secure)
    .same_site(SameSite::Lax)
    .path("/")
    .max_age(time::Duration::days(days))
    .build()
}
pub fn secure_cookie(settings: &Settings, headers: &HeaderMap, bind: std::net::SocketAddr) -> bool {
    if !settings.public_url.starts_with("https:") {
        return false;
    }
    // Local recovery/configuration stays usable before public DNS/TLS is ready.
    // Cookies remain host-only; public-host sessions always require HTTPS.
    let local = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| url::Url::parse(&format!("http://{h}")).ok())
        .is_some_and(|u| {
            matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
                && u.port_or_known_default() == Some(bind.port())
        });
    !local
}
pub async fn context(state: &State, headers: &HeaderMap) -> Result<Context> {
    let jar = CookieJar::from_headers(headers);
    let name = if secure_cookie(&*state.0.settings.read().await, headers, state.0.bind) {
        "__Host-loki_session"
    } else {
        "loki_session"
    };
    let raw = jar.get(name).ok_or_else(Error::unauthorized)?.value();
    let session: Session = sqlx::query_as(
        "SELECT id,user_id,csrf,auth_version FROM sessions WHERE id=? AND expires>?",
    )
    .bind(digest(raw))
    .bind(now())
    .fetch_optional(&state.0.db)
    .await?
    .ok_or_else(Error::unauthorized)?;
    let user: Option<User> = match &session.user_id {
        Some(id) => {
            sqlx::query_as("SELECT * FROM users WHERE id=? AND auth_version=?")
                .bind(id)
                .bind(session.auth_version)
                .fetch_optional(&state.0.db)
                .await?
        }
        None => None,
    };
    Ok(Context { session, user })
}
pub async fn new_session(
    state: &State,
    user: Option<(&str, i64)>,
    label: &str,
) -> Result<(String, String)> {
    let raw = random();
    let csrf = random();
    let (uid, version) = user.map(|(id, v)| (Some(id), v)).unwrap_or((None, 0));
    let result=sqlx::query("INSERT INTO sessions(id,user_id,csrf,created,expires,label,auth_version) SELECT ?,?,?,?,?,?,? WHERE ? IS NULL OR EXISTS(SELECT 1 FROM users WHERE id=? AND auth_version=? AND status IN('approved','pending'))")
        .bind(digest(&raw)).bind(uid).bind(&csrf).bind(now()).bind(now()+if user.is_some(){604800}else{3600}).bind(label.chars().take(150).collect::<String>()).bind(version).bind(uid).bind(uid).bind(version).execute(&state.0.db).await?;
    if result.rows_affected() != 1 {
        return Err(Error::unauthorized());
    }
    Ok((raw, csrf))
}
pub async fn session(
    axum::extract::State(state): axum::extract::State<State>,
    headers: HeaderMap,
) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    if let Some(value) = existing_session(&state, &headers).await {
        return Ok(Json(value).into_response());
    }
    let settings = state.0.settings.read().await.clone();
    let setup = state.0.setup_token.lock().await.is_some();
    let (raw, csrf) = new_session(&state, None, "Before sign-in").await?;
    let jar = CookieJar::from_headers(&headers).add(cookie(
        raw,
        secure_cookie(&settings, &headers, state.0.bind),
        1,
    ));
    Ok((jar, Json(session_json(&settings, setup, None, &csrf))).into_response())
}
/// Session details for a browser that already holds a valid session cookie.
/// Never creates a session, so the dashboard shell can embed it safely.
pub async fn existing_session(state: &State, headers: &HeaderMap) -> Option<serde_json::Value> {
    let ctx = context(state, headers).await.ok()?;
    let settings = state.0.settings.read().await.clone();
    let setup = state.0.setup_token.lock().await.is_some();
    Some(session_json(
        &settings,
        setup,
        ctx.user.as_ref(),
        &ctx.session.csrf,
    ))
}
fn session_json(
    settings: &Settings,
    setup: bool,
    user: Option<&User>,
    csrf: &str,
) -> serde_json::Value {
    serde_json::json!({"user":user,"csrf":csrf,"setup":setup,"name":settings.name,"registration":settings.registration,"public_url":settings.public_url})
}
pub async fn rate(state: &State, key: String, max: u32, seconds: i64) -> Result<()> {
    let mut limits = state.0.limits.lock().await;
    if limits.len() >= 10000 {
        limits.retain(|_, v| v.1 > now());
        if limits.len() >= 10000 {
            return Err(Error::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Try again later.",
            ));
        }
    }
    let value = limits.entry(key).or_insert((0, now() + seconds));
    if value.1 <= now() {
        *value = (0, now() + seconds);
    }
    value.0 += 1;
    if value.0 > max {
        return Err(Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts. Try again later.",
        ));
    }
    Ok(())
}
/// Where a request came from, for rate limits. The request guard adds it.
#[derive(Clone, Debug)]
pub struct ClientIp(pub String);
/// Behind LOKI's Caddy every connection is loopback, so the client is the last
/// address Caddy appended to `X-Forwarded-For`; other peers can't choose it.
/// IPv6 clients are grouped by /64, since one host usually holds the whole prefix.
pub fn client_ip(
    peer: Option<std::net::SocketAddr>,
    headers: &HeaderMap,
    behind_caddy: bool,
) -> String {
    use std::net::IpAddr;
    let Some(peer) = peer else {
        return "local".into();
    };
    let forwarded = (behind_caddy && peer.ip().is_loopback())
        .then(|| headers.get_all("x-forwarded-for").iter().next_back())
        .flatten()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    match forwarded.unwrap_or(peer.ip()).to_canonical() {
        IpAddr::V6(ip) => {
            let s = ip.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        ip => ip.to_string(),
    }
}
/// Counts a password guess against this client and against the account. The
/// account ceiling is high enough that one client can't lock its owner out.
pub async fn login_rate(state: &State, username: &str, ip: &ClientIp) -> Result<()> {
    let username = username.trim().to_lowercase();
    rate(state, format!("login:{username}:{}", ip.0), 8, 900).await?;
    rate(state, format!("login:{username}"), 100, 900).await
}
pub const BIO_CHARS: usize = 300;
const BIO_LINES: usize = 8;
/// Normalizes line endings and trims a bio, rejecting ones that are too long.
pub fn clean_bio(bio: &str) -> Result<String> {
    let bio = bio.replace("\r\n", "\n").trim().to_string();
    if bio.chars().count() > BIO_CHARS {
        return Err(Error::bad(format!(
            "Bio must be at most {BIO_CHARS} characters."
        )));
    }
    if bio.lines().count() > BIO_LINES {
        return Err(Error::bad(format!(
            "Bio must be at most {BIO_LINES} lines."
        )));
    }
    if bio.chars().any(|c| c.is_control() && c != '\n') {
        return Err(Error::bad("Bio cannot contain control characters."));
    }
    Ok(bio)
}
pub fn validate_identity(username: &str, email: &str, name: &str) -> Result<()> {
    if !(3..=32).contains(&username.len())
        || !username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(Error::bad(
            "Use a username of 3–32 letters, numbers, or underscores.",
        ));
    }
    if name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(Error::bad("Display name must be at most 80 characters."));
    }
    if !email.is_empty()
        && (email.len() > 254
            || email.chars().any(char::is_whitespace)
            || email.matches('@').count() != 1
            || !email.split('@').next_back().unwrap_or("").contains('.'))
    {
        return Err(Error::bad("Enter a valid email address."));
    }
    Ok(())
}
pub async fn hash(state: &State, password: String) -> Result<String> {
    if !(10..=128).contains(&password.len()) {
        return Err(Error::bad("Passwords must contain 10–128 bytes."));
    }
    let _permit = state.0.passwords.acquire().await?;
    Ok(tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(
                password.as_bytes(),
                &SaltString::generate(&mut rand::rngs::OsRng),
            )
            .map(|h| h.to_string())
            .map_err(|e| anyhow::anyhow!("{e}"))
    })
    .await??)
}
pub async fn verify(state: &State, password: String, hash: String) -> Result<bool> {
    if password.len() > 128 {
        return Ok(false);
    }
    let _permit = state.0.passwords.acquire().await?;
    Ok(tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).ok().is_some_and(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        })
    })
    .await?)
}

#[derive(Deserialize)]
pub struct Setup {
    pub token: String,
    pub username: String,
    pub password: String,
    pub name: String,
    pub public_url: String,
    #[serde(default)]
    pub startup: bool,
    #[serde(default)]
    pub startup_confirmation: Option<String>,
    #[serde(default)]
    pub data_dir: String,
}
fn check_setup_token(expected: Option<&str>, provided: &str) -> Result<()> {
    let Some(expected) = expected else {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "LOKI is already set up. Sign in with the owner account you created.",
        ));
    };
    if !equal(expected, provided.trim()) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "The setup token is invalid or no longer current. Use Open LOKI from the tray, or copy the current token from setup-token.txt.",
        ));
    }
    Ok(())
}
pub async fn setup(
    axum::extract::State(state): axum::extract::State<State>,
    Json(input): Json<Setup>,
) -> Result<Json<serde_json::Value>> {
    check_setup_token(state.0.setup_token.lock().await.as_deref(), &input.token)?;
    validate_identity(&input.username, "", &input.username)?;
    let url = validate_public_url(&input.public_url)?;
    let password = hash(&state, input.password).await?;
    let _guard = state.0.operations.lock().await;
    let mut token = state.0.setup_token.lock().await;
    check_setup_token(token.as_deref(), &input.token)?;
    if !input.data_dir.trim().is_empty() {
        crate::storage::prepare_destination(&state, input.data_dir.trim()).await?;
    }
    #[cfg(windows)]
    let startup_registration = if input.startup {
        match state.0.startup.change(
            true,
            &state.0.data,
            state.0.bind,
            input.startup_confirmation.as_deref(),
        )? {
            crate::startup::Change::Confirm(conflict) => return Ok(Json(conflict.response())),
            crate::startup::Change::Ready(registration) => Some(registration),
        }
    } else {
        None
    };
    #[cfg(not(windows))]
    if input.startup {
        crate::platform::startup(true, &state.0.data, state.0.bind).await?;
    }
    let user = id();
    let settings = Settings {
        name: input.name.chars().take(80).collect(),
        public_url: url,
        startup: input.startup,
        ..Settings::default()
    };
    let mut tx = state.0.db.begin().await?;
    sqlx::query("INSERT INTO users(id,username,display_name,password,owner,status,created) VALUES(?,?,?,?,1,'approved',?)").bind(&user).bind(input.username.trim()).bind(&input.username).bind(password).bind(now()).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO memberships(user_id,group_id) VALUES(?,'members')")
        .bind(&user)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT OR REPLACE INTO settings(id,json) VALUES(1,?)")
        .bind(serde_json::to_string(&settings)?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    #[cfg(windows)]
    if let Some(registration) = startup_registration {
        registration.commit();
    }
    *state.0.settings.write().await = settings;
    *token = None;
    let _ = tokio::fs::remove_file(state.0.data.join("setup-token.txt")).await;
    state
        .audit(&user, "setup", "Owner account established")
        .await?;
    drop(token);
    drop(_guard);
    if !input.data_dir.trim().is_empty() {
        if let Err(error) = crate::storage::switch_home(&state, input.data_dir.trim(), None).await {
            tracing::error!(error = %error.message, "owner created but setup data relocation failed");
            return Ok(Json(
                serde_json::json!({"ok":true,"warning":format!("Your owner account was created, but the data directory could not be moved: {} Sign in and retry the move from Server settings.",error.message)}),
            ));
        }
        return Ok(Json(serde_json::json!({"ok":true,"restarting":true})));
    }
    Ok(Json(serde_json::json!({"ok":true})))
}
pub fn validate_public_url(value: &str) -> Result<String> {
    let url =
        url::Url::parse(value.trim()).map_err(|_| Error::bad("Enter an absolute HTTP(S) URL."))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !matches!(url.scheme(), "http" | "https")
        || (url.scheme() == "http" && !local)
        || url.host_str().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::bad(
            "Use an HTTPS origin, or HTTP on localhost for development.",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}
#[derive(Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub code: String,
}
pub async fn login(
    axum::extract::State(state): axum::extract::State<State>,
    axum::Extension(ip): axum::Extension<ClientIp>,
    headers: HeaderMap,
    Json(input): Json<Credentials>,
) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    login_rate(&state, &input.username, &ip).await?;
    let user:Option<User>=sqlx::query_as("SELECT * FROM users WHERE username=? COLLATE NOCASE OR (email!='' AND email=? COLLATE NOCASE)").bind(input.username.trim()).bind(input.username.trim()).fetch_optional(&state.0.db).await?;
    let stored=user.as_ref().map(|u|u.password.clone()).unwrap_or_else(||"$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into());
    let valid = verify(&state, input.password, stored).await?;
    let user = user.filter(|_| valid).ok_or_else(|| {
        Error::new(
            StatusCode::UNAUTHORIZED,
            "Incorrect credentials or authentication code.",
        )
    })?;
    if user.status == "suspended" || user.status == "rejected" {
        return Err(Error::forbidden());
    }
    if user.totp.is_some() && !second_factor(&state, &user, &input.code).await? {
        return Err(Error::new(
            StatusCode::UNAUTHORIZED,
            "Incorrect credentials or authentication code.",
        ));
    }
    if let Ok(ctx) = context(&state, &headers).await {
        sqlx::query("DELETE FROM sessions WHERE id=?")
            .bind(ctx.session.id)
            .execute(&state.0.db)
            .await?;
    }
    let label = headers
        .get("user-agent")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("Browser");
    let (raw, _) = new_session(&state, Some((&user.id, user.auth_version)), label).await?;
    let jar = CookieJar::from_headers(&headers).add(cookie(
        raw,
        secure_cookie(
            &state.0.settings.read().await.clone(),
            &headers,
            state.0.bind,
        ),
        7,
    ));
    state.audit(&user.id, "login", "Browser sign-in").await?;
    Ok((jar, Json(serde_json::json!({"ok":true}))).into_response())
}
pub async fn logout(
    axum::extract::State(state): axum::extract::State<State>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>> {
    let ctx = context(&state, &headers).await?;
    sqlx::query("DELETE FROM sessions WHERE id=?")
        .bind(ctx.session.id)
        .execute(&state.0.db)
        .await?;
    Ok(Json(serde_json::json!({"ok":true})))
}
#[derive(Deserialize)]
pub struct Registration {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub display_name: String,
}
pub async fn register(
    axum::extract::State(state): axum::extract::State<State>,
    axum::Extension(ip): axum::Extension<ClientIp>,
    Json(input): Json<Registration>,
) -> Result<Json<serde_json::Value>> {
    rate(&state, format!("registrations:{}", ip.0), 10, 3600).await?;
    rate(&state, "registrations".into(), 60, 3600).await?;
    let mode = state.0.settings.read().await.registration.clone();
    if state.0.setup_token.lock().await.is_some() || mode == "closed" {
        return Err(Error::forbidden());
    }
    validate_identity(&input.username, &input.email, &input.display_name)?;
    let password = hash(&state, input.password).await?;
    let user = id();
    let status = if mode == "open" {
        "approved"
    } else {
        "pending"
    };
    let mut tx = state.0.db.begin().await?;
    let inserted=sqlx::query("INSERT INTO users(id,username,display_name,email,password,status,created) VALUES(?,?,?,?,?,?,?)").bind(&user).bind(&input.username).bind(if input.display_name.is_empty(){&input.username}else{&input.display_name}).bind(input.email.to_lowercase()).bind(password).bind(status).bind(now()).execute(&mut *tx).await;
    if inserted.is_err() {
        return Err(Error::bad("That username or email is unavailable."));
    }
    if status == "approved" {
        sqlx::query("INSERT INTO memberships(user_id,group_id) VALUES(?,'members')")
            .bind(&user)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(serde_json::json!({"ok":true,"status":status})))
}

pub async fn allowed(
    state: &State,
    user: Option<&User>,
    resource: &str,
    role: &str,
) -> Result<bool> {
    if user.is_some_and(|u| u.owner && u.status == "approved") {
        return Ok(true);
    }
    let visibility: Option<String> =
        sqlx::query_scalar("SELECT visibility FROM resources WHERE id=?")
            .bind(resource)
            .fetch_optional(&state.0.db)
            .await?;
    let Some(visibility) = visibility else {
        return Ok(false);
    };
    if role == "access" && visibility == "public" && user.is_none() {
        return Ok(true);
    }
    let Some(user) = user.filter(|u| u.status == "approved") else {
        return Ok(false);
    };
    if role == "access" && matches!(visibility.as_str(), "public" | "members") {
        return Ok(true);
    }
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM grants WHERE resource=? AND (role=? OR role='admin') AND ((subject_kind='user' AND subject=?) OR (subject_kind='group' AND subject IN (SELECT group_id FROM memberships WHERE user_id=?)))").bind(resource).bind(role).bind(&user.id).bind(&user.id).fetch_one(&state.0.db).await?;
    Ok(count > 0)
}
pub async fn revoke_user(state: &State, user: &str) -> Result<()> {
    let mut tx = state.0.db.begin().await?;
    sqlx::query("UPDATE users SET auth_version=auth_version+1 WHERE id=?")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    for table in [
        "sessions",
        "tokens",
        "codes",
        "gateway_sessions",
        "sync_devices",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE user_id=?"))
            .bind(user)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
pub fn base32(bytes: &[u8]) -> String {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut out, mut buffer, mut bits) = (String::new(), 0u32, 0u8);
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(alphabet[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(alphabet[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}
pub fn totp(secret: &str, step: i64) -> Option<String> {
    let (mut bytes, mut buf, mut bits) = (Vec::new(), 0u32, 0u8);
    for b in secret.bytes() {
        let pos = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
            .iter()
            .position(|x| *x == b)?;
        buf = (buf << 5) | pos as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buf >> bits) as u8);
        }
    }
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&bytes).ok()?;
    mac.update(&(step as u64).to_be_bytes());
    let result = mac.finalize().into_bytes();
    let offset = (result[19] & 15) as usize;
    let number = u32::from_be_bytes(result[offset..offset + 4].try_into().ok()?) & 0x7fff_ffff;
    Some(format!("{:06}", number % 1_000_000))
}
pub async fn second_factor(state: &State, user: &User, code: &str) -> Result<bool> {
    if let Some(secret) = &user.totp {
        for step in now() / 30 - 1..=now() / 30 + 1 {
            if step > user.totp_step && totp(secret, step).is_some_and(|s| equal(&s, code)) {
                let n = sqlx::query("UPDATE users SET totp_step=? WHERE id=? AND totp_step<?")
                    .bind(step)
                    .bind(&user.id)
                    .bind(step)
                    .execute(&state.0.db)
                    .await?
                    .rows_affected();
                return Ok(n == 1);
            }
        }
    }
    let n = sqlx::query("DELETE FROM recovery_codes WHERE user_id=? AND hash=?")
        .bind(&user.id)
        .bind(digest(code.trim()))
        .execute(&state.0.db)
        .await?
        .rows_affected();
    Ok(n == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forwarded_clients_are_trusted_only_from_caddy() {
        let forwarded = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", value.parse().unwrap());
            headers
        };
        let loopback = Some("127.0.0.1:50000".parse().unwrap());
        let remote = Some("203.0.113.9:50000".parse().unwrap());
        let spoofed = forwarded("198.51.100.1, 192.0.2.7");
        assert_eq!(client_ip(loopback, &spoofed, true), "192.0.2.7");
        assert_eq!(client_ip(loopback, &spoofed, false), "127.0.0.1");
        assert_eq!(client_ip(remote, &spoofed, true), "203.0.113.9");
        assert_eq!(client_ip(loopback, &forwarded("junk"), true), "127.0.0.1");
        assert_eq!(client_ip(None, &spoofed, true), "local");
        assert_eq!(
            client_ip(loopback, &forwarded("2001:db8:1:2:3:4:5:6"), true),
            "2001:db8:1:2::/64"
        );
        assert_eq!(
            client_ip(
                Some("[::ffff:192.0.2.7]:1".parse().unwrap()),
                &spoofed,
                false
            ),
            "192.0.2.7"
        );
    }
    #[tokio::test]
    async fn one_client_cannot_lock_an_account_out_for_others() {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::open(tmp.path().into(), "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let attacker = ClientIp("192.0.2.7".into());
        for _ in 0..8 {
            login_rate(&state, "Owner", &attacker).await.unwrap();
        }
        assert!(login_rate(&state, " owner ", &attacker).await.is_err());
        login_rate(&state, "owner", &ClientIp("198.51.100.1".into()))
            .await
            .unwrap();
        state.0.db.close().await;
    }
    #[test]
    fn rfc_totp_vector() {
        assert_eq!(
            totp(&base32(b"12345678901234567890"), 1).as_deref(),
            Some("287082")
        );
    }
    #[test]
    fn bios_are_trimmed_and_limited() {
        assert_eq!(
            clean_bio(
                " a
b "
            )
            .unwrap(),
            "a
b"
        );
        assert!(clean_bio(&"é".repeat(BIO_CHARS)).is_ok());
        assert!(clean_bio(&"é".repeat(BIO_CHARS + 1)).is_err());
        assert!(
            clean_bio(
                &"a
"
                .repeat(9)
            )
            .is_err()
        );
        assert!(clean_bio("tab	here").is_err());
    }
    #[test]
    fn rejects_unsafe_origins() {
        for url in [
            "http://example.com",
            "https://x/path",
            "https://user@x",
            "https://x?redirect=bad",
        ] {
            assert!(validate_public_url(url).is_err());
        }
        assert!(validate_public_url("https://loki.example.com").is_ok());
    }
    #[test]
    fn public_cookies_are_secure_and_local_recovery_remains_usable() {
        let settings = Settings {
            public_url: "https://loki.example.com".into(),
            ..Settings::default()
        };
        let bind = "127.0.0.1:7070".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("host", "loki.example.com".parse().unwrap());
        assert!(secure_cookie(&settings, &headers, bind));
        headers.insert("host", "127.0.0.1:7070".parse().unwrap());
        assert!(!secure_cookie(&settings, &headers, bind));
        headers.insert("host", "localhost:9999".parse().unwrap());
        assert!(secure_cookie(&settings, &headers, bind));
    }
}
