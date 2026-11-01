//! Realtime WASI instances: long-lived state, WebSockets held by LOKI, ticks, file
//! streaming, limits, failure recovery, restarts, the LAN listener and shutdown.
//!
//! Build `tests/fixtures/realtime-echo` for wasm32-wasip2 and set
//! LOKI_REALTIME_FIXTURE to its .wasm to run this test.
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use loki_server::{State, apps, auth, components, web};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{io::Write, net::SocketAddr, time::Duration};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};
use tower::ServiceExt;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const APP: &str = "realtime-echo";

async fn connect(port: u16, host: &str) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://127.0.0.1:{port}/ws").into_client_request()?;
    request
        .headers_mut()
        .insert(header::HOST, host.parse().unwrap());
    Ok(tokio_tungstenite::connect_async(request).await?.0)
}
async fn next(socket: &mut Socket) -> Message {
    tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("timed out waiting for a message")
        .expect("connection ended")
        .expect("connection failed")
}
async fn text(socket: &mut Socket) -> String {
    match next(socket).await {
        Message::Binary(data) => String::from_utf8(data.to_vec()).unwrap(),
        Message::Text(text) => text.to_string(),
        other => panic!("expected a message, got {other:?}"),
    }
}
async fn say(socket: &mut Socket, message: &str) {
    socket.send(Message::text(message)).await.unwrap();
}
/// Reads until the server closes the connection, returning the close code.
async fn closed(socket: &mut Socket) -> u16 {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("timed out waiting for close")
        {
            Some(Ok(Message::Close(frame))) => {
                return frame.map(|f| u16::from(f.code)).unwrap_or(0);
            }
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => return 0,
        }
    }
}
async fn get(router: &axum::Router, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(path)
        .header(header::HOST, format!("{APP}.localhost:{}", port_of(router)))
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into())
}
fn port_of(_: &axum::Router) -> u16 {
    PORT.with(|p| p.get())
}
thread_local! {static PORT: std::cell::Cell<u16> = const { std::cell::Cell::new(0) };}
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn realtime_apps_hold_state_and_connections_without_holding_loki() {
    let Ok(fixture) = std::env::var("LOKI_REALTIME_FIXTURE") else {
        eprintln!(
            "Set LOKI_REALTIME_FIXTURE to the compiled realtime-echo .wasm to run the realtime test."
        );
        return;
    };
    let bytes = std::fs::read(fixture).unwrap();
    let blob: Vec<u8> = (0..3 * 1024 * 1024 + 5)
        .map(|i| (i * 31 % 251) as u8)
        .collect();
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    PORT.with(|p| p.set(port));
    let state = State::open(
        directory.path().into(),
        format!("127.0.0.1:{port}").parse().unwrap(),
    )
    .await
    .unwrap();
    let token = state.0.setup_token.lock().await.clone().unwrap();
    let _ = auth::setup(axum::extract::State(state.clone()), axum::Json(serde_json::from_value(json!({"token":token,"username":"owner","password":"owner-password-123","name":"Realtime","public_url":"http://127.0.0.1:7070"})).unwrap())).await.unwrap();
    let uid: String = sqlx::query_scalar("SELECT id FROM users WHERE owner=1")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    let manifest = json!({"protocol":1,"id":APP,"name":"Realtime echo","version":"0.1.0","runtime":"wasi","target":"wasm32-wasip2","entry":"echo.wasm","health":"/health","capabilities":["realtime","storage","uploads"],"roles":["admin"],"panel":true,
        "limits":{"fuel":50_000_000,"memory_mib":32,"timeout_seconds":5,"realtime":{"path":"/ws","tick_ms":50,"max_connections":3,"max_message_bytes":65536,"event_fuel":5_000_000}},
        "files":{"echo.wasm":hex::encode(Sha256::digest(&bytes)),"blob.bin":hex::encode(Sha256::digest(&blob))}});
    let package = directory.path().join("echo.loki-app");
    let mut archive = zip::ZipWriter::new(std::fs::File::create(&package).unwrap());
    for (name, data) in [
        ("manifest.json", manifest.to_string().into_bytes()),
        ("echo.wasm", bytes),
        ("blob.bin", blob.clone()),
    ] {
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(&data).unwrap();
    }
    archive.finish().unwrap();
    apps::import(&state, package, &uid).await.unwrap();
    // The page routes below are public; the socket would be open either way.
    sqlx::query("UPDATE resources SET visibility='public' WHERE id=?")
        .bind(APP)
        .execute(&state.0.db)
        .await
        .unwrap();
    apps::action(&state, APP, "enable").await.unwrap();
    let router = web::router(state.clone());
    let server = router
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
    let host = format!("{APP}.localhost:{port}");

    // Anonymous players connect, and the instance keeps its state between events.
    let mut a = connect(port, &host).await.unwrap();
    assert_eq!(text(&mut a).await, "open true");
    say(&mut a, "hello").await;
    assert_eq!(text(&mut a).await, "hello");
    let mut b = connect(port, &host).await.unwrap();
    assert_eq!(text(&mut b).await, "open true");
    say(&mut a, "all hi").await;
    assert_eq!(text(&mut a).await, "hi");
    assert_eq!(text(&mut b).await, "hi");
    say(&mut a, "count").await;
    assert_eq!(text(&mut a).await, "2");
    tokio::time::sleep(Duration::from_millis(400)).await;
    say(&mut a, "ticks").await;
    let ticks: u64 = text(&mut a).await[6..].parse().unwrap();
    assert!(ticks >= 4, "only {ticks} ticks");

    // Files stream in order with other messages, straight from disk.
    say(&mut a, "file").await;
    let mut received = Vec::new();
    while received.len() < blob.len() {
        match next(&mut a).await {
            Message::Binary(data) => {
                assert!(data.len() <= 65536);
                received.extend_from_slice(&data);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(received == blob, "streamed file differs");
    assert_eq!(text(&mut a).await, format!("sent {}", blob.len()));
    assert_eq!(get(&router, "/size").await.1, blob.len().to_string());
    assert_eq!(get(&router, "/escape").await.1, "true");

    // The app's panel, drawn by LOKI's dashboard: everyone with access sees it, the
    // app decides what each role gets, and browsers can't reach it on the app itself.
    let owner: auth::User = sqlx::query_as("SELECT * FROM users WHERE id=?")
        .bind(&uid)
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    let shown = components::panel(&state, APP, &owner, None).await.unwrap();
    assert_eq!(shown["panel"]["sections"].as_array().unwrap().len(), 2);
    assert_eq!(
        shown["panel"]["sections"][0]["items"][0]["items"][0]["value"],
        "0"
    );
    let bump = Request::post("/")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"action":"bump","value":"2"}).to_string()))
        .unwrap();
    let bumped = components::panel(&state, APP, &owner, Some(bump))
        .await
        .unwrap();
    assert_eq!(bumped["notice"], "Bumped.");
    assert_eq!(
        bumped["panel"]["sections"][0]["items"][0]["items"][0]["value"],
        "2"
    );
    let upload = Request::post("/")
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=XX")
        .body(Body::from(
            [
                "--XX",
                "Content-Disposition: form-data; name=\"action\"",
                "",
                "upload",
                "--XX",
                "Content-Disposition: form-data; name=\"file\"; filename=\"note.txt\"",
                "Content-Type: text/plain",
                "",
                "hello",
                "--XX--",
                "",
            ]
            .join("\r\n"),
        ))
        .unwrap();
    let stored = components::panel(&state, APP, &owner, Some(upload))
        .await
        .unwrap();
    assert_eq!(stored["notice"], "Stored.");
    let kept = directory
        .path()
        .join("app-data")
        .join(APP)
        .join("uploads")
        .join("panel")
        .join("note.txt");
    assert_eq!(std::fs::read_to_string(kept).unwrap(), "hello");
    let broken = Request::post("/")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"action":"broken"}).to_string()))
        .unwrap();
    assert_eq!(
        components::panel(&state, APP, &owner, Some(broken))
            .await
            .unwrap_err()
            .status,
        StatusCode::BAD_GATEWAY
    );
    sqlx::query("INSERT INTO users(id,username,display_name,password,status,created) VALUES('member-id','member','Member','x','approved',0)")
        .execute(&state.0.db)
        .await
        .unwrap();
    let member: auth::User = sqlx::query_as("SELECT * FROM users WHERE id='member-id'")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    let seen = components::panel(&state, APP, &member, None).await.unwrap();
    assert_eq!(
        seen["panel"]["sections"].as_array().unwrap().len(),
        1,
        "members see no admin section"
    );
    sqlx::query("UPDATE resources SET visibility='restricted' WHERE id=?")
        .bind(APP)
        .execute(&state.0.db)
        .await
        .unwrap();
    assert_eq!(
        components::panel(&state, APP, &member, None)
            .await
            .unwrap_err()
            .status,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE resources SET visibility='public' WHERE id=?")
        .bind(APP)
        .execute(&state.0.db)
        .await
        .unwrap();
    assert_eq!(get(&router, "/_loki/panel").await.0, StatusCode::NOT_FOUND);

    // A device signed in with a code is a known user, over HTTP and on the socket.
    assert_eq!(get(&router, "/whoami").await.1, "anonymous");
    let access = device_token(&state, &router, &uid).await;
    let request = Request::builder()
        .uri("/whoami")
        .header(header::HOST, &host)
        .header(header::AUTHORIZATION, format!("Bearer {access}"))
        .body(Body::empty())
        .unwrap();
    let body = router
        .clone()
        .oneshot(request)
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(body, "owner");
    let mut request = format!("ws://127.0.0.1:{port}/ws")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert(header::HOST, host.parse().unwrap());
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {access}").parse().unwrap(),
    );
    let (mut known, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(text(&mut known).await, "open true as owner");
    known.close(None).await.unwrap();

    // Connection and message limits.
    let mut c = connect(port, &host).await.unwrap();
    assert_eq!(text(&mut c).await, "open true");
    assert!(
        connect(port, &host).await.is_err(),
        "a fourth connection was accepted"
    );
    c.send(Message::binary(vec![0u8; 70_000])).await.unwrap();
    assert_eq!(closed(&mut c).await, 1009);
    // The app may close a connection itself.
    say(&mut b, "close").await;
    assert_eq!(closed(&mut b).await, 4000);
    // A connection that falls behind is closed rather than buffered.
    let mut slow = connect(port, &host).await.unwrap();
    assert_eq!(text(&mut slow).await, "open true");
    say(&mut slow, "flood").await;
    assert_eq!(closed(&mut slow).await, 1008);
    tokio::time::sleep(Duration::from_millis(200)).await;
    say(&mut a, "count").await;
    assert_eq!(text(&mut a).await, "1");

    // A trap closes every connection; a fresh instance follows after a short pause.
    say(&mut a, "panic").await;
    assert_eq!(closed(&mut a).await, 1011);
    let mut a = reconnect(port, &host).await;
    say(&mut a, "count").await;
    assert_eq!(text(&mut a).await, "1");
    // Running out of fuel counts as a failure too.
    say(&mut a, "spin").await;
    assert_eq!(closed(&mut a).await, 1011);
    let mut a = reconnect(port, &host).await;

    // An HTTP handler can restart the instance.
    assert_eq!(get(&router, "/restart").await.1, "restarted");
    assert_eq!(closed(&mut a).await, 1012);
    let mut a = reconnect(port, &host).await;
    say(&mut a, "ticks").await;
    assert!(text(&mut a).await.starts_with("ticks "));

    // The LAN listener answers by IP, but only the socket and the health check.
    let lan = free_port();
    sqlx::query("UPDATE apps SET lan_bind=? WHERE id=?")
        .bind(format!("127.0.0.1:{lan}"))
        .bind(APP)
        .execute(&state.0.db)
        .await
        .unwrap();
    apps::action(&state, APP, "restart").await.unwrap();
    assert_eq!(closed(&mut a).await, 1001);
    let mut on_lan = connect(lan, &format!("192.168.1.5:{lan}")).await.unwrap();
    assert_eq!(text(&mut on_lan).await, "open true");
    // The app learns its LAN address, to tell players where to connect.
    let shown = components::panel(&state, APP, &owner, None).await.unwrap();
    assert_eq!(
        shown["panel"]["sections"][0]["items"][1]["text"],
        format!("127.0.0.1:{lan} 127.0.0.1")
    );
    let client = reqwest_get(lan, "/health").await;
    assert_eq!(client, (200, "ready".into()));
    assert_eq!(reqwest_get(lan, "/size").await.0, 404);

    // Backups and disabling end sessions instead of waiting on them.
    let snapshot = tokio::time::timeout(Duration::from_secs(30), web::snapshot(&state, true))
        .await
        .expect("backup waited on an open connection");
    snapshot.unwrap();
    assert_eq!(closed(&mut on_lan).await, 1001);
    let mut on_lan = reconnect(lan, &format!("192.168.1.5:{lan}")).await;
    tokio::time::timeout(
        Duration::from_secs(30),
        apps::action(&state, APP, "disable"),
    )
    .await
    .expect("disable waited on an open connection")
    .unwrap();
    assert_eq!(closed(&mut on_lan).await, 1001);
    assert!(
        connect(lan, "x").await.is_err(),
        "the LAN listener outlived the app"
    );
    assert!(connect(port, &host).await.is_err());
}

/// Signs a device in to the app with a code, approved by the owner.
async fn device_token(state: &State, router: &axum::Router, uid: &str) -> String {
    async fn form(router: &axum::Router, path: &str, body: String) -> serde_json::Value {
        let request = Request::post(path)
            .header(header::HOST, "127.0.0.1:7070")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        let body = router
            .clone()
            .oneshot(request)
            .await
            .unwrap()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        serde_json::from_slice(&body).unwrap()
    }
    let client = format!("component-{APP}");
    let started = form(router, "/oidc/device", format!("client_id={client}")).await;
    let owner: auth::User = sqlx::query_as("SELECT * FROM users WHERE id=?")
        .bind(uid)
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    loki_server::device_codes::decide(state, &owner, started["user_code"].as_str().unwrap(), true)
        .await
        .unwrap();
    let grant = "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code";
    let device = started["device_code"].as_str().unwrap();
    let tokens = form(
        router,
        "/oidc/token",
        format!("grant_type={grant}&client_id={client}&device_code={device}"),
    )
    .await;
    tokens["access_token"].as_str().unwrap().to_string()
}

/// Connects once the instance is back, after a failure or restart.
async fn reconnect(port: u16, host: &str) -> Socket {
    for _ in 0..100 {
        if let Ok(mut socket) = connect(port, host).await
            && let Ok(Some(Ok(message))) =
                tokio::time::timeout(Duration::from_secs(2), socket.next()).await
            && message.to_text().unwrap_or("") == "open true"
        {
            return socket;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the realtime instance did not come back");
}

/// A plain HTTP GET over TCP, returning the status and body.
async fn reqwest_get(port: u16, path: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 192.168.1.5\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}
