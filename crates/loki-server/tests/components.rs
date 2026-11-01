mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use loki_server::{State, apps, auth, web};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use tower::ServiceExt;

async fn request(
    app: &axum::Router,
    path: &str,
    cookie: &str,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let request = Request::builder()
        .uri(path)
        .header(header::HOST, "wasi-probe.localhost:7070")
        .header(header::COOKIE, cookie)
        .header(header::AUTHORIZATION, "Bearer must-not-reach-the-guest")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or_else(|_| json!(String::from_utf8_lossy(&body))),
    )
}
/// Runs against each frozen SDK build, or a fresh one from LOKI_COMPONENT_FIXTURE.
#[tokio::test]
async fn wasi_guests_are_scoped_bounded_and_stop_without_stopping_loki() {
    for build in common::builds("wasi_probe.wasm", "LOKI_COMPONENT_FIXTURE") {
        sandbox(&build.wasm, &build.sdk).await;
    }
}
async fn sandbox(bytes: &[u8], sdk: &str) {
    let directory = tempfile::tempdir().unwrap();
    let state = State::open(directory.path().into(), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let token = state.0.setup_token.lock().await.clone().unwrap();
    let _=auth::setup(axum::extract::State(state.clone()),axum::Json(serde_json::from_value(json!({"token":token,"username":"owner","password":"owner-password-123","name":"Components","public_url":"http://127.0.0.1:7070"})).unwrap())).await.unwrap();
    let uid: String = sqlx::query_scalar("SELECT id FROM users WHERE owner=1")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    let mut manifest = json!({"protocol":1,"id":"wasi-probe","name":"WASI probe","version":"0.1.0","runtime":"wasi","target":"wasm32-wasip2","entry":"probe.wasm","health":"/health","roles":["admin"],"capabilities":["database"],"limits":{"fuel":1000000,"memory_mib":16,"timeout_seconds":2},"files":{"probe.wasm":hex::encode(Sha256::digest(bytes))},"built_with":{"sdk":sdk,"cargo_loki":sdk}});
    let package = directory.path().join("probe.loki-app");
    let mut archive = zip::ZipWriter::new(std::fs::File::create(&package).unwrap());
    archive
        .start_file("manifest.json", zip::write::SimpleFileOptions::default())
        .unwrap();
    archive.write_all(manifest.to_string().as_bytes()).unwrap();
    archive
        .start_file("probe.wasm", zip::write::SimpleFileOptions::default())
        .unwrap();
    archive.write_all(bytes).unwrap();
    archive.finish().unwrap();
    apps::import(&state, package, &uid).await.unwrap();
    apps::action(&state, "wasi-probe", "enable").await.unwrap();
    let router = web::router(state.clone());
    assert_eq!(request(&router, "/", "").await.0, StatusCode::SEE_OTHER);
    let raw = auth::random();
    sqlx::query(
        "INSERT INTO gateway_sessions(id,resource,user_id,expires) VALUES(?,'wasi-probe',?,?)",
    )
    .bind(auth::digest(&raw))
    .bind(&uid)
    .bind(auth::now() + 60)
    .execute(&state.0.db)
    .await
    .unwrap();
    let cookie = format!("loki_component={raw}");
    let (_, headers, context) = request(&router, "/", &cookie).await;
    assert_eq!(context["user"]["id"], uid);
    assert_eq!(context["user"]["roles"], json!(["access", "admin"]));
    assert!(context.get("password").is_none());
    assert!(context.get("client_secret").is_none());
    assert!(headers.get(header::SET_COOKIE).is_none());
    let (_, _, visible) = request(&router, "/cookies", &cookie).await;
    assert_eq!(visible, json!({"cookies":0,"authorization":0}));
    for path in [
        "/fs",
        "/network",
        "/sql-escape",
        "/missing-capability",
        "/missing-storage",
    ] {
        let (status, _, result) = request(&router, path, &cookie).await;
        assert_eq!(status, StatusCode::OK, "{path}: {result}");
        assert_eq!(result["denied"], true, "{path}");
    }
    for path in ["/trap", "/loop", "/memory"] {
        assert_eq!(
            request(&router, path, &cookie).await.0,
            StatusCode::BAD_GATEWAY,
            "{path}"
        );
        assert_eq!(request(&router, "/", &cookie).await.0, StatusCode::OK);
    }
    // Add storage through an ordinary same-version update, then exercise it
    // from an actual guest, including streamed and active-format responses.
    manifest["capabilities"] = json!(["database", "storage"]);
    let package = directory.path().join("storage.loki-app");
    let mut archive = zip::ZipWriter::new(std::fs::File::create(&package).unwrap());
    archive
        .start_file("manifest.json", zip::write::SimpleFileOptions::default())
        .unwrap();
    archive.write_all(manifest.to_string().as_bytes()).unwrap();
    archive
        .start_file("probe.wasm", zip::write::SimpleFileOptions::default())
        .unwrap();
    archive.write_all(bytes).unwrap();
    archive.finish().unwrap();
    apps::import(&state, package, &uid).await.unwrap();
    let (status, _, stored) = request(&router, "/storage", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored["preferences"]["value"]["theme"], "forest");
    assert_eq!(stored["traversal"]["status"], 400);
    let (status, headers, body) = request(&router, "/storage-download", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "Hello, branch.");
    assert!(
        headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    let (_, headers, _) = request(&router, "/storage-active", &cookie).await;
    assert!(
        headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    let (status, _, body) = request(&router, "/storage-abort", &cookie).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, "denied");
    apps::action(&state, "wasi-probe", "disable").await.unwrap();
    apps::action(&state, "wasi-probe", "enable").await.unwrap();
    assert_eq!(
        request(&router, "/storage-read", &cookie).await.2["value"]["theme"],
        "forest"
    );
    apps::action(&state, "wasi-probe", "disable").await.unwrap();
    assert_eq!(
        request(&router, "/", &cookie).await.0,
        StatusCode::NOT_FOUND
    );
    let health = router
        .oneshot(
            Request::builder()
                .uri("/health")
                .header(header::HOST, "127.0.0.1:7070")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    state.0.db.close().await;
}
