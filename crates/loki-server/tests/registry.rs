mod common;
use axum::{Router, extract::Path, routing::get};
use loki_server::{State, apps, auth, registry};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex},
};

/// A registry on this computer: `index.json` and the packages it links to.
#[derive(Clone, Default)]
struct Registry {
    index: Arc<Mutex<Value>>,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

async fn serve(registry: Registry) -> String {
    let index = registry.index.clone();
    let files = registry.files.clone();
    let router = Router::new()
        .route(
            "/index.json",
            get(move || {
                let index = index.lock().unwrap().clone();
                // Null stands for a registry that is down.
                async move {
                    (!index.is_null())
                        .then_some(axum::Json(index))
                        .ok_or(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                }
            }),
        )
        .route(
            "/files/{name}",
            get(move |Path(name): Path<String>| {
                let file = files.lock().unwrap().get(&name).cloned();
                async move { file.ok_or(axum::http::StatusCode::NOT_FOUND) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}/")
}

fn package(wasm: &[u8], sdk: &str, version: &str) -> Vec<u8> {
    let manifest = json!({"protocol":1,"id":"wasi-probe","name":"WASI probe","version":version,"runtime":"wasi","target":"wasm32-wasip2","entry":"probe.wasm","health":"/health","capabilities":["database"],"files":{"probe.wasm":hex::encode(Sha256::digest(wasm))},"built_with":{"sdk":sdk,"cargo_loki":sdk}});
    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    archive.start_file("manifest.json", options).unwrap();
    archive.write_all(manifest.to_string().as_bytes()).unwrap();
    archive.start_file("probe.wasm", options).unwrap();
    archive.write_all(wasm).unwrap();
    archive.finish().unwrap().into_inner()
}

fn entry(base: &str, file: &str, bytes: &[u8], version: &str, sdk: &str) -> Value {
    json!({"id":"wasi-probe","name":"WASI probe","version":version,"url":format!("{base}files/{file}"),"sha256":hex::encode(Sha256::digest(bytes)),"size":bytes.len(),"sdk":sdk,"capabilities":["database"]})
}

async fn statuses(state: &State) -> Vec<String> {
    let list = registry::list(state, true).await.unwrap();
    list["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["status"].as_str().unwrap().to_string())
        .collect()
}

async fn installed(state: &State) -> (String, String) {
    let app = apps::get(state, "wasi-probe").await.unwrap();
    let manifest: apps::Manifest = serde_json::from_str(&app.manifest).unwrap();
    (manifest.version, app.source)
}

fn staging_is_empty(state: &State) -> bool {
    std::fs::read_dir(state.0.data.join("staging"))
        .unwrap()
        .next()
        .is_none()
}

#[tokio::test]
async fn registry_apps_install_update_and_refuse_files_that_were_not_reviewed() {
    let build = common::builds("wasi_probe.wasm", "LOKI_COMPONENT_FIXTURE")
        .pop()
        .unwrap();
    let sdk = build.sdk.as_str();
    let directory = tempfile::tempdir().unwrap();
    let state = State::open(directory.path().into(), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let token = state.0.setup_token.lock().await.clone().unwrap();
    let _ = auth::setup(axum::extract::State(state.clone()), axum::Json(serde_json::from_value(json!({"token":token,"username":"owner","password":"owner-password-123","name":"Registry","public_url":"http://127.0.0.1:7070"})).unwrap())).await.unwrap();
    let uid: String = sqlx::query_scalar("SELECT id FROM users WHERE owner=1")
        .fetch_one(&state.0.db)
        .await
        .unwrap();

    let server = Registry::default();
    let base = serve(server.clone()).await;
    state.0.settings.write().await.registry_url = base.clone();
    let first = package(&build.wasm, sdk, "0.1.0");
    let second = package(&build.wasm, sdk, "0.2.0");
    server.files.lock().unwrap().extend([
        ("first".into(), first.clone()),
        ("second".into(), second.clone()),
    ]);
    let publish = |apps: Vec<Value>| {
        *server.index.lock().unwrap() = json!({"registry":1,"generated":"now","apps":apps});
    };

    // A new app installs, and is remembered as coming from the registry.
    publish(vec![entry(&base, "first", &first, "0.1.0", sdk)]);
    assert_eq!(statuses(&state).await, ["install"]);
    registry::install(&state, "wasi-probe", &uid).await.unwrap();
    assert_eq!(installed(&state).await, ("0.1.0".into(), "registry".into()));
    assert_eq!(statuses(&state).await, ["installed"]);
    assert!(registry::install(&state, "wasi-probe", &uid).await.is_err());
    assert!(staging_is_empty(&state));

    // A file that isn't the reviewed one is refused and leaves nothing behind.
    let mut tampered = entry(&base, "second", &second, "0.2.0", sdk);
    tampered["sha256"] = json!("0".repeat(64));
    publish(vec![tampered]);
    assert_eq!(statuses(&state).await, ["update"]);
    let error = registry::install(&state, "wasi-probe", &uid)
        .await
        .unwrap_err();
    assert!(
        error.message.contains("not the package"),
        "{}",
        error.message
    );
    assert!(staging_is_empty(&state));

    // So is a reviewed file whose manifest isn't the listed release.
    publish(vec![entry(&base, "second", &second, "0.3.0", sdk)]);
    registry::list(&state, true).await.unwrap();
    let error = registry::install(&state, "wasi-probe", &uid)
        .await
        .unwrap_err();
    assert!(
        error.message.contains("not the release"),
        "{}",
        error.message
    );
    assert!(staging_is_empty(&state));
    assert_eq!(installed(&state).await.0, "0.1.0");

    // A newer release is offered on the dashboard and updates the app.
    publish(vec![entry(&base, "second", &second, "0.2.0", sdk)]);
    registry::list(&state, true).await.unwrap();
    let app = apps::get(&state, "wasi-probe").await.unwrap();
    assert_eq!(
        registry::update_for(&state, &app, "0.1.0").await.as_deref(),
        Some("0.2.0")
    );
    registry::install(&state, "wasi-probe", &uid).await.unwrap();
    assert_eq!(installed(&state).await, ("0.2.0".into(), "registry".into()));

    // An app built with a newer SDK than this LOKI is listed but not installable.
    let mut future = entry(&base, "second", &second, "0.3.0", "99.0.0");
    future["id"] = json!("future-app");
    publish(vec![future]);
    assert_eq!(statuses(&state).await, ["unsupported"]);
    assert!(registry::install(&state, "future-app", &uid).await.is_err());

    // Uploading a package over a registry app stops registry updates for it.
    publish(vec![entry(&base, "second", &second, "0.2.0", sdk)]);
    let upload = directory.path().join("upload.loki-app");
    std::fs::write(&upload, package(&build.wasm, sdk, "0.2.1")).unwrap();
    apps::import(&state, upload, &uid, "upload").await.unwrap();
    assert_eq!(installed(&state).await, ("0.2.1".into(), "upload".into()));
    let app = apps::get(&state, "wasi-probe").await.unwrap();
    assert_eq!(registry::update_for(&state, &app, "0.2.1").await, None);
    assert_eq!(statuses(&state).await, ["installed"]);

    // While the registry is down, LOKI shows the last index it read, even after a
    // restart, and says why it couldn't refresh.
    *server.index.lock().unwrap() = Value::Null;
    state.0.registry.lock().await.take();
    let list = registry::list(&state, true).await.unwrap();
    assert_eq!(list["apps"][0]["id"], "wasi-probe");
    assert!(list["error"].as_str().unwrap().contains("503"));
}
