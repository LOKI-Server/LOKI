//! Signing in to an app with LOKI, in a page or a small window, and signing out of
//! the app or of LOKI.
mod common;
use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use loki_server::{State, apps, auth, web};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use tower::ServiceExt;

const LOKI: &str = "127.0.0.1:7070";
const APP: &str = "wasi-probe.localhost:7070";

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}
impl Reply {
    fn location(&self) -> &str {
        self.headers[header::LOCATION].to_str().unwrap()
    }
    /// The value of a cookie this reply sets.
    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.strip_prefix(&format!("{name}=")))
            .map(|v| v.split(';').next().unwrap().to_string())
    }
}
async fn send(
    router: &axum::Router,
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match body {
        Some(value) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into(),
    }
}
/// The path and query of a URL LOKI redirected to.
fn path_of(url: &str) -> String {
    let url = url::Url::parse(url).unwrap();
    format!("{}?{}", url.path(), url.query().unwrap_or(""))
}

/// An installation with the probe app enabled and the owner signed in to LOKI.
/// Returns the dashboard session cookie and its CSRF token.
async fn installation(directory: &std::path::Path) -> (State, axum::Router, String, String) {
    let build = common::builds("wasi_probe.wasm", "LOKI_COMPONENT_FIXTURE").remove(0);
    let state = State::open(directory.into(), LOKI.parse().unwrap())
        .await
        .unwrap();
    let token = state.0.setup_token.lock().await.clone().unwrap();
    let _=auth::setup(axum::extract::State(state.clone()),axum::Json(serde_json::from_value(json!({"token":token,"username":"owner","password":"owner-password-123","name":"Asgard","public_url":"http://127.0.0.1:7070"})).unwrap())).await.unwrap();
    let uid: String = sqlx::query_scalar("SELECT id FROM users WHERE owner=1")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    let manifest = json!({"protocol":1,"id":"wasi-probe","name":"WASI probe","version":"0.1.0","runtime":"wasi","target":"wasm32-wasip2","entry":"probe.wasm","health":"/health","capabilities":["database"],"files":{"probe.wasm":hex::encode(Sha256::digest(&build.wasm))},"built_with":{"sdk":build.sdk,"cargo_loki":build.sdk}});
    let package = directory.join("probe.loki-app");
    let mut archive = zip::ZipWriter::new(std::fs::File::create(&package).unwrap());
    let options = zip::write::SimpleFileOptions::default();
    archive.start_file("manifest.json", options).unwrap();
    archive.write_all(manifest.to_string().as_bytes()).unwrap();
    archive.start_file("probe.wasm", options).unwrap();
    archive.write_all(&build.wasm).unwrap();
    archive.finish().unwrap();
    apps::import(&state, package, &uid).await.unwrap();
    apps::action(&state, "wasi-probe", "enable").await.unwrap();
    let router = web::router(state.clone());

    let visit = send(&router, "GET", LOKI, "/api/session", &[], None).await;
    let cookie = visit.cookie("loki_session").unwrap();
    let csrf = serde_json::from_str::<Value>(&visit.body).unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_string();
    let jar = format!("loki_session={cookie}");
    let login = send(
        &router,
        "POST",
        LOKI,
        "/api/login",
        &[("cookie", &jar), ("x-csrf-token", &csrf)],
        Some(json!({"username":"owner","password":"owner-password-123"})),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.body);
    let jar = format!("loki_session={}", login.cookie("loki_session").unwrap());
    let session = send(
        &router,
        "GET",
        LOKI,
        "/api/session",
        &[("cookie", &jar)],
        None,
    )
    .await;
    let csrf = serde_json::from_str::<Value>(&session.body).unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_string();
    (state, router, jar, csrf)
}

/// Signs in to the app through LOKI and returns the app's session cookie and
/// where the sign-in ended up.
async fn sign_in(router: &axum::Router, loki: &str, start: &str) -> (String, Reply) {
    let started = send(router, "GET", APP, start, &[], None).await;
    assert_eq!(started.status, StatusCode::SEE_OTHER, "{}", started.body);
    let flow = format!(
        "loki_component_flow={}",
        started.cookie("loki_component_flow").unwrap()
    );
    let authorized = send(
        router,
        "GET",
        LOKI,
        &path_of(started.location()),
        &[("cookie", loki)],
        None,
    )
    .await;
    assert_eq!(
        authorized.status,
        StatusCode::SEE_OTHER,
        "{}",
        authorized.body
    );
    let callback = authorized.location();
    assert!(
        callback.starts_with("http://wasi-probe.localhost:7070/_loki/callback?"),
        "{callback}"
    );
    let done = send(
        router,
        "GET",
        APP,
        &path_of(callback),
        &[("cookie", &flow)],
        None,
    )
    .await;
    let session = done.cookie("loki_component").expect("an app session");
    (format!("loki_component={session}"), done)
}

#[tokio::test]
async fn signing_out_sticks_and_signing_out_of_loki_signs_out_of_apps() {
    let directory = tempfile::tempdir().unwrap();
    let (state, router, loki, csrf) = installation(directory.path()).await;
    let page = [("accept", "text/html,application/xhtml+xml")];

    // A browser that isn't signed in gets the app's own sign-in page, not a bounce.
    let landing = send(&router, "GET", APP, "/cookies?x=1", &page, None).await;
    assert_eq!(landing.status, StatusCode::OK);
    assert!(landing.body.contains("Sign in to WASI probe"));
    assert!(
        landing
            .body
            .contains("/_loki/start?next=%2Fcookies%3Fx%3D1")
    );
    assert!(landing.body.contains("data-popup"));
    let policy = landing.headers[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(
        policy.contains("script-src 'nonce-") && !policy.contains("unsafe"),
        "{policy}"
    );
    // Scripts and other non-page requests still go straight to signing in.
    let fetched = send(&router, "GET", APP, "/", &[], None).await;
    assert_eq!(fetched.location(), "/_loki/start");

    // LOKI's own sign-in page names the app.
    let started = send(&router, "GET", APP, "/_loki/start?next=%2F", &[], None).await;
    let anonymous = send(
        &router,
        "GET",
        LOKI,
        &path_of(started.location()),
        &[],
        None,
    )
    .await;
    assert!(
        anonymous
            .location()
            .starts_with("/login?next=%2Foidc%2Fauthorize")
    );
    assert!(
        anonymous.location().ends_with("&app=WASI+probe"),
        "{}",
        anonymous.location()
    );

    // In a small window, the sign-in hands the page back and closes.
    let (app, done) = sign_in(&router, &loki, "/_loki/start?next=%2Fcookies&popup=1").await;
    assert_eq!(done.status, StatusCode::OK);
    assert!(done.body.contains("window.opener.location.replace(next)"));
    assert!(done.body.contains("const next = \"/cookies\";"));
    // Without one, it returns to where you were; outside addresses are refused.
    let (_, plain) = sign_in(&router, &loki, "/_loki/start?next=%2Fcookies").await;
    assert_eq!(plain.location(), "/cookies");
    let (_, outside) = sign_in(&router, &loki, "/_loki/start?next=%2F%2Fevil.example").await;
    assert_eq!(outside.location(), "/");

    let signed_in = send(&router, "GET", APP, "/", &[("cookie", &app)], None).await;
    assert!(
        signed_in.body.contains("\"username\":\"owner\""),
        "{}",
        signed_in.body
    );
    // App sessions never outlive the LOKI session they came from.
    let (app_expires, loki_expires): (i64, i64) = sqlx::query_as("SELECT g.expires, s.expires FROM gateway_sessions g JOIN sessions s ON s.id = g.login_session LIMIT 1")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    assert!(app_expires <= loki_expires);

    // Signing out of the app stays on the app, and doesn't sign you straight back in.
    let raw = app.strip_prefix("loki_component=").unwrap();
    let app_csrf = auth::digest(&format!("component-csrf:{raw}"));
    let out = send(
        &router,
        "POST",
        APP,
        "/logout",
        &[
            ("cookie", &app),
            ("x-csrf-token", &app_csrf),
            ("origin", "http://wasi-probe.localhost:7070"),
        ],
        None,
    )
    .await;
    assert_eq!(out.location(), "/_loki/signed-out");
    let signed_out = send(&router, "GET", APP, "/_loki/signed-out", &page, None).await;
    assert!(signed_out.body.contains("signed out of WASI probe"));
    assert!(signed_out.body.contains("http://127.0.0.1:7070/sign-out"));
    let back = send(&router, "GET", APP, "/", &[("cookie", &app), page[0]], None).await;
    assert!(back.body.contains("Sign in to WASI probe"));

    // Signing out of LOKI signs out of the apps opened from it.
    let (app, _) = sign_in(&router, &loki, "/_loki/start?next=%2F").await;
    assert!(
        send(&router, "GET", APP, "/", &[("cookie", &app)], None)
            .await
            .body
            .contains("owner")
    );
    let logout = send(
        &router,
        "POST",
        LOKI,
        "/api/logout",
        &[("cookie", &loki), ("x-csrf-token", &csrf)],
        Some(json!({})),
    )
    .await;
    assert_eq!(logout.status, StatusCode::OK, "{}", logout.body);
    let after = send(&router, "GET", APP, "/", &[("cookie", &app), page[0]], None).await;
    assert!(
        after.body.contains("Sign in to WASI probe"),
        "{}",
        after.body
    );
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM gateway_sessions")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    assert_eq!(left, 0);
    state.0.db.close().await;
}
