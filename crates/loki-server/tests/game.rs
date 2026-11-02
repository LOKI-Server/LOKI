//! Game-server features of WASI apps: rooms, calls from HTTP, posts between rooms,
//! timers, batched messages, broadcasts and text frames, plus appended storage with
//! an owner's quota, outbound requests, scheduled jobs, streamed responses and kept
//! instances.
//!
//! Runs against each frozen SDK build of `tests/fixtures/game-room` (SDK 0.5 and
//! later), or a fresh wasm32-wasip2 build when LOKI_GAME_FIXTURE points at one.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use loki_server::{State, apps, auth, components, web};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Write, time::Duration};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};
use tower::ServiceExt;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const APP: &str = "game-room";

async fn connect(port: u16, path: &str) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://127.0.0.1:{port}{path}").into_client_request()?;
    request.headers_mut().insert(
        header::HOST,
        format!("{APP}.localhost:{port}").parse().unwrap(),
    );
    Ok(tokio_tungstenite::connect_async(request).await?.0)
}
async fn next(socket: &mut Socket) -> Message {
    tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("timed out waiting for a message")
        .expect("connection ended")
        .expect("connection failed")
}
/// The next message as text, whether sent as a text or a binary frame.
async fn text(socket: &mut Socket) -> String {
    match next(socket).await {
        Message::Text(text) => text.to_string(),
        Message::Binary(data) => String::from_utf8_lossy(&data).into_owned(),
        other => panic!("expected a message, got {other:?}"),
    }
}
async fn say(socket: &mut Socket, message: &str) {
    socket.send(Message::text(message)).await.unwrap();
}
async fn get(router: &axum::Router, port: u16, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(path)
        .header(header::HOST, format!("{APP}.localhost:{port}"))
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn game_servers_have_rooms_timers_calls_storage_outbound_and_schedules() {
    for build in common::builds("game_room.wasm", "LOKI_GAME_FIXTURE") {
        game(build.wasm, &build.sdk).await;
    }
}
async fn game(bytes: Vec<u8>, sdk: &str) {
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Something for the app to reach: only this address is on its outbound list.
    let remote = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_port = remote.local_addr().unwrap().port();
    tokio::spawn(async move {
        let app = axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        axum::serve(remote, app).await.unwrap();
    });
    let unlisted = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unlisted_port = unlisted.local_addr().unwrap().port();

    let state = State::open(
        directory.path().into(),
        format!("127.0.0.1:{port}").parse().unwrap(),
    )
    .await
    .unwrap();
    let token = state.0.setup_token.lock().await.clone().unwrap();
    let _ = auth::setup(axum::extract::State(state.clone()), axum::Json(serde_json::from_value(json!({"token":token,"username":"owner","password":"owner-password-123","name":"Game","public_url":"http://127.0.0.1:7070"})).unwrap())).await.unwrap();
    let uid: String = sqlx::query_scalar("SELECT id FROM users WHERE owner=1")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    let manifest = json!({"protocol":1,"id":APP,"name":"Game room","version":"0.1.0","runtime":"wasi","target":"wasm32-wasip2","entry":"game.wasm","health":"/health",
        "capabilities":["realtime","storage","outbound"],
        "outbound":[format!("127.0.0.1:{remote_port}")],
        "schedules":[{"name":"tick","every_minutes":60}],
        "limits":{"fuel":50_000_000,"memory_mib":32,"timeout_seconds":5,"reuse_instances":true,"storage_mib":1,
            "realtime":{"path":"/play","max_connections":8,"rooms":{"max":2,"idle_seconds":5}}},
        "files":{"game.wasm":hex::encode(Sha256::digest(&bytes))},"built_with":{"sdk":sdk,"cargo_loki":sdk}});
    let package = directory.path().join("game.loki-app");
    let mut archive = zip::ZipWriter::new(std::fs::File::create(&package).unwrap());
    for (name, data) in [
        ("manifest.json", manifest.to_string().into_bytes()),
        ("game.wasm", bytes.clone()),
    ] {
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(&data).unwrap();
    }
    archive.finish().unwrap();
    apps::import(&state, package, &uid, "upload").await.unwrap();
    sqlx::query("UPDATE resources SET visibility='public' WHERE id=?")
        .bind(APP)
        .execute(&state.0.db)
        .await
        .unwrap();
    apps::action(&state, APP, "enable").await.unwrap();
    let router = web::router(state.clone());
    let server = router
        .clone()
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
    // The compiled component is cached for the next start.
    let cache = directory.path().join("cache/components");
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);

    // Rooms: the main instance at /play, and one instance per room up to the limit.
    let mut main = connect(port, "/play").await.unwrap();
    // The greeting is a text frame (`send-text`); broadcasts below are binary.
    assert_eq!(next(&mut main).await, Message::text("room "));
    let mut alpha = connect(port, "/play/alpha").await.unwrap();
    assert_eq!(text(&mut alpha).await, "room alpha");
    let mut beta = connect(port, "/play/beta").await.unwrap();
    assert_eq!(text(&mut beta).await, "room beta");
    assert!(
        connect(port, "/play/gamma").await.is_err(),
        "a third room opened past the limit"
    );
    assert!(connect(port, "/play/Bad_Name").await.is_err());
    let (status, rooms) = get(&router, port, "/rooms").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&rooms).unwrap(),
        json!([{"id":"","connections":1},{"id":"alpha","connections":1},{"id":"beta","connections":1}])
    );

    // The HTTP side asks a room for its live state.
    assert_eq!(
        get(&router, port, "/call?room=alpha&q=hi").await.1,
        "alpha:1:hi"
    );
    assert_eq!(get(&router, port, "/call?room=&q=x").await.1, ":1:x");
    assert_eq!(
        get(&router, port, "/call?room=nope&q=x").await.0,
        StatusCode::NOT_FOUND
    );

    // Rooms post to each other, and the main instance.
    say(&mut alpha, "post beta hello").await;
    assert_eq!(text(&mut beta).await, "post from 'alpha': hello");
    say(&mut beta, "post  up").await;
    assert_eq!(text(&mut main).await, "post from 'beta': up");
    say(&mut alpha, "post nowhere x").await;
    assert_eq!(text(&mut alpha).await, "post failed 404");

    // Timers fire once, in the room that set them.
    say(&mut alpha, "timer 50").await;
    assert_eq!(text(&mut alpha).await, "timer 1 fired");

    // Broadcasts reach every connection in the room, once.
    let mut alpha2 = connect(port, "/play/alpha").await.unwrap();
    assert_eq!(text(&mut alpha2).await, "room alpha");
    say(&mut alpha, "all together").await;
    assert_eq!(text(&mut alpha).await, "together");
    assert_eq!(text(&mut alpha2).await, "together");

    // Messages that wait while the instance is busy arrive in one batch, in order.
    say(&mut alpha, "slow").await;
    for _ in 0..10 {
        say(&mut alpha, "x").await;
    }
    say(&mut alpha, "batch").await;
    let batch: usize = text(&mut alpha)
        .await
        .strip_prefix("batch ")
        .unwrap()
        .parse()
        .unwrap();
    assert!(batch > 1, "messages were not batched: {batch}");

    // An empty room closes after its idle time; the main instance never does.
    beta.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(7)).await;
    let rooms: Value = serde_json::from_str(&get(&router, port, "/rooms").await.1).unwrap();
    assert_eq!(
        rooms,
        json!([{"id":"","connections":1},{"id":"alpha","connections":2}])
    );
    let mut gamma = connect(port, "/play/gamma").await.unwrap();
    assert_eq!(text(&mut gamma).await, "room gamma");

    // Storage grows by appending, within the quota, and renames in one step.
    assert_eq!(get(&router, port, "/usage").await.1, (1024 * 1024).to_string());
    assert_eq!(get(&router, port, "/append?path=tmp&kib=400").await.1, "409600");
    assert_eq!(get(&router, port, "/append?path=tmp&kib=400").await.1, "819200");
    assert_eq!(
        get(&router, port, "/append?path=tmp&kib=400").await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(get(&router, port, "/rename?from=tmp&to=world").await.1, "819200");
    assert_eq!(get(&router, port, "/append?path=other&kib=1").await.1, "1024");
    assert_eq!(
        get(&router, port, "/rename?from=other&to=world").await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        get(&router, port, "/rename?from=other&to=world&replace").await.1,
        "1024"
    );
    // The owner can give the app more room than it asked for.
    sqlx::query("UPDATE apps SET storage_mib=4 WHERE id=?")
        .bind(APP)
        .execute(&state.0.db)
        .await
        .unwrap();
    apps::action(&state, APP, "restart").await.unwrap();
    assert_eq!(
        get(&router, port, "/usage").await.1,
        (4 * 1024 * 1024).to_string()
    );
    for size in [410_624, 820_224, 1_229_824] {
        assert_eq!(
            get(&router, port, "/append?path=world&kib=400").await.1,
            size.to_string()
        );
    }

    // Outbound requests reach only the listed host.
    assert_eq!(
        get(
            &router,
            port,
            &format!("/fetch?url=http://127.0.0.1:{remote_port}/ping")
        )
        .await
        .1,
        "200 pong"
    );
    for url in [
        format!("http://127.0.0.1:{unlisted_port}/ping"),
        format!("http://localhost:{remote_port}/ping"),
    ] {
        assert_eq!(
            get(&router, port, &format!("/fetch?url={url}")).await.0,
            StatusCode::FORBIDDEN,
            "{url}"
        );
    }

    // Large responses stream whole.
    let (status, body) = get(&router, port, "/stream").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), 300 * 1024);
    assert!(body.starts_with("abcdefghijklmnopqrstuvwxyz"));

    // Kept instances remember requests; a trap replaces the instance.
    let first: u64 = get(&router, port, "/count").await.1.parse().unwrap();
    let second: u64 = get(&router, port, "/count").await.1.parse().unwrap();
    assert_eq!(second, first + 1);
    assert_eq!(get(&router, port, "/trap").await.0, StatusCode::BAD_GATEWAY);
    let after: u64 = get(&router, port, "/count").await.1.parse().unwrap();
    assert!(after <= second, "the trapped instance was kept");

    // Scheduled jobs run without a user and can't be called from outside.
    components::run_schedule(&state, APP, "tick").await.unwrap();
    components::run_schedule(&state, APP, "tick").await.unwrap();
    assert_eq!(get(&router, port, "/runs").await.1, "2");
    assert!(components::run_schedule(&state, APP, "other").await.is_err());
    let outside = Request::builder()
        .method("POST")
        .uri("/_loki/schedule/tick")
        .header(header::HOST, format!("{APP}.localhost:{port}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.clone().oneshot(outside).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    drop((main, alpha, alpha2, gamma));
    tokio::time::timeout(
        Duration::from_secs(30),
        apps::action(&state, APP, "disable"),
    )
    .await
    .expect("disable waited on an open connection")
    .unwrap();
}
