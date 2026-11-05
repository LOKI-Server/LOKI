use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use loki_server::{State, auth, web};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Browser {
    app: Router,
    cookie: String,
    csrf: String,
}
impl Browser {
    fn new(app: Router) -> Self {
        Self {
            app,
            cookie: String::new(),
            csrf: String::new(),
        }
    }
    async fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "127.0.0.1:7070")
            .header(header::COOKIE, &self.cookie);
        let body = if let Some(value) = body {
            request = request
                .header("x-csrf-token", &self.csrf)
                .header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        } else {
            Body::empty()
        };
        let response = self
            .app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        if let Some(cookie) = response.headers().get(header::SET_COOKIE) {
            self.cookie = cookie.to_str().unwrap().split(';').next().unwrap().into();
        }
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&bytes)}));
        (status, value)
    }
    async fn session(&mut self) -> Value {
        let (s, v) = self.request("GET", "/api/session", None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        self.csrf = v["csrf"].as_str().unwrap().into();
        v
    }
    async fn post(&mut self, path: &str, value: Value) -> Value {
        let (s, v) = self.request("POST", path, Some(value)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }
    async fn action(&mut self, name: &str, id: &str, data: Value) -> Value {
        self.post("/api/action", json!({"action":name,"id":id,"data":data}))
            .await
    }
}
async fn installation() -> (tempfile::TempDir, State, Browser) {
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path().into(), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let mut browser = Browser::new(web::router(state.clone()));
    browser.session().await;
    let token = state.0.setup_token.lock().await.clone().unwrap();
    browser.post("/api/setup",json!({"token":token,"username":"owner","password":"owner-password-123","name":"Test universe","public_url":"http://127.0.0.1:7070"})).await;
    browser
        .post(
            "/api/login",
            json!({"username":"owner","password":"owner-password-123"}),
        )
        .await;
    browser.session().await;
    (tmp, state, browser)
}

async fn file_api(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, bytes::Bytes) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "127.0.0.1:7070");
    if !token.is_empty() {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    let body = match body {
        Some(value) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

async fn download_device(app: &Router, username: &str) -> Value {
    let (status, _, body) = file_api(app, "", "POST", "/files/v1/login",
        Some(json!({"username":username,"password":"owner-password-123","name":"Downloader","platform":"test"})), &[]).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn account_pairing_works_for_files_and_sync_and_is_managed_from_account() {
    let (_tmp, state, mut owner) = installation().await;
    let app = web::router(state.clone());
    let (status, _, bytes) = file_api(
        &app,
        "",
        "POST",
        "/auth/v1/pair",
        Some(json!({"name":"Combined client","platform":"test"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pair: Value = serde_json::from_slice(&bytes).unwrap();
    let shown = owner
        .action("device-lookup", "", json!({"code":pair["user_code"]}))
        .await;
    assert_eq!(shown["kind"], "account");
    assert_eq!(shown["service"], "LOKI account");
    owner
        .action("device-approve", "", json!({"code":pair["user_code"]}))
        .await;
    let (status, _, bytes) = file_api(
        &app,
        "",
        "POST",
        "/auth/v1/pair/poll",
        Some(json!({"device_code":pair["device_code"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let issued: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(issued["access"]["sync"], true);
    let token = issued["token"].as_str().unwrap();
    let id = issued["device"]["id"].as_str().unwrap();
    for path in [
        "/auth/v1/me",
        "/sync/v1/me",
        "/files/v1/me",
        "/files/v1/shares",
    ] {
        assert_eq!(
            file_api(&app, token, "GET", path, None, &[]).await.0,
            StatusCode::OK,
            "{path}"
        );
    }
    let (_, dashboard) = owner.request("GET", "/api/state", None).await;
    assert_eq!(dashboard["devices"][0]["id"], id);
    assert!(dashboard["devices"][0].get("token").is_none());
    owner
        .action("device-rename", id, json!({"name":"Renamed device"}))
        .await;
    let (_, _, bytes) = file_api(&app, token, "GET", "/auth/v1/me", None, &[]).await;
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["device"]["name"],
        "Renamed device"
    );
    owner.action("device-revoke", id, json!({})).await;
    for path in ["/auth/v1/me", "/sync/v1/me", "/files/v1/me"] {
        assert_eq!(
            file_api(&app, token, "GET", path, None, &[]).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        file_api(
            &app,
            "",
            "POST",
            "/auth/v1/pair/poll",
            Some(json!({"device_code":pair["device_code"]})),
            &[]
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    state.0.db.close().await;
}

#[tokio::test]
async fn accounts_without_sync_can_pair_and_manage_their_devices() {
    let (_tmp, state, _owner) = installation().await;
    sqlx::query("INSERT INTO users(id,username,display_name,password,status,created) SELECT 'reader','reader','Reader',password,'approved',created FROM users WHERE owner=1")
        .execute(&state.0.db).await.unwrap();
    let app = web::router(state.clone());
    let mut reader = Browser::new(app.clone());
    reader.session().await;
    reader
        .post(
            "/api/login",
            json!({"username":"reader","password":"owner-password-123"}),
        )
        .await;
    reader.session().await;
    let (_, _, bytes) = file_api(
        &app,
        "",
        "POST",
        "/auth/v1/pair",
        Some(json!({"name":"File reader"})),
        &[],
    )
    .await;
    let pair: Value = serde_json::from_slice(&bytes).unwrap();
    reader
        .action("device-approve", "", json!({"code":pair["user_code"]}))
        .await;
    let (status, _, bytes) = file_api(
        &app,
        "",
        "POST",
        "/auth/v1/pair/poll",
        Some(json!({"device_code":pair["device_code"]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let issued: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(issued["access"]["sync"], false);
    let token = issued["token"].as_str().unwrap();
    assert_eq!(
        file_api(&app, token, "GET", "/files/v1/shares", None, &[])
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        file_api(&app, token, "GET", "/sync/v1/me", None, &[])
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    reader
        .action(
            "device-rename",
            issued["device"]["id"].as_str().unwrap(),
            json!({"name":"Reader"}),
        )
        .await;
    let (_, dashboard) = reader.request("GET", "/api/state", None).await;
    assert!(dashboard.get("sync").is_none());
    assert_eq!(dashboard["devices"][0]["name"], "Reader");
    reader
        .action(
            "device-revoke",
            issued["device"]["id"].as_str().unwrap(),
            json!({}),
        )
        .await;
    assert_eq!(
        file_api(&app, token, "GET", "/files/v1/me", None, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    state.0.db.close().await;
}

#[tokio::test]
async fn upgrading_reuses_old_sync_and_file_tokens_without_signing_in_again() {
    let (tmp, state, _owner) = installation().await;
    for (table, id) in [("sync_devices", "old-sync"), ("file_devices", "old-files")] {
        sqlx::query(&format!("INSERT INTO {table}(id,user_id,name,token,auth_version,created,last_seen) SELECT ?,id,?,?,auth_version,1,1 FROM users WHERE owner=1"))
            .bind(id).bind(id).bind(auth::digest(id)).execute(&state.0.db).await.unwrap();
    }
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    sqlx::query(&format!("PRAGMA user_version={}", version - 4))
        .execute(&state.0.db)
        .await
        .unwrap();
    state.0.db.close().await;
    let upgraded = State::open(tmp.path().into(), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let app = web::router(upgraded.clone());
    for token in ["old-sync", "old-files"] {
        for path in ["/auth/v1/me", "/sync/v1/me", "/files/v1/me"] {
            assert_eq!(
                file_api(&app, token, "GET", path, None, &[]).await.0,
                StatusCode::OK,
                "{token} {path}"
            );
        }
        assert_eq!(
            file_api(&app, token, "DELETE", "/auth/v1/device", None, &[])
                .await
                .0,
            StatusCode::OK
        );
        for path in ["/sync/v1/me", "/files/v1/me"] {
            assert_eq!(
                file_api(&app, token, "GET", path, None, &[]).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
    }
    upgraded.0.db.close().await;
}

#[tokio::test]
async fn native_downloads_browse_hash_resume_and_reject_unsafe_paths() {
    use sha2::{Digest, Sha256};
    let (tmp, state, mut owner) = installation().await;
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(root.join("nested")).unwrap();
    std::fs::write(root.join("日本語.txt"), b"0123456789").unwrap();
    std::fs::write(root.join("empty"), b"").unwrap();
    std::fs::write(root.join("hidden.TMP"), b"secret").unwrap();
    std::fs::write(root.join("nested/hidden.tmp"), b"secret").unwrap();
    for n in 0..110 {
        std::fs::write(root.join(format!("item{n:03}")), b"x").unwrap();
    }
    let share = owner
        .action(
            "share-save",
            "",
            json!({"name":"Archive","root":root,"excluded_extensions":"tmp"}),
        )
        .await;
    let id = share["id"].as_str().unwrap();
    let app = web::router(state.clone());
    let (status, headers, bytes) = file_api(&app, "", "GET", "/files/v1", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["read_only"],
        true
    );
    let device = download_device(&app, "owner").await;
    let token = device["token"].as_str().unwrap();
    let (_, _, bytes) = file_api(&app, token, "GET", "/files/v1/shares", None, &[]).await;
    let shares: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(shares["shares"][0]["id"], id);
    assert!(
        shares
            .to_string()
            .find(&root.to_string_lossy().to_string())
            .is_none()
    );
    let prefix = format!("/files/v1/shares/{id}");
    let (_, _, bytes) = file_api(&app, token, "GET", &format!("{prefix}/entries"), None, &[]).await;
    let listing: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(listing["total"], 113);
    assert_eq!(listing["entries"].as_array().unwrap().len(), 100);
    assert_eq!(listing["entries"][0]["name"], "nested");
    let (_, _, bytes) = file_api(
        &app,
        token,
        "GET",
        &format!("{prefix}/entries?page=1"),
        None,
        &[],
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["entries"]
            .as_array()
            .unwrap()
            .len(),
        13
    );
    for suffix in ["page=1000", "sort=wrong"] {
        assert_eq!(
            file_api(
                &app,
                token,
                "GET",
                &format!("{prefix}/entries?{suffix}"),
                None,
                &[]
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let encoded = url::form_urlencoded::byte_serialize("日本語.txt".as_bytes()).collect::<String>();
    let uri = format!("{prefix}/file?path={encoded}");
    let (status, headers, bytes) = file_api(&app, token, "GET", &uri, None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[..], b"0123456789");
    assert_eq!(headers[header::CONTENT_LENGTH], "10");
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert!(
        headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    assert!(
        headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .contains(&encoded)
    );
    let date = headers[header::LAST_MODIFIED].to_str().unwrap();
    let (status, headers, bytes) =
        file_api(&app, token, "HEAD", &uri, None, &[("range", "bytes=2-5")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], "10");
    assert!(bytes.is_empty());
    for (range, expected) in [
        ("bytes=2-5", &b"2345"[..]),
        ("bytes=7-", &b"789"[..]),
        ("bytes=-3", &b"789"[..]),
    ] {
        let (status, _, bytes) = file_api(
            &app,
            token,
            "GET",
            &uri,
            None,
            &[("range", range), ("if-range", date)],
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(&bytes[..], expected);
    }
    let (status, headers, _) =
        file_api(&app, token, "GET", &uri, None, &[("range", "bytes=99-")]).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::CONTENT_RANGE], "bytes */10");
    for validator in ["Thu, 01 Jan 1970 00:00:00 GMT", "W/\"old\""] {
        let (status, _, bytes) = file_api(
            &app,
            token,
            "GET",
            &uri,
            None,
            &[("range", "bytes=7-"), ("if-range", validator)],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&bytes[..], b"0123456789");
    }
    for (checksum, expected) in [
        (false, Value::Null),
        (true, json!(hex::encode(Sha256::digest(b"0123456789")))),
    ] {
        let (status, _, bytes) = file_api(
            &app,
            token,
            "GET",
            &format!("{prefix}/metadata?path={encoded}&checksum={checksum}"),
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let meta: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(meta["size"], 10);
        assert_eq!(meta["sha256"], expected);
        assert_eq!(meta["last_modified"], date);
    }
    for endpoint in ["file", "metadata"] {
        for name in ["hidden.TMP", "nested/hidden.tmp", "nested", "missing"] {
            assert_eq!(
                file_api(
                    &app,
                    token,
                    "GET",
                    &format!("{prefix}/{endpoint}?path={name}"),
                    None,
                    &[]
                )
                .await
                .0,
                StatusCode::NOT_FOUND,
                "{endpoint}: {name}"
            );
        }
        for name in ["..%2Floki.db", "%2Fetc%2Fpasswd", "C%3Asecret", "a%5Cb"] {
            assert_eq!(
                file_api(
                    &app,
                    token,
                    "GET",
                    &format!("{prefix}/{endpoint}?path={name}"),
                    None,
                    &[]
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
    }
    let (status, _, bytes) = file_api(
        &app,
        token,
        "GET",
        &format!("{prefix}/file?path=empty"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.is_empty());
    assert_eq!(
        file_api(
            &app,
            token,
            "GET",
            &format!("{prefix}/file?path=empty"),
            None,
            &[("range", "bytes=0-")]
        )
        .await
        .0,
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    // A valid filesystem timestamp outside HTTP's range must not panic.
    std::fs::File::options()
        .write(true)
        .open(root.join("empty"))
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1))
        .unwrap();
    let (status, headers, _) = file_api(
        &app,
        token,
        "HEAD",
        &format!("{prefix}/file?path=empty"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::LAST_MODIFIED));
    let (status, _, bytes) = file_api(
        &app,
        token,
        "GET",
        &format!("{prefix}/metadata?path=empty&checksum=true"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let meta: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(meta["last_modified"].is_null());
    assert_eq!(meta["sha256"], hex::encode(Sha256::digest(b"")));
    assert_eq!(
        file_api(&app, token, "PUT", &uri, None, &[]).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(std::fs::read(root.join("hidden.TMP")).unwrap(), b"secret");
    state
        .0
        .maintenance
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        file_api(&app, token, "GET", &uri, None, &[]).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    state.0.db.close().await;
}

#[tokio::test]
async fn native_device_credentials_are_shared_and_permissions_are_live() {
    let (tmp, state, mut owner) = installation().await;
    // Approved account with no Save sync grant.
    sqlx::query("INSERT INTO users(id,username,display_name,password,status,created) SELECT 'reader','reader','Reader',password,'approved',created FROM users WHERE owner=1")
        .execute(&state.0.db).await.unwrap();
    sqlx::query("INSERT INTO memberships(user_id,group_id) VALUES('reader','members')")
        .execute(&state.0.db)
        .await
        .unwrap();
    let root = tmp.path().join("shared");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("readme"), b"contents").unwrap();
    let share = owner
        .action("share-save", "", json!({"name":"Archive","root":root}))
        .await;
    let id = share["id"].as_str().unwrap();
    let app = web::router(state.clone());
    let device = download_device(&app, "reader").await;
    let token = device["token"].as_str().unwrap();
    let prefix = format!("/files/v1/shares/{id}");
    assert_eq!(
        file_api(&app, token, "GET", "/sync/v1/me", None, &[])
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        file_api(&app, token, "GET", &format!("/api/shares/{id}"), None, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    for endpoint in [
        prefix.clone(),
        format!("{prefix}/entries"),
        format!("{prefix}/metadata?path=readme"),
        format!("{prefix}/file?path=readme"),
    ] {
        assert_eq!(
            file_api(&app, token, "GET", &endpoint, None, &[]).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let (_, _, bytes) = file_api(&app, token, "GET", "/files/v1/shares", None, &[]).await;
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["shares"],
        json!([])
    );
    for access in [
        json!({"visibility":"members","grants":[]}),
        json!({"visibility":"restricted","grants":[{"subject_kind":"user","subject":"reader","role":"access"}]}),
        json!({"visibility":"restricted","grants":[{"subject_kind":"group","subject":"members","role":"access"}]}),
        json!({"visibility":"public","grants":[]}),
    ] {
        owner.action("resource-access", id, access).await;
        assert_eq!(
            file_api(
                &app,
                token,
                "GET",
                &format!("{prefix}/file?path=readme"),
                None,
                &[]
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    // Even a public share requires a device token on the native API.
    assert_eq!(
        file_api(
            &app,
            "",
            "GET",
            &format!("{prefix}/file?path=readme"),
            None,
            &[("cookie", &owner.cookie)]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let sync = file_api(
        &app,
        "",
        "POST",
        "/sync/v1/login",
        Some(json!({"username":"owner","password":"owner-password-123","name":"Sync"})),
        &[],
    )
    .await;
    assert_eq!(sync.0, StatusCode::OK);
    let sync: Value = serde_json::from_slice(&sync.2).unwrap();
    assert_eq!(
        file_api(
            &app,
            sync["token"].as_str().unwrap(),
            "GET",
            "/files/v1/me",
            None,
            &[]
        )
        .await
        .0,
        StatusCode::OK
    );
    owner
        .action(
            "resource-access",
            id,
            json!({"visibility":"restricted","grants":[]}),
        )
        .await;
    assert_eq!(
        file_api(
            &app,
            token,
            "GET",
            &format!("{prefix}/file?path=readme"),
            None,
            &[]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    owner
        .action("user-status", "reader", json!({"status":"suspended"}))
        .await;
    assert_eq!(
        file_api(&app, token, "GET", "/files/v1/me", None, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    state.0.db.close().await;
}

#[tokio::test]
async fn download_devices_can_be_revoked_without_cross_account_access() {
    let (_tmp, state, mut owner) = installation().await;
    let app = web::router(state.clone());
    let device = download_device(&app, "owner").await;
    let token = device["token"].as_str().unwrap();
    let id = device["device"]["id"].as_str().unwrap();
    let stored: String = sqlx::query_scalar("SELECT token FROM devices WHERE id=?")
        .bind(id)
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    assert_eq!(stored, auth::digest(token));
    let (_, _, bytes) = file_api(&app, token, "GET", "/files/v1/devices", None, &[]).await;
    let devices: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(devices["devices"][0]["id"], id);
    assert!(devices["devices"][0].get("token").is_none());
    sqlx::query("INSERT INTO users(id,username,display_name,password,status,created) SELECT 'other','other','Other',password,'approved',created FROM users WHERE owner=1")
        .execute(&state.0.db).await.unwrap();
    let other = download_device(&app, "other").await;
    let other_token = other["token"].as_str().unwrap();
    file_api(
        &app,
        other_token,
        "DELETE",
        &format!("/files/v1/devices/{id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(
        file_api(&app, token, "GET", "/files/v1/me", None, &[])
            .await
            .0,
        StatusCode::OK
    );
    // Browser management requires a session and CSRF protection on deletion.
    let (_, devices) = owner.request("GET", "/api/file-devices", None).await;
    assert_eq!(devices["devices"][0]["id"], id);
    assert_eq!(
        file_api(
            &app,
            "",
            "DELETE",
            &format!("/api/file-devices/{id}"),
            None,
            &[("cookie", &owner.cookie)]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        file_api(
            &app,
            "",
            "DELETE",
            &format!("/api/file-devices/{id}"),
            None,
            &[("cookie", &owner.cookie), ("x-csrf-token", &owner.csrf)]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        file_api(&app, token, "GET", "/files/v1/me", None, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        file_api(&app, other_token, "DELETE", "/files/v1/device", None, &[])
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        file_api(&app, other_token, "GET", "/files/v1/me", None, &[])
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let device = download_device(&app, "owner").await;
    owner
        .action(
            "password",
            "",
            json!({"current_password":"owner-password-123","password":"updated-password-123"}),
        )
        .await;
    assert_eq!(
        file_api(
            &app,
            device["token"].as_str().unwrap(),
            "GET",
            "/files/v1/me",
            None,
            &[]
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM devices")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    state.0.db.close().await;
}

#[tokio::test]
async fn download_login_requires_mfa_and_shares_password_guess_limits() {
    let (_tmp, state, _owner) = installation().await;
    let app = web::router(state.clone());
    let secret = "JBSWY3DPEHPK3PXP";
    sqlx::query("UPDATE users SET totp=? WHERE owner=1")
        .bind(secret)
        .execute(&state.0.db)
        .await
        .unwrap();
    let login = json!({"username":"owner","password":"owner-password-123","name":"Downloader"});
    let (status, _, bytes) = file_api(
        &app,
        "",
        "POST",
        "/files/v1/login",
        Some(login.clone()),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["code"],
        "invalid_credentials"
    );
    let mut input = login.clone();
    input["code"] = json!(auth::totp(secret, auth::now() / 30).unwrap());
    let (status, _, bytes) = file_api(&app, "", "POST", "/files/v1/login", Some(input), &[]).await;
    assert_eq!(status, StatusCode::OK, "{bytes:?}");
    // Browser login during setup and both file logins consumed the same limit.
    for _ in 0..5 {
        file_api(&app, "", "POST", "/sync/v1/login", Some(login.clone()), &[]).await;
    }
    assert_eq!(
        file_api(&app, "", "POST", "/files/v1/login", Some(login), &[])
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    state.0.db.close().await;
}

#[tokio::test]
async fn owner_is_unique_and_csrf_is_required() {
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path().into(), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let token = state.0.setup_token.lock().await.clone().unwrap();
    let app = web::router(state.clone());
    let mut first = Browser::new(app.clone());
    let mut second = Browser::new(app);
    first.session().await;
    second.session().await;
    let input = json!({"token":token,"username":"owner","password":"owner-password-123","name":"LOKI","public_url":"http://127.0.0.1:7070"});
    let (a, b) = tokio::join!(
        first.request("POST", "/api/setup", Some(input.clone())),
        second.request("POST", "/api/setup", Some(input))
    );
    assert!(matches!(
        (a.0, b.0),
        (StatusCode::OK, StatusCode::CONFLICT) | (StatusCode::CONFLICT, StatusCode::OK)
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE owner=1")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    assert_eq!(count, 1);
    first.csrf = "wrong".into();
    assert_eq!(
        first
            .request(
                "POST",
                "/api/login",
                Some(json!({"username":"owner","password":"owner-password-123"}))
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    state.0.db.close().await;
}
#[tokio::test]
async fn setup_errors_are_actionable_and_invalid_storage_does_not_consume_setup() {
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path().join("home"), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let mut browser = Browser::new(web::router(state.clone()));
    browser.session().await;
    let token = state.0.setup_token.lock().await.clone().unwrap();
    let occupied = tmp.path().join("occupied");
    std::fs::create_dir(&occupied).unwrap();
    std::fs::write(occupied.join("keep.txt"), b"preserve me").unwrap();
    let mut input = json!({"token":"invalid","username":"owner","password":"owner-password-123","name":"LOKI","public_url":"https://loki.example.com","data_dir":occupied.to_string_lossy()});
    let (status, error) = browser
        .request("POST", "/api/setup", Some(input.clone()))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(error["error"].as_str().unwrap().contains("setup token"));
    input["token"] = json!(format!(" {token}\r\n"));
    let (status, error) = browser
        .request("POST", "/api/setup", Some(input.clone()))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["error"].as_str().unwrap().contains("empty folder"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
            .fetch_one(&state.0.db)
            .await
            .unwrap(),
        0
    );
    assert!(state.0.setup_token.lock().await.is_some());
    assert_eq!(
        std::fs::read(occupied.join("keep.txt")).unwrap(),
        b"preserve me"
    );
    input["data_dir"] = json!("");
    browser.post("/api/setup", input.clone()).await;
    let (status, error) = browser.request("POST", "/api/setup", Some(input)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(error["error"].as_str().unwrap().contains("already set up"));
    assert_eq!(browser.session().await["setup"], false);
    state.0.db.close().await;
}

#[tokio::test]
async fn registration_grants_and_revocation() {
    let (_tmp, state, mut owner) = installation().await;
    let mut member = Browser::new(web::router(state.clone()));
    member.session().await;
    member
        .post(
            "/api/register",
            json!({"username":"member","password":"member-password-123"}),
        )
        .await;
    member
        .post(
            "/api/login",
            json!({"username":"member","password":"member-password-123"}),
        )
        .await;
    let info = member.session().await;
    assert_eq!(info["user"]["status"], "pending");
    let uid = info["user"]["id"].as_str().unwrap();
    owner
        .action("user-status", uid, json!({"status":"approved"}))
        .await;
    assert!(member.session().await["user"].is_null());
    member
        .post(
            "/api/login",
            json!({"username":"member","password":"member-password-123"}),
        )
        .await;
    member.session().await;
    owner
        .action("group-create", "", json!({"name":"Friends"}))
        .await;
    let group: String = sqlx::query_scalar("SELECT id FROM groups WHERE name='Friends'")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    owner
        .action("membership", uid, json!({"group":group,"enabled":true}))
        .await;
    let resource = "test-app";
    sqlx::query("INSERT INTO resources(id,kind,name) VALUES(?,'app','Testing')")
        .bind(resource)
        .execute(&state.0.db)
        .await
        .unwrap();
    owner.action("resource-access",resource,json!({"visibility":"restricted","grants":[{"subject_kind":"group","subject":group,"role":"access"}]})).await;
    let user: auth::User = sqlx::query_as("SELECT * FROM users WHERE id=?")
        .bind(uid)
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    assert!(
        auth::allowed(&state, Some(&user), resource, "access")
            .await
            .unwrap()
    );
    assert!(
        !auth::allowed(&state, Some(&user), resource, "admin")
            .await
            .unwrap()
    );
    let reset = owner.action("reset-issue", uid, json!({})).await;
    let token = reset["url"].as_str().unwrap().split('#').nth(1).unwrap();
    let mut anon = Browser::new(web::router(state.clone()));
    anon.session().await;
    anon.post(
        "/api/reset",
        json!({"token":token,"password":"replacement-password"}),
    )
    .await;
    assert_eq!(
        anon.request(
            "POST",
            "/api/reset",
            Some(json!({"token":token,"password":"replacement-password"}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert!(member.session().await["user"].is_null());
    let mut settings = state.0.settings.read().await.clone();
    let mut unknown = json!(settings);
    unknown["registration"] = json!("invite");
    assert_eq!(
        owner
            .request(
                "POST",
                "/api/action",
                Some(json!({"action":"settings","data":unknown}))
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    settings.registration = loki_server::Registration::Closed;
    owner.action("settings", "", json!(settings)).await;
    assert_eq!(
        anon.request(
            "POST",
            "/api/register",
            Some(json!({"username":"closed","password":"closed-password-123"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    settings.registration = loki_server::Registration::Open;
    owner.action("settings", "", json!(settings)).await;
    let created = anon
        .post(
            "/api/register",
            json!({"username":"openuser","password":"open-password-123"}),
        )
        .await;
    assert_eq!(created["status"], "approved");
    state.0.db.close().await;
}
#[tokio::test]
async fn share_exclusions_filter_before_pagination_and_block_direct_file_requests() {
    let (tmp, state, mut owner) = installation().await;
    let folder = tmp.path().join("shared");
    std::fs::create_dir_all(folder.join("folder.tmp")).unwrap();
    for n in 0..105 {
        std::fs::write(folder.join(format!("visible-{n:03}.txt")), b"hello world").unwrap();
        std::fs::write(folder.join(format!("hidden-{n:03}.tmp")), b"hidden").unwrap();
    }
    for name in [
        "photo.JPG",
        "archive.TAR.GZ",
        ".tmp",
        "notes.tmp.txt",
        "folder.tmp/日本語.txt",
        "folder.tmp/hidden.BAK",
    ] {
        std::fs::write(folder.join(name), b"contents").unwrap();
    }
    let share = owner.action("share-save", "", json!({"name":"Filtered","root":folder,"excluded_extensions":".TMP, *.JPG; .tar.gz bak"})).await;
    let id = share["id"].as_str().unwrap();
    let listing_url = format!("/api/shares/{id}");
    let mut guest = Browser::new(web::router(state.clone()));
    assert_eq!(
        guest.request("GET", &listing_url, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    owner
        .action(
            "resource-access",
            id,
            json!({"visibility":"public","grants":[]}),
        )
        .await;
    let (status, listing) = guest.request("GET", &listing_url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listing["total"], 107);
    assert_eq!(listing["entries"].as_array().unwrap().len(), 100);
    assert_eq!(listing["entries"][0]["name"], "folder.tmp");
    let (_, next) = guest
        .request("GET", &format!("{listing_url}?page=1&sort=size"), None)
        .await;
    assert_eq!(next["total"], 107);
    assert_eq!(next["entries"].as_array().unwrap().len(), 7);
    let (_, nested) = guest
        .request("GET", &format!("{listing_url}?path=folder.tmp"), None)
        .await;
    assert_eq!(nested["total"], 1);
    assert_eq!(nested["entries"][0]["name"], "日本語.txt");
    for path in [
        "photo.JPG",
        "photo.%4A%50%47",
        "archive.TAR.GZ",
        ".tmp",
        "folder.tmp%2Fhidden.BAK",
    ] {
        for suffix in ["", "&download=true"] {
            let url = format!("{listing_url}/file?path={path}{suffix}");
            assert_eq!(
                guest.request("GET", &url, None).await.0,
                StatusCode::NOT_FOUND,
                "{url}"
            );
            assert_eq!(
                owner.request("GET", &url, None).await.0,
                StatusCode::NOT_FOUND,
                "{url}"
            );
        }
    }
    let blocked = guest
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("{listing_url}/file?path=photo.JPG"))
                .header(header::RANGE, "bytes=0-2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::NOT_FOUND);
    let allowed = guest
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{listing_url}/file?path=visible-000.txt"))
                .header(header::RANGE, "bytes=0-4")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        allowed
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"hello"
    );
    let (_, snapshot) = owner.request("GET", "/api/state", None).await;
    let saved = snapshot["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap();
    assert_eq!(
        saved["excluded_extensions"],
        json!(["bak", "jpg", "tar.gz", "tmp"])
    );
    let invalid = owner.request("POST", "/api/action", Some(json!({"action":"share-save","id":id,"data":{"name":"Invalid edit","root":folder,"excluded_extensions":"../txt"}}))).await;
    assert_eq!(invalid.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        guest
            .request("GET", &format!("{listing_url}/file?path=photo.JPG"), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // Other shares of the same folder have independent settings; clearing the
    // filter takes effect on the next request and never deletes the file.
    let other = owner
        .action("share-save", "", json!({"name":"Unfiltered","root":folder}))
        .await;
    assert_eq!(
        owner
            .request(
                "GET",
                &format!(
                    "/api/shares/{}/file?path=photo.JPG",
                    other["id"].as_str().unwrap()
                ),
                None
            )
            .await
            .0,
        StatusCode::OK
    );
    owner
        .action(
            "share-save",
            id,
            json!({"name":"Filtered","root":folder,"excluded_extensions":""}),
        )
        .await;
    assert_eq!(
        guest
            .request("GET", &format!("{listing_url}/file?path=photo.JPG"), None)
            .await
            .0,
        StatusCode::OK
    );
    assert!(folder.join("photo.JPG").is_file());
    state.0.db.close().await;
}

#[tokio::test]
async fn file_permissions_ranges_and_active_content() {
    let (tmp, state, mut owner) = installation().await;
    let folder = tmp.path().join("shared");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("日本語.txt"), b"0123456789").unwrap();
    std::fs::write(folder.join("evil.svg"), b"<svg onload='alert(1)'/>").unwrap();
    let share = owner
        .action("share-save", "", json!({"name":"Archive","root":folder}))
        .await;
    let id = share["id"].as_str().unwrap();
    let mut guest = Browser::new(web::router(state.clone()));
    guest.session().await;
    assert_eq!(
        guest
            .request("GET", &format!("/api/shares/{id}"), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    owner
        .action(
            "resource-access",
            id,
            json!({"visibility":"public","grants":[]}),
        )
        .await;
    assert_eq!(
        guest
            .request("GET", &format!("/api/shares/{id}"), None)
            .await
            .1["total"],
        2
    );
    let uri = format!(
        "/api/shares/{id}/file?path={}",
        url::form_urlencoded::byte_serialize("日本語.txt".as_bytes()).collect::<String>()
    );
    let response = web::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::RANGE, "bytes=2-5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(
        &response.into_body().collect().await.unwrap().to_bytes()[..],
        b"2345"
    );
    let response = web::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/api/shares/{id}/file?path=evil.svg"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    assert_eq!(
        guest
            .request(
                "GET",
                &format!("/api/shares/{id}/file?path=..%2Floki.db"),
                None
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    owner
        .action(
            "resource-access",
            id,
            json!({"visibility":"restricted","grants":[]}),
        )
        .await;
    assert_eq!(
        guest
            .request("GET", &format!("/api/shares/{id}/file?path=evil.svg"), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    state.0.db.close().await;
}
#[tokio::test]
async fn oidc_pkce_replay_and_refresh_reuse() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    let (_tmp, state, owner) = installation().await;
    sqlx::query("INSERT INTO resources(id,kind,name) VALUES('client','app','Client')")
        .execute(&state.0.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO clients(id,resource,secret,redirect) VALUES('client','client',?,'http://127.0.0.1:9001/auth/callback')").bind(auth::digest("secret")).execute(&state.0.db).await.unwrap();
    let verifier = auth::random();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse("http://local/oidc/authorize").unwrap();
    url.query_pairs_mut().extend_pairs([
        ("client_id", "client"),
        ("redirect_uri", "http://127.0.0.1:9001/auth/callback"),
        ("response_type", "code"),
        ("scope", "openid profile"),
        ("state", "state"),
        ("nonce", "nonce"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    let response = owner
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{}?{}", url.path(), url.query().unwrap()))
                .header(header::COOKIE, &owner.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let callback = url::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let code = callback
        .query_pairs()
        .find(|(k, _)| k == "code")
        .unwrap()
        .1
        .into_owned();
    async fn form(app: Router, endpoint: &str, pairs: &[(&str, &str)]) -> (StatusCode, Value) {
        let mut data = url::form_urlencoded::Serializer::new(String::new());
        data.extend_pairs(pairs.iter().copied());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(endpoint)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(data.finish()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    let pairs = [
        ("grant_type", "authorization_code"),
        ("client_id", "client"),
        ("client_secret", "secret"),
        ("code", code.as_str()),
        ("redirect_uri", "http://127.0.0.1:9001/auth/callback"),
        ("code_verifier", verifier.as_str()),
    ];
    let mut wrong = pairs;
    wrong[5].1 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    assert_eq!(
        form(owner.app.clone(), "/oidc/token", &wrong).await.0,
        StatusCode::BAD_REQUEST
    );
    let (status, tokens) = form(owner.app.clone(), "/oidc/token", &pairs).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(
        form(owner.app.clone(), "/oidc/token", &pairs).await.0,
        StatusCode::BAD_REQUEST
    );
    let refresh = tokens["refresh_token"].as_str().unwrap();
    let input = [
        ("grant_type", "refresh_token"),
        ("client_id", "client"),
        ("client_secret", "secret"),
        ("refresh_token", refresh),
    ];
    let (status, new) = form(owner.app.clone(), "/oidc/token", &input).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        form(owner.app.clone(), "/oidc/token", &input).await.0,
        StatusCode::BAD_REQUEST
    );
    let access = new["access_token"].as_str().unwrap();
    let (_, info) = form(
        owner.app.clone(),
        "/oidc/introspect",
        &[
            ("client_id", "client"),
            ("client_secret", "secret"),
            ("token", access),
        ],
    )
    .await;
    assert_eq!(info["active"], false);
    // Two overlapping uses of one refresh token must leave no active family,
    // even when the first exchange was still minting its replacement.
    let response = owner
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{}?{}", url.path(), url.query().unwrap()))
                .header(header::COOKIE, &owner.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let callback = url::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let fresh_code = callback
        .query_pairs()
        .find(|(k, _)| k == "code")
        .unwrap()
        .1
        .into_owned();
    let mut fresh_pairs = pairs;
    fresh_pairs[3].1 = &fresh_code;
    let (status, fresh) = form(owner.app.clone(), "/oidc/token", &fresh_pairs).await;
    assert_eq!(status, StatusCode::OK);
    let refresh = fresh["refresh_token"].as_str().unwrap();
    let duplicate = [
        ("grant_type", "refresh_token"),
        ("client_id", "client"),
        ("client_secret", "secret"),
        ("refresh_token", refresh),
    ];
    let (a, b) = tokio::join!(
        form(owner.app.clone(), "/oidc/token", &duplicate),
        form(owner.app.clone(), "/oidc/token", &duplicate)
    );
    assert!(a.0 == StatusCode::BAD_REQUEST || b.0 == StatusCode::BAD_REQUEST);
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tokens t JOIN token_families f ON f.id=t.family WHERE f.revoked=0",
    )
    .fetch_one(&state.0.db)
    .await
    .unwrap();
    assert_eq!(active, 0);
    state.0.db.close().await;
}
#[test]
fn malformed_packages_do_not_escape() {
    use std::io::Write;
    let tmp = tempfile::tempdir().unwrap();
    let package = tmp.path().join("bad.loki-app");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&package).unwrap());
    let options = zip::write::SimpleFileOptions::default();
    let manifest = json!({"protocol":1,"id":"test","name":"Test","version":"1.0.0","runtime":"wasi","target":"wasm32-wasip2","entry":"app.wasm","health":"/health","files":{"app.wasm":"a".repeat(64)},"built_with":{"sdk":env!("CARGO_PKG_VERSION"),"cargo_loki":env!("CARGO_PKG_VERSION")}});
    zip.start_file("manifest.json", options).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    zip.start_file("../escape", options).unwrap();
    zip.write_all(b"bad").unwrap();
    zip.finish().unwrap();
    let error = loki_server::apps::extract(&package, &tmp.path().join("out")).unwrap_err();
    assert!(
        error.message.contains("safe relative paths"),
        "{}",
        error.message
    );
    assert!(!tmp.path().join("escape").exists());
}
#[tokio::test]
async fn credential_epoch_rejects_inflight_old_password_proof() {
    let (_tmp, state, mut owner) = installation().await;
    let uid = owner.session().await["user"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let version: i64 = sqlx::query_scalar("SELECT auth_version FROM users WHERE id=?")
        .bind(&uid)
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    auth::revoke_user(&state, &uid).await.unwrap();
    assert!(
        auth::new_session(&state, Some((&uid, version)), "stale sign-in")
            .await
            .is_err()
    );
    assert!(owner.session().await["user"].is_null());
    state.0.db.close().await;
}

#[tokio::test]
async fn totp_setup_recovery_codes_and_replay_protection() {
    let (_tmp, state, mut owner) = installation().await;
    let setup = owner
        .action(
            "mfa-start",
            "",
            json!({"current_password":"owner-password-123"}),
        )
        .await;
    let secret = setup["secret"].as_str().unwrap();
    let code = auth::totp(secret, auth::now() / 30).unwrap();
    let enabled = owner.action("mfa-enable", "", json!({"code":code})).await;
    let recovery = enabled["recovery_codes"][0].as_str().unwrap();
    owner.post("/api/logout", json!({})).await;
    owner.session().await;
    let data = json!({"username":"owner","password":"owner-password-123","code":code});
    assert_eq!(
        owner.request("POST", "/api/login", Some(data)).await.0,
        StatusCode::UNAUTHORIZED
    );
    owner
        .post(
            "/api/login",
            json!({"username":"owner","password":"owner-password-123","code":recovery}),
        )
        .await;
    owner.session().await;
    owner.post("/api/logout", json!({})).await;
    owner.session().await;
    assert_eq!(
        owner
            .request(
                "POST",
                "/api/login",
                Some(json!({"username":"owner","password":"owner-password-123","code":recovery}))
            )
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    state.0.db.close().await;
}

#[tokio::test]
async fn members_can_write_a_bio_within_the_limit() {
    let (_tmp, _state, mut owner) = installation().await;
    owner
        .action(
            "bio",
            "",
            json!({"bio":"  Plays Pokémon Emerald.\r\nCollects saves.  "}),
        )
        .await;
    let info = owner.session().await;
    assert_eq!(
        info["user"]["bio"],
        "Plays Pokémon Emerald.\nCollects saves."
    );
    let (status, _) = owner
        .request(
            "POST",
            "/api/action",
            Some(json!({"action":"bio","id":"","data":{"bio":"枝".repeat(301)}})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    owner
        .action("bio", "", json!({"bio":"枝".repeat(300)}))
        .await;
    owner.action("bio", "", json!({"bio":""})).await;
    assert_eq!(owner.session().await["user"]["bio"], "");
}

#[tokio::test]
async fn dashboard_actions_read_their_fields() {
    let (tmp, state, mut owner) = installation().await;
    let rejected = |status: StatusCode| assert_eq!(status, StatusCode::BAD_REQUEST);
    let send = |name: &str, id: &str, data: Value| json!({"action":name,"id":id,"data":data});

    owner
        .action(
            "profile",
            "",
            json!({"current_password":"owner-password-123","username":"owner","display_name":"Odin","email":"Odin@Example.com"}),
        )
        .await;
    let session = owner.session().await;
    assert_eq!(session["user"]["display_name"], "Odin");
    assert_eq!(session["user"]["email"], "odin@example.com");
    owner.action("session-revoke", "unknown", json!({})).await;

    let (status, error) = owner
        .request(
            "POST",
            "/api/action",
            Some(send("user-create", "", json!({}))),
        )
        .await;
    rejected(status);
    assert!(
        error["error"].as_str().unwrap().contains("username"),
        "{error}"
    );
    let created = owner
        .action("user-create", "", json!({"username":"freya"}))
        .await;
    assert!(created["url"].as_str().unwrap().contains("/reset#"));

    owner
        .action("group-create", "", json!({"name":"Valkyries"}))
        .await;
    let group: String = sqlx::query_scalar("SELECT id FROM groups WHERE name='Valkyries'")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    owner.action("group-delete", &group, json!({})).await;

    let folder = tmp.path().join("shared");
    std::fs::create_dir(&folder).unwrap();
    let share = owner
        .action(
            "share-save",
            "",
            json!({"root":folder.to_string_lossy(),"name":"Saves","excluded_extensions":".tmp"}),
        )
        .await;
    let share = share["id"].as_str().unwrap().to_string();
    owner.action("resource-delete", &share, json!({})).await;

    let route = owner
        .action(
            "route-save",
            "",
            json!({"name":"Media","hostname":"Media.Example.com","upstream":"http://127.0.0.1:8096","gated":false,"service":"","service_kind":"none"}),
        )
        .await;
    let route = route["id"].as_str().unwrap().to_string();
    let host: String = sqlx::query_scalar("SELECT hostname FROM routes WHERE id=?")
        .bind(&route)
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    assert_eq!(host, "media.example.com");
    rejected(
        owner
            .request(
                "POST",
                "/api/action",
                Some(send("route-action", &route, json!({"operation":"explode"}))),
            )
            .await
            .0,
    );
    owner
        .action("route-action", &route, json!({"operation":"delete"}))
        .await;

    let saved = owner
        .action(
            "sync-folder-save",
            "",
            json!({"name":"GBA saves","kind":"retroarch-saves"}),
        )
        .await;
    let sync = saved["id"].as_str().unwrap().to_string();
    owner
        .action("sync-folder-save", &sync, json!({"keep_versions":3}))
        .await;
    rejected(
        owner
            .request(
                "POST",
                "/api/action",
                Some(send(
                    "sync-folder-delete",
                    &sync,
                    json!({"confirm":"wrong"}),
                )),
            )
            .await
            .0,
    );
    owner
        .action("sync-folder-delete", &sync, json!({"confirm":"GBA saves"}))
        .await;

    rejected(
        owner
            .request(
                "POST",
                "/api/action",
                Some(send("device-lookup", "", json!({"code":"BCDF-GHJK"}))),
            )
            .await
            .0,
    );
    owner
        .action(
            "mfa-disable",
            "",
            json!({"current_password":"owner-password-123"}),
        )
        .await;
    owner.action("revoke-all", "", json!({})).await;
    assert!(owner.session().await["user"].is_null());
    state.0.db.close().await;
}
