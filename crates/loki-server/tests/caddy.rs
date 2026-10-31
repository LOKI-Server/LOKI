use axum::{Json, Router, http::StatusCode, routing::get};
use loki_server::{State, caddy};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn invalid_apply_restores_active_caddy_configuration() {
    let binary = std::env::var("CADDY").unwrap_or_else(|_| "caddy".into());
    if std::process::Command::new(&binary)
        .arg("version")
        .output()
        .is_err()
    {
        eprintln!("Caddy CLI not installed; set CADDY to its executable to run native validation.");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path().into(), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = calls.clone();
    let api = Router::new()
        .route(
            "/config/",
            get(|| async { Json(json!({"admin":{"listen":"127.0.0.1:2019"}})) }),
        )
        .route(
            "/load",
            axum::routing::post(move |Json(value): Json<serde_json::Value>| {
                let calls = captured.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error":"simulated rejection"})),
                        )
                    } else {
                        assert_eq!(value, json!({"admin":{"listen":"127.0.0.1:2019"}}));
                        (StatusCode::OK, Json(json!({})))
                    }
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let mut settings = state.0.settings.read().await.clone();
    settings.caddy_binary = binary;
    settings.caddy_mode = "existing".into();
    settings.caddy_admin = format!("http://{address}");
    settings.caddy_enabled = true;
    let result = caddy::apply_settings(&state, &settings).await;
    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    settings.caddy_base = "{broken".into();
    assert!(caddy::preview(&state, &settings).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
    state.0.db.close().await;
}
#[tokio::test]
async fn managed_caddy_serves_routes_and_preserves_them_after_bad_edits() {
    let binary = std::env::var("CADDY").unwrap_or_else(|_| "caddy".into());
    if std::process::Command::new(&binary)
        .arg("version")
        .output()
        .is_err()
    {
        return;
    }
    let port = || {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let admin_port = port();
    let http_port = port();
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path().into(), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let mut settings = state.0.settings.read().await.clone();
    settings.caddy_binary = binary;
    settings.caddy_enabled = true;
    settings.caddy_admin = format!("http://127.0.0.1:{admin_port}");
    settings.caddy_base=json!({"apps":{"http":{"servers":{"local-test":{"listen":[format!("127.0.0.1:{http_port}")],"routes":[{"handle":[{"handler":"static_response","body":"held together"}]}]}}}}}).to_string();
    caddy::apply_settings(&state, &settings).await.unwrap();
    let client = loki_server::platform::http();
    let url = format!("http://127.0.0.1:{http_port}/");
    assert_eq!(
        client.get(&url).send().await.unwrap().text().await.unwrap(),
        "held together"
    );
    let valid = settings.caddy_base.clone();
    settings.caddy_base = "invalid json".into();
    assert!(caddy::apply_settings(&state, &settings).await.is_err());
    assert_eq!(
        client.get(&url).send().await.unwrap().text().await.unwrap(),
        "held together"
    );
    settings.caddy_base = valid;
    settings.caddy_enabled = false;
    caddy::apply_settings(&state, &settings).await.unwrap();
    assert!(client.get(&url).send().await.is_err());
    state.0.db.close().await;
}
