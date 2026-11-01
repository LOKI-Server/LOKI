//! Sign in with a code (RFC 8628). A device shows a short code, the person approves
//! it in the dashboard, and the device collects its credential by polling. Save sync
//! uses it for its own device tokens; any app client can use it through
//! `/oidc/device` and the token endpoint.
use crate::{Error, Result, State, auth, auth::User};
use axum::http::StatusCode;
use rand::Rng;
use serde_json::{Value, json};

pub const LIFETIME: i64 = 600;
pub const INTERVAL: i64 = 5;
/// Consonants only: no vowels to spell words, no look-alike digits.
const CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
/// Codes for Save sync. Others are `oidc`, for an app's client.
pub const SYNC: &str = "sync";
pub const OIDC: &str = "oidc";

/// Accepts `abcd efgh`, `ABCD-EFGH`, and similar spellings of the same code.
fn normalize(code: &str) -> Option<String> {
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

/// What a device asks for: its own name and platform, and for `oidc` the client and scope.
pub struct Request<'a> {
    pub kind: &'a str,
    pub client: &'a str,
    pub scope: &'a str,
    pub name: &'a str,
    pub platform: &'a str,
}
/// Starts a sign-in and returns the RFC 8628 device authorization response.
pub async fn create(state: &State, ip: &auth::ClientIp, request: Request<'_>) -> Result<Value> {
    auth::rate(state, format!("device-codes:{}", ip.0), 30, 900).await?;
    auth::rate(state, "device-codes".into(), 600, 900).await?;
    sqlx::query("DELETE FROM device_codes WHERE expires<?")
        .bind(auth::now())
        .execute(&state.0.db)
        .await?;
    let device_code = auth::random();
    let mut code = user_code();
    // Collisions among a few live codes are unlikely; retry rather than fail.
    for _ in 0..5 {
        let inserted = sqlx::query("INSERT OR IGNORE INTO device_codes(id,user_code,kind,client,scope,name,platform,expires) VALUES(?,?,?,?,?,?,?,?)")
            .bind(auth::digest(&device_code))
            .bind(&code)
            .bind(request.kind)
            .bind(request.client)
            .bind(request.scope)
            .bind(request.name)
            .bind(request.platform)
            .bind(auth::now() + LIFETIME)
            .execute(&state.0.db)
            .await?;
        if inserted.rows_affected() == 1 {
            let base = state.0.settings.read().await.public_url.clone();
            return Ok(json!({
                "device_code": device_code,
                "user_code": code,
                "verification_uri": format!("{base}/pair"),
                "verification_uri_complete": format!("{base}/pair?code={code}"),
                "interval": INTERVAL,
                "expires_in": LIFETIME,
            }));
        }
        code = user_code();
    }
    Err(Error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Try signing in again.".into(),
    ))
}

/// Why a poll has no credential yet.
pub enum Waiting {
    Pending,
    SlowDown,
    Denied,
    Expired,
}
impl Waiting {
    /// The RFC 8628 error code.
    pub fn code(&self) -> &'static str {
        match self {
            Waiting::Pending => "authorization_pending",
            Waiting::SlowDown => "slow_down",
            Waiting::Denied => "access_denied",
            Waiting::Expired => "expired_token",
        }
    }
}
/// An approved code, now spent.
pub struct Approved {
    pub user: User,
    pub scope: String,
    pub name: String,
    pub platform: String,
}
/// user_id, auth_version, polled, scope, name, platform
type Row = (Option<String>, Option<i64>, i64, String, String, String);
/// Checks a device code. Approval is single use: the code is gone once collected.
pub async fn poll(
    state: &State,
    kind: &str,
    client: &str,
    device_code: &str,
) -> Result<std::result::Result<Approved, Waiting>> {
    let id = auth::digest(device_code);
    let row: Option<Row> = sqlx::query_as(
        "SELECT user_id,auth_version,polled,scope,name,platform FROM device_codes WHERE id=? AND kind=? AND client=? AND expires>?",
    )
    .bind(&id)
    .bind(kind)
    .bind(client)
    .bind(auth::now())
    .fetch_optional(&state.0.db)
    .await?;
    let Some((user_id, version, polled, scope, name, platform)) = row else {
        return Ok(Err(Waiting::Expired));
    };
    if version == Some(-1) {
        sqlx::query("DELETE FROM device_codes WHERE id=?")
            .bind(&id)
            .execute(&state.0.db)
            .await?;
        return Ok(Err(Waiting::Denied));
    }
    // One second of leeway absorbs network jitter in well-behaved clients.
    let slow = polled > auth::now() - (INTERVAL - 1);
    sqlx::query("UPDATE device_codes SET polled=? WHERE id=?")
        .bind(auth::now())
        .bind(&id)
        .execute(&state.0.db)
        .await?;
    let (Some(user_id), Some(version)) = (user_id, version) else {
        return Ok(Err(if slow {
            Waiting::SlowDown
        } else {
            Waiting::Pending
        }));
    };
    let consumed = sqlx::query("DELETE FROM device_codes WHERE id=?")
        .bind(&id)
        .execute(&state.0.db)
        .await?;
    if consumed.rows_affected() != 1 {
        return Ok(Err(Waiting::Expired));
    }
    let user: Option<User> =
        sqlx::query_as("SELECT * FROM users WHERE id=? AND auth_version=? AND status='approved'")
            .bind(&user_id)
            .bind(version)
            .fetch_optional(&state.0.db)
            .await?;
    Ok(match user {
        Some(user) => Ok(Approved {
            user,
            scope,
            name,
            platform,
        }),
        None => Err(Waiting::Expired),
    })
}

/// The resource a code would grant: Save sync, or the app behind an OIDC client.
async fn resource(state: &State, kind: &str, client: &str) -> Result<(String, String)> {
    if kind == SYNC {
        return Ok((crate::sync::RESOURCE.into(), "Save sync".into()));
    }
    sqlx::query_as(
        "SELECT r.id,r.name FROM clients c JOIN resources r ON r.id=c.resource WHERE c.id=?",
    )
    .bind(client)
    .fetch_optional(&state.0.db)
    .await?
    .ok_or_else(|| Error::bad("That code is unknown or expired. Check your device."))
}
/// kind, client, name, platform, expires
type Pending = (String, String, String, String, i64);
async fn pending(state: &State, user: &User, code: &str) -> Result<(String, Pending)> {
    auth::rate(state, format!("device-approve:{}", user.id), 30, 900).await?;
    let code = normalize(code)
        .ok_or_else(|| Error::bad("Enter the 8-letter code shown on your device."))?;
    let row: Pending = sqlx::query_as("SELECT kind,client,name,platform,expires FROM device_codes WHERE user_code=? AND user_id IS NULL AND auth_version IS NULL AND expires>?")
        .bind(&code)
        .bind(auth::now())
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(|| Error::bad("That code is unknown or expired. Check your device."))?;
    Ok((code, row))
}
/// What the dashboard shows before someone approves a code.
pub async fn lookup(state: &State, user: &User, code: &str) -> Result<Value> {
    let (code, (kind, client, name, platform, expires)) = pending(state, user, code).await?;
    let (_, service) = resource(state, &kind, &client).await?;
    Ok(
        json!({"code":code,"kind":kind,"service":service,"name":name,"platform":platform,"expires":expires}),
    )
}
/// Approves or declines a code. Approval needs access to what the code grants.
pub async fn decide(state: &State, user: &User, code: &str, approve: bool) -> Result<Value> {
    let (code, (kind, client, ..)) = pending(state, user, code).await?;
    let (resource, service) = resource(state, &kind, &client).await?;
    if approve && !auth::allowed(state, Some(user), &resource, "access").await? {
        return Err(Error(
            StatusCode::FORBIDDEN,
            format!("Your account cannot use {service}. Ask the server owner for access."),
        ));
    }
    let updated = if approve {
        sqlx::query("UPDATE device_codes SET user_id=?,auth_version=? WHERE user_code=? AND user_id IS NULL AND auth_version IS NULL AND expires>?")
            .bind(&user.id)
            .bind(user.auth_version)
    } else {
        sqlx::query("UPDATE device_codes SET auth_version=-1 WHERE user_code=? AND user_id IS NULL AND auth_version IS NULL AND expires>?")
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
    Ok(json!({"ok":true,"kind":kind}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codes_are_unambiguous_and_forgiving() {
        let code = user_code();
        assert_eq!(normalize(&code).as_deref(), Some(code.as_str()));
        assert_eq!(normalize(" bcdf ghjk ").as_deref(), Some("BCDF-GHJK"));
        assert_eq!(normalize("BCDFGHJ"), None);
        assert_eq!(normalize("ABCD-EFGH"), None);
    }
}
