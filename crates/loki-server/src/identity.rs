//! OpenID Connect authorization-code provider. Only registered, first-party clients.
use crate::{
    Error, Result, State,
    auth::{self, User},
};
use askama::Template;
#[derive(Template)]
#[template(path = "denied.html")]
struct Denied {
    name: String,
    message: &'static str,
    assets: &'static str,
}
use axum::{
    Form, Json,
    extract::{Query, State as ExtractState},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use rsa::{RsaPrivateKey, traits::PublicKeyParts};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::FromRow;

/// Tokens are signed with ring's constant-time, blinded RSA. The rsa crate's
/// private-key operations leak timing (RUSTSEC-2023-0071), so LOKI uses it only to
/// generate and read the key, never to sign with it.
pub(crate) fn signer(key: &RsaPrivateKey) -> anyhow::Result<RsaKeyPair> {
    use rsa::pkcs8::EncodePrivateKey;
    RsaKeyPair::from_pkcs8(key.to_pkcs8_der()?.as_bytes())
        .map_err(|e| anyhow::anyhow!("The identity key cannot sign tokens: {e}."))
}
/// An RS256 (RSASSA-PKCS1-v1_5 with SHA-256) signature.
pub(crate) fn sign(signer: &RsaKeyPair, message: &[u8]) -> Result<Vec<u8>> {
    let mut signature = vec![0; signer.public().modulus_len()];
    signer
        .sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            message,
            &mut signature,
        )
        .map_err(|_| anyhow::anyhow!("Could not sign an identity token."))?;
    Ok(signature)
}
pub fn kid(state: &State) -> String {
    auth::digest(&URL_SAFE_NO_PAD.encode(state.0.key.n().to_bytes_be()))[..16].into()
}
pub fn jwks_value(state: &State) -> Value {
    json!({"keys":[{"kty":"RSA","use":"sig","alg":"RS256","kid":kid(state),"n":URL_SAFE_NO_PAD.encode(state.0.key.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(state.0.key.e().to_bytes_be())}]})
}
pub async fn jwks(ExtractState(state): ExtractState<State>) -> Json<Value> {
    Json(jwks_value(&state))
}
pub async fn discovery(ExtractState(state): ExtractState<State>) -> Json<Value> {
    let issuer = state.0.settings.read().await.public_url.clone();
    Json(
        json!({"issuer":issuer,"authorization_endpoint":format!("{issuer}/oidc/authorize"),"token_endpoint":format!("{issuer}/oidc/token"),"userinfo_endpoint":format!("{issuer}/oidc/userinfo"),"jwks_uri":format!("{issuer}/oidc/jwks"),"introspection_endpoint":format!("{issuer}/oidc/introspect"),"revocation_endpoint":format!("{issuer}/oidc/revoke"),"response_types_supported":["code"],"device_authorization_endpoint":format!("{issuer}/oidc/device"),"grant_types_supported":["authorization_code","refresh_token","urn:ietf:params:oauth:grant-type:device_code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"],"token_endpoint_auth_methods_supported":["client_secret_basic","client_secret_post"],"code_challenge_methods_supported":["S256"],"scopes_supported":["openid","profile","email"],"claims_supported":["sub","iss","aud","exp","iat","nonce","preferred_username","name","email","email_verified","picture"]}),
    )
}
#[derive(FromRow)]
pub struct Client {
    pub id: String,
    pub secret: String,
    pub resource: String,
    pub redirect: String,
}
#[derive(Deserialize)]
pub struct Authorize {
    pub client_id: String,
    pub redirect_uri: String,
    pub response_type: String,
    pub scope: String,
    pub state: String,
    pub nonce: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
}
pub async fn authorize(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Query(input): Query<Authorize>,
) -> Result<Response> {
    let client: Client = sqlx::query_as("SELECT * FROM clients WHERE id=?")
        .bind(&input.client_id)
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(|| Error::bad("Unknown client."))?;
    if input.redirect_uri != client.redirect
        || input.response_type != "code"
        || input.code_challenge_method != "S256"
        || input.code_challenge.len() != 43
        || URL_SAFE_NO_PAD
            .decode(&input.code_challenge)
            .map_or(true, |s| s.len() != 32)
        || input.state.len() > 1024
        || input.nonce.is_empty()
        || input.nonce.len() > 256
        || !input.scope.split_whitespace().any(|x| x == "openid")
        || input
            .scope
            .split_whitespace()
            .any(|s| !matches!(s, "openid" | "profile" | "email"))
    {
        return Err(Error::bad("Invalid authorization request."));
    }
    let ctx = auth::context(&state, &headers).await.ok();
    let Some(user) = ctx.as_ref().and_then(|c| c.user.as_ref()) else {
        let mut url = url::Url::parse("http://local/oidc/authorize")?;
        url.query_pairs_mut().extend_pairs([
            ("client_id", input.client_id.as_str()),
            ("redirect_uri", &input.redirect_uri),
            ("response_type", &input.response_type),
            ("scope", &input.scope),
            ("state", &input.state),
            ("nonce", &input.nonce),
            ("code_challenge", &input.code_challenge),
            ("code_challenge_method", &input.code_challenge_method),
        ]);
        // The sign-in page says which app it is for.
        let app: String = sqlx::query_scalar("SELECT name FROM resources WHERE id=?")
            .bind(&client.resource)
            .fetch_one(&state.0.db)
            .await?;
        let encode =
            |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let next = format!("{}?{}", url.path(), url.query().unwrap_or(""));
        return Ok(Redirect::to(&format!(
            "/login?next={}&app={}",
            encode(&next),
            encode(&app)
        ))
        .into_response());
    };
    if !auth::allowed(&state, Some(user), &client.resource, "access").await? {
        let name: String = sqlx::query_scalar("SELECT name FROM resources WHERE id=?")
            .bind(&client.resource)
            .fetch_one(&state.0.db)
            .await?;
        let message = if user.status == "pending" {
            "Your account is waiting for the server owner's approval."
        } else {
            "Your account does not have access to this application. Ask the server owner to grant you access."
        };
        return Ok((
            StatusCode::FORBIDDEN,
            axum::response::Html(
                Denied {
                    name,
                    message,
                    assets: crate::assets::base(),
                }
                .render()?,
            ),
        )
            .into_response());
    }
    let raw = auth::random();
    // Remember the LOKI session, so signing out of it ends what this code signs in.
    let login_session = ctx.as_ref().map(|c| c.session.id.clone());
    sqlx::query("INSERT INTO codes(id,client,user_id,redirect,challenge,nonce,scope,expires,auth_version,login_session) VALUES(?,?,?,?,?,?,?,?,?,?)").bind(auth::digest(&raw)).bind(&client.id).bind(&user.id).bind(&client.redirect).bind(input.code_challenge).bind(input.nonce).bind(input.scope).bind(auth::now()+120).bind(user.auth_version).bind(login_session).execute(&state.0.db).await?;
    let mut url = url::Url::parse(&client.redirect)?;
    url.query_pairs_mut()
        .append_pair("code", &raw)
        .append_pair("state", &input.state);
    Ok(Redirect::to(url.as_str()).into_response())
}
#[derive(Default, Deserialize)]
pub struct TokenInput {
    #[serde(default)]
    pub grant_type: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub code_verifier: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub device_code: String,
}
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
#[derive(Default, Deserialize)]
pub struct DeviceInput {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub scope: String,
    /// LOKI extensions: what the approval page calls the device.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub platform: String,
}
/// Starts signing in a device with a code (RFC 8628). Clients need no secret here,
/// since apps on phones, consoles and desktops cannot keep one.
pub async fn device(
    ExtractState(state): ExtractState<State>,
    axum::Extension(ip): axum::Extension<crate::auth::ClientIp>,
    Form(input): Form<DeviceInput>,
) -> Result<Json<Value>> {
    let client = public_client(&state, &input.client_id).await?;
    let scope = if input.scope.trim().is_empty() {
        "openid".to_string()
    } else {
        input.scope.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    if !scope
        .split(' ')
        .all(|s| matches!(s, "openid" | "profile" | "email"))
    {
        return Err(Error::new(StatusCode::BAD_REQUEST, "invalid_scope"));
    }
    let short = |value: &str, max: usize| {
        let value = value.trim();
        (value.chars().count() <= max && !value.chars().any(char::is_control))
            .then(|| value.to_string())
            .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "invalid_request"))
    };
    let name = short(&input.name, 80)?;
    let platform = short(&input.platform, 40)?;
    let request = crate::device_codes::Request {
        kind: crate::device_codes::OIDC,
        client: &client.id,
        scope: &scope,
        name: if name.is_empty() { "A device" } else { &name },
        platform: &platform,
    };
    Ok(Json(
        crate::device_codes::create(&state, &ip, request).await?,
    ))
}
async fn public_client(state: &State, id: &str) -> Result<Client> {
    sqlx::query_as("SELECT * FROM clients WHERE id=?")
        .bind(id)
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(|| Error::new(StatusCode::UNAUTHORIZED, "invalid_client"))
}
pub async fn client_auth(state: &State, headers: &HeaderMap, input: &TokenInput) -> Result<Client> {
    let mut id = input.client_id.clone();
    let mut secret = input.client_secret.clone();
    if let Some(basic) = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Basic "))
    {
        let decoded = String::from_utf8(STANDARD.decode(basic).map_err(|_| Error::unauthorized())?)
            .map_err(|_| Error::unauthorized())?;
        let (a, b) = decoded.split_once(':').ok_or_else(Error::unauthorized)?;
        id = a.into();
        secret = b.into();
    }
    let client: Client = sqlx::query_as("SELECT * FROM clients WHERE id=?")
        .bind(id)
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(Error::unauthorized)?;
    if !auth::equal(&client.secret, &auth::digest(&secret)) {
        return Err(Error::unauthorized());
    }
    Ok(client)
}
#[derive(FromRow)]
struct Code {
    user_id: String,
    auth_version: i64,
    challenge: String,
    nonce: String,
    scope: String,
}
#[derive(FromRow)]
struct Token {
    user_id: String,
    auth_version: i64,
    scope: String,
    family: String,
    used: i64,
}
fn invalid_grant() -> Error {
    Error::new(StatusCode::BAD_REQUEST, "invalid_grant")
}
pub async fn token(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Form(input): Form<TokenInput>,
) -> Result<Json<Value>> {
    // Devices signed in with a code have no secret, so their sign-in and refreshes
    // name the client without proving it. Their refresh tokens are marked as such.
    let secretless = input.client_secret.is_empty() && !headers.contains_key("authorization");
    let public =
        input.grant_type == DEVICE_GRANT || (input.grant_type == "refresh_token" && secretless);
    let client = if public {
        public_client(&state, &input.client_id).await?
    } else {
        client_auth(&state, &headers, &input).await?
    };
    let (user_id, scope, nonce, family, auth_version) = match input.grant_type.as_str() {
        DEVICE_GRANT => {
            let approved = crate::device_codes::poll(
                &state,
                crate::device_codes::OIDC,
                &client.id,
                &input.device_code,
            )
            .await?
            .map_err(|waiting| Error::new(StatusCode::BAD_REQUEST, waiting.code()))?;
            state
                .audit(
                    &approved.user.id,
                    "device-signed-in",
                    &format!(
                        "{} ({}) for {}",
                        approved.name, approved.platform, client.id
                    ),
                )
                .await?;
            (
                approved.user.id,
                approved.scope,
                None,
                auth::id(),
                approved.user.auth_version,
            )
        }
        "authorization_code" => {
            if !(43..=128).contains(&input.code_verifier.len())
                || !input
                    .code_verifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
            {
                return Err(invalid_grant());
            }
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(input.code_verifier.as_bytes()));
            let code:Code=sqlx::query_as("DELETE FROM codes WHERE id=? AND client=? AND redirect=? AND challenge=? AND expires>? RETURNING user_id,challenge,nonce,scope,auth_version").bind(auth::digest(&input.code)).bind(&client.id).bind(&input.redirect_uri).bind(&challenge).bind(auth::now()).fetch_optional(&state.0.db).await?.ok_or_else(invalid_grant)?;
            if code.challenge != challenge {
                return Err(invalid_grant());
            }
            (
                code.user_id,
                code.scope,
                Some(code.nonce),
                auth::id(),
                code.auth_version,
            )
        }
        "refresh_token" => {
            let mut tx = state.0.db.begin().await?;
            let old:Token=sqlx::query_as("UPDATE tokens SET used=used+1 WHERE id=? AND client=? AND kind='refresh' AND expires>? AND (?=0 OR family IN(SELECT id FROM token_families WHERE public=1)) RETURNING user_id,scope,family,used,auth_version").bind(auth::digest(&input.refresh_token)).bind(&client.id).bind(auth::now()).bind(public).fetch_optional(&mut *tx).await?.ok_or_else(invalid_grant)?;
            if old.used > 1 {
                sqlx::query("UPDATE token_families SET revoked=1 WHERE id=?")
                    .bind(&old.family)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM tokens WHERE family=?")
                    .bind(&old.family)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Err(invalid_grant());
            }
            tx.commit().await?;
            (old.user_id, old.scope, None, old.family, old.auth_version)
        }
        _ => return Err(Error::bad("unsupported_grant_type")),
    };
    let user: User = sqlx::query_as("SELECT * FROM users WHERE id=? AND auth_version=?")
        .bind(&user_id)
        .bind(auth_version)
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(invalid_grant)?;
    if !auth::allowed(&state, Some(&user), &client.resource, "access").await? {
        return Err(invalid_grant());
    }
    let access = auth::random();
    let refresh = auth::random();
    let mut tx = state.0.db.begin().await?;
    if input.grant_type == "authorization_code" || input.grant_type == DEVICE_GRANT {
        sqlx::query("INSERT INTO token_families(id,expires,public) VALUES(?,?,?)")
            .bind(&family)
            .bind(auth::now() + 604800)
            .bind(input.grant_type == DEVICE_GRANT)
            .execute(&mut *tx)
            .await?;
    }
    for (raw, kind, ttl) in [(&access, "access", 900), (&refresh, "refresh", 604800)] {
        let result=sqlx::query(
            "INSERT INTO tokens(id,client,user_id,scope,expires,family,kind,auth_version) SELECT ?,?,?,?,?,?,?,? WHERE EXISTS(SELECT 1 FROM users WHERE id=? AND auth_version=? AND status='approved') AND EXISTS(SELECT 1 FROM token_families WHERE id=? AND revoked=0)",
        )
        .bind(auth::digest(raw))
        .bind(&client.id)
        .bind(&user_id)
        .bind(&scope)
        .bind(auth::now() + ttl)
        .bind(&family)
        .bind(kind)
        .bind(auth_version).bind(&user_id).bind(auth_version).bind(&family)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(invalid_grant());
        }
    }
    tx.commit().await?;
    let issuer = state.0.settings.read().await.public_url.clone();
    let mut claims =
        json!({"iss":issuer,"sub":user.id,"aud":client.id,"iat":auth::now(),"exp":auth::now()+300});
    if let Some(nonce) = nonce {
        claims["nonce"] = json!(nonce);
    }
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(
        &json!({"alg":"RS256","typ":"JWT","kid":kid(&state)}),
    )?);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
    let message = format!("{header}.{payload}");
    let signature = sign(&state.0.signer, message.as_bytes())?;
    Ok(Json(
        json!({"token_type":"Bearer","expires_in":900,"access_token":access,"refresh_token":refresh,"scope":scope,"id_token":format!("{message}.{}",URL_SAFE_NO_PAD.encode(signature))}),
    ))
}
pub async fn token_user(
    state: &State,
    raw: &str,
    client: Option<&str>,
) -> Result<Option<(User, Client, String)>> {
    let record:Option<(String,String,String)>=sqlx::query_as("SELECT t.user_id,t.client,t.scope FROM tokens t JOIN users u ON u.id=t.user_id AND u.auth_version=t.auth_version JOIN token_families f ON f.id=t.family AND f.revoked=0 WHERE t.id=? AND t.kind='access' AND t.used=0 AND t.expires>?").bind(auth::digest(raw)).bind(auth::now()).fetch_optional(&state.0.db).await?;
    let Some((uid, cid, scope)) = record else {
        return Ok(None);
    };
    if client.is_some_and(|c| c != cid) {
        return Ok(None);
    }
    let client: Client = sqlx::query_as("SELECT * FROM clients WHERE id=?")
        .bind(cid)
        .fetch_one(&state.0.db)
        .await?;
    let user: User = sqlx::query_as("SELECT * FROM users WHERE id=?")
        .bind(uid)
        .fetch_one(&state.0.db)
        .await?;
    if !auth::allowed(state, Some(&user), &client.resource, "access").await? {
        return Ok(None);
    }
    Ok(Some((user, client, scope)))
}
pub async fn introspect(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Form(input): Form<TokenInput>,
) -> Result<Json<Value>> {
    let client = client_auth(&state, &headers, &input).await?;
    let Some((user, _, scope)) = token_user(&state, &input.token, Some(&client.id)).await? else {
        return Ok(Json(json!({"active":false})));
    };
    let admin = auth::allowed(&state, Some(&user), &client.resource, "admin").await?;
    let mut roles = vec!["access".to_string()];
    if admin {
        roles.push("admin".to_string());
    }
    let declared: Option<String> = sqlx::query_scalar("SELECT manifest FROM apps WHERE id=?")
        .bind(&client.resource)
        .fetch_optional(&state.0.db)
        .await?;
    if let Some(manifest) = declared {
        let manifest: crate::apps::Manifest = serde_json::from_str(&manifest)?;
        for role in manifest.roles {
            if !roles.contains(&role)
                && auth::allowed(&state, Some(&user), &client.resource, &role).await?
            {
                roles.push(role);
            }
        }
    }
    let exp: i64 = sqlx::query_scalar("SELECT expires FROM tokens WHERE id=?")
        .bind(auth::digest(&input.token))
        .fetch_one(&state.0.db)
        .await?;
    Ok(Json(
        json!({"active":true,"sub":user.id,"client_id":client.id,"aud":client.id,"exp":exp,"scope":scope,"username":user.username,"name":user.display_name,"email":user.email,"roles":roles,"token_type":"Bearer"}),
    ))
}
pub async fn userinfo(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let raw = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(Error::unauthorized)?;
    let (user, _, scope) = token_user(&state, raw, None)
        .await?
        .ok_or_else(Error::unauthorized)?;
    let mut value = json!({"sub":user.id});
    if scope.split_whitespace().any(|s| s == "profile") {
        value["preferred_username"] = json!(user.username);
        value["name"] = json!(user.display_name);
        if user.avatar {
            value["picture"] = json!(format!(
                "{}/avatars/{}",
                state.0.settings.read().await.public_url,
                user.id
            ));
        }
    }
    if scope.split_whitespace().any(|s| s == "email") {
        value["email"] = json!(user.email);
        value["email_verified"] = json!(false);
    }
    Ok(Json(value))
}
pub async fn revoke(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Form(input): Form<TokenInput>,
) -> Result<Json<Value>> {
    let client = client_auth(&state, &headers, &input).await?;
    let mut tx = state.0.db.begin().await?;
    sqlx::query("UPDATE token_families SET revoked=1 WHERE id IN(SELECT family FROM tokens WHERE id=? AND client=?)").bind(auth::digest(&input.token)).bind(&client.id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM tokens WHERE client=? AND family IN(SELECT family FROM tokens WHERE id=? AND client=?)").bind(&client.id).bind(auth::digest(&input.token)).bind(&client.id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::{
        pkcs1v15::{Signature, SigningKey, VerifyingKey},
        signature::{SignatureEncoding, Signer, Verifier},
    };
    #[test]
    fn ring_signs_exactly_what_rsa_would_and_it_verifies() {
        let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let message = b"header.payload";
        let signature = sign(&signer(&key).unwrap(), message).unwrap();
        let expected = SigningKey::<Sha256>::new(key.clone()).sign(message);
        assert_eq!(signature, expected.to_bytes().as_ref());
        VerifyingKey::<Sha256>::new(key.to_public_key())
            .verify(message, &Signature::try_from(signature.as_slice()).unwrap())
            .unwrap();
    }
}
