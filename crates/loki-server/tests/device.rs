//! Signing in a device with a code through the OIDC device grant (RFC 8628).
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use loki_server::{State, auth, device_codes, web};
use serde_json::{Value, json};
use tower::ServiceExt;

const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

async fn send(router: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
async fn form(router: &axum::Router, path: &str, fields: &[(&str, &str)]) -> (StatusCode, Value) {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    let request = Request::post(path)
        .header(header::HOST, "127.0.0.1:7070")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    send(router, request).await
}
async fn user(state: &State, name: &str) -> auth::User {
    sqlx::query_as("SELECT * FROM users WHERE username=?")
        .bind(name)
        .fetch_one(&state.0.db)
        .await
        .unwrap()
}
async fn ready(state: &State) {
    // Polls are rate limited per code; tests skip the wait.
    sqlx::query("UPDATE device_codes SET polled=0")
        .execute(&state.0.db)
        .await
        .unwrap();
}

#[tokio::test]
async fn devices_sign_in_to_apps_with_a_code() {
    let directory = tempfile::tempdir().unwrap();
    let state = State::open(directory.path().into(), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let token = state.0.setup_token.lock().await.clone().unwrap();
    let _ = auth::setup(axum::extract::State(state.clone()), axum::Json(serde_json::from_value(json!({"token":token,"username":"owner","password":"owner-password-123","name":"Devices","public_url":"http://127.0.0.1:7070"})).unwrap())).await.unwrap();
    // An app's client, guarding a restricted resource.
    sqlx::query("INSERT INTO resources(id,kind,name,visibility) VALUES('arcade','app','Arcade','restricted')")
        .execute(&state.0.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO clients(id,secret,resource,redirect) VALUES('arcade-client',?,'arcade','http://arcade.localhost/cb')")
        .bind(auth::digest("never-shared"))
        .execute(&state.0.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,username,display_name,password,status,created) VALUES('member-id','member','Member','x','approved',0)")
        .execute(&state.0.db)
        .await
        .unwrap();
    let router = web::router(state.clone());
    let owner = user(&state, "owner").await;
    let member = user(&state, "member").await;

    let request = Request::get("/.well-known/openid-configuration")
        .header(header::HOST, "127.0.0.1:7070")
        .body(Body::empty())
        .unwrap();
    let (_, discovery) = send(&router, request).await;
    assert_eq!(
        discovery["device_authorization_endpoint"],
        "http://127.0.0.1:7070/oidc/device"
    );
    assert!(
        discovery["grant_types_supported"]
            .as_array()
            .unwrap()
            .contains(&json!(GRANT))
    );

    // The device asks without a secret, since it cannot keep one.
    let (status, started) = form(
        &router,
        "/oidc/device",
        &[
            ("client_id", "arcade-client"),
            ("scope", "openid profile"),
            ("name", "Living room console"),
            ("platform", "Linux"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let device_code = started["device_code"].as_str().unwrap().to_string();
    let code = started["user_code"].as_str().unwrap().to_string();
    assert!(
        started["verification_uri_complete"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/pair?code={code}"))
    );
    let poll = [
        ("grant_type", GRANT),
        ("client_id", "arcade-client"),
        ("device_code", device_code.as_str()),
    ];
    let (status, waiting) = form(&router, "/oidc/token", &poll).await;
    assert_eq!(
        (status, waiting["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("authorization_pending"))
    );
    let (_, waiting) = form(&router, "/oidc/token", &poll).await;
    assert_eq!(waiting["error"], "slow_down");

    // The approval page names the app, and only people with access may approve.
    let shown = device_codes::lookup(&state, &owner, &code.to_lowercase())
        .await
        .unwrap();
    assert_eq!(
        (
            shown["kind"].as_str(),
            shown["service"].as_str(),
            shown["name"].as_str()
        ),
        (Some("oidc"), Some("Arcade"), Some("Living room console"))
    );
    assert_eq!(
        device_codes::decide(&state, &member, &code, true)
            .await
            .unwrap_err()
            .0,
        StatusCode::FORBIDDEN
    );
    device_codes::decide(&state, &owner, &code, true)
        .await
        .unwrap();

    ready(&state).await;
    let (status, tokens) = form(&router, "/oidc/token", &poll).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["scope"], "openid profile");
    let access = tokens["access_token"].as_str().unwrap();
    let request = Request::get("/oidc/userinfo")
        .header(header::HOST, "127.0.0.1:7070")
        .header(header::AUTHORIZATION, format!("Bearer {access}"))
        .body(Body::empty())
        .unwrap();
    let (status, info) = send(&router, request).await;
    assert_eq!(
        (status, info["preferred_username"].as_str()),
        (StatusCode::OK, Some("owner"))
    );
    // The device refreshes without a secret, since its tokens came from a code.
    let refresh = tokens["refresh_token"].as_str().unwrap();
    let (status, refreshed) = form(
        &router,
        "/oidc/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", "arcade-client"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{refreshed}");
    let access = refreshed["access_token"].as_str().unwrap().to_string();
    // Tokens from a browser sign-in still need the client's secret to refresh.
    sqlx::query("UPDATE token_families SET public=0")
        .execute(&state.0.db)
        .await
        .unwrap();
    let next = refreshed["refresh_token"].as_str().unwrap();
    let (status, _) = form(
        &router,
        "/oidc/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", next),
            ("client_id", "arcade-client"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A code is spent once collected.
    ready(&state).await;
    let (_, spent) = form(&router, "/oidc/token", &poll).await;
    assert_eq!(spent["error"], "expired_token");

    // Declining, other clients, unknown clients and bad scopes.
    let (_, declined) = form(&router, "/oidc/device", &[("client_id", "arcade-client")]).await;
    device_codes::decide(
        &state,
        &owner,
        declined["user_code"].as_str().unwrap(),
        false,
    )
    .await
    .unwrap();
    let other = [
        ("grant_type", GRANT),
        ("client_id", "sync"),
        ("device_code", declined["device_code"].as_str().unwrap()),
    ];
    assert_eq!(
        form(&router, "/oidc/token", &other).await.0,
        StatusCode::UNAUTHORIZED
    );
    let poll = [
        ("grant_type", GRANT),
        ("client_id", "arcade-client"),
        ("device_code", declined["device_code"].as_str().unwrap()),
    ];
    assert_eq!(
        form(&router, "/oidc/token", &poll).await.1["error"],
        "access_denied"
    );
    assert_eq!(
        form(&router, "/oidc/device", &[("client_id", "nobody")])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        form(
            &router,
            "/oidc/device",
            &[("client_id", "arcade-client"), ("scope", "admin")]
        )
        .await
        .1["error"],
        "invalid_scope"
    );

    // A password change ends the device's access, as for any other session.
    sqlx::query("UPDATE users SET auth_version=auth_version+1 WHERE id=?")
        .bind(&owner.id)
        .execute(&state.0.db)
        .await
        .unwrap();
    let request = Request::get("/oidc/userinfo")
        .header(header::HOST, "127.0.0.1:7070")
        .header(header::AUTHORIZATION, format!("Bearer {access}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&router, request).await.0, StatusCode::UNAUTHORIZED);
}
