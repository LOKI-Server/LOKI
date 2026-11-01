//! Actions people take on their own account.
use super::actions::ok;
use crate::{Error, Result, State, auth, auth::User};
use serde::Deserialize;
use serde_json::{Value, json};

/// Proof that the person at the keyboard owns the account, for sensitive changes.
#[derive(Deserialize)]
pub(super) struct Proof {
    current_password: String,
    /// An authentication or recovery code, when the account uses MFA.
    #[serde(default)]
    code: String,
}
impl Proof {
    pub(super) async fn check(&self, state: &State, user: &User) -> Result<()> {
        if !auth::verify(state, self.current_password.clone(), user.password.clone()).await? {
            return Err(Error::bad("Incorrect current password."));
        }
        if user.totp.is_some() && !auth::second_factor(state, user, &self.code).await? {
            return Err(Error::bad(
                "Enter a current authentication or recovery code.",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
pub(super) struct Profile {
    #[serde(flatten)]
    proof: Proof,
    username: String,
    display_name: String,
    email: String,
}
pub(super) async fn profile(state: &State, user: &User, input: Profile) -> Result<Value> {
    input.proof.check(state, user).await?;
    auth::validate_identity(&input.username, &input.email, &input.display_name)?;
    let result = sqlx::query("UPDATE users SET username=?,display_name=?,email=? WHERE id=?")
        .bind(&input.username)
        .bind(&input.display_name)
        .bind(input.email.to_lowercase())
        .bind(&user.id)
        .execute(&state.0.db)
        .await;
    if result.is_err() {
        return Err(Error::bad("That username or email is unavailable."));
    }
    Ok(ok())
}

#[derive(Deserialize)]
pub(super) struct Bio {
    bio: String,
}
pub(super) async fn bio(state: &State, user: &User, input: Bio) -> Result<Value> {
    let bio = auth::clean_bio(&input.bio)?;
    sqlx::query("UPDATE users SET bio=? WHERE id=?")
        .bind(bio)
        .bind(&user.id)
        .execute(&state.0.db)
        .await?;
    Ok(ok())
}

#[derive(Deserialize)]
pub(super) struct Password {
    #[serde(flatten)]
    proof: Proof,
    password: String,
}
pub(super) async fn password(state: &State, user: &User, input: Password) -> Result<Value> {
    input.proof.check(state, user).await?;
    let hash = auth::hash(state, input.password).await?;
    let mut tx = state.0.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE users SET password=?,auth_version=auth_version+1 WHERE id=? AND auth_version=?",
    )
    .bind(hash)
    .bind(&user.id)
    .bind(user.auth_version)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(Error::unauthorized());
    }
    for table in [
        "sessions",
        "tokens",
        "codes",
        "gateway_sessions",
        "sync_devices",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE user_id=?"))
            .bind(&user.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(ok())
}

pub(super) async fn revoke_session(state: &State, user: &User, id: &str) -> Result<Value> {
    let own: Option<String> =
        sqlx::query_scalar("SELECT id FROM sessions WHERE id=? AND user_id=?")
            .bind(id)
            .bind(&user.id)
            .fetch_optional(&state.0.db)
            .await?;
    if let Some(id) = own {
        auth::end_session(state, &id).await?;
    }
    Ok(ok())
}

pub(super) async fn mfa_start(state: &State, user: &User, proof: Proof) -> Result<Value> {
    proof.check(state, user).await?;
    let mut bytes = [0; 20];
    rand::fill(&mut bytes);
    let secret = auth::base32(&bytes);
    sqlx::query("UPDATE users SET totp_pending=? WHERE id=?")
        .bind(&secret)
        .bind(&user.id)
        .execute(&state.0.db)
        .await?;
    let issuer = state.0.settings.read().await.name.clone();
    let mut url = url::Url::parse("otpauth://totp/LOKI")?;
    url.set_path(&format!("{issuer}:{}", user.username));
    url.query_pairs_mut()
        .append_pair("secret", &secret)
        .append_pair("issuer", &issuer)
        .append_pair("algorithm", "SHA1")
        .append_pair("digits", "6")
        .append_pair("period", "30");
    Ok(json!({"secret":secret,"uri":url.as_str()}))
}

/// A code the person typed: an authenticator code or a device's sign-in code.
#[derive(Deserialize)]
pub(super) struct Code {
    pub(super) code: String,
}
pub(super) async fn mfa_enable(state: &State, user: &User, input: Code) -> Result<Value> {
    let secret = user
        .totp_pending
        .as_ref()
        .ok_or_else(|| Error::bad("Begin MFA setup first."))?;
    let current = auth::now() / 30;
    let step = (current - 1..=current + 1)
        .find(|step| auth::totp(secret, *step).is_some_and(|c| auth::equal(&c, &input.code)))
        .ok_or_else(|| Error::bad("Incorrect authentication code."))?;
    let codes: Vec<String> = (0..10).map(|_| auth::random()[..16].to_string()).collect();
    let mut tx = state.0.db.begin().await?;
    sqlx::query("UPDATE users SET totp=totp_pending,totp_pending=NULL,totp_step=? WHERE id=?")
        .bind(step)
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM recovery_codes WHERE user_id=?")
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    for code in &codes {
        sqlx::query("INSERT INTO recovery_codes(user_id,hash) VALUES(?,?)")
            .bind(&user.id)
            .bind(auth::digest(code))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(json!({"recovery_codes":codes}))
}

pub(super) async fn mfa_disable(state: &State, user: &User, proof: Proof) -> Result<Value> {
    proof.check(state, user).await?;
    let mut tx = state.0.db.begin().await?;
    sqlx::query("UPDATE users SET totp=NULL,totp_pending=NULL WHERE id=?")
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM recovery_codes WHERE user_id=?")
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ok())
}
