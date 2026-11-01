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
