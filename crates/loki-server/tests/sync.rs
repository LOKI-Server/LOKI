use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use loki_server::{State, web};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct Browser {
    app: Router,
    cookie: String,
    csrf: String,
}
impl Browser {
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
        let body = match body {
            Some(value) => {
                request = request
                    .header("x-csrf-token", &self.csrf)
                    .header(header::CONTENT_TYPE, "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn session(&mut self) -> Value {
        let (status, value) = self.request("GET", "/api/session", None).await;
        assert_eq!(status, StatusCode::OK);
        self.csrf = value["csrf"].as_str().unwrap().into();
        value
    }
    async fn post(&mut self, path: &str, value: Value) -> Value {
        let (status, value) = self.request("POST", path, Some(value)).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        value
    }
    async fn action(&mut self, name: &str, id: &str, data: Value) -> Value {
        self.post("/api/action", json!({"action":name,"id":id,"data":data}))
            .await
    }
}

/// A sync client, speaking only the bearer-token API.
#[derive(Clone)]
struct Device {
    app: Router,
    token: String,
}
async fn send(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Body,
    json: bool,
) -> (StatusCode, bytes::Bytes) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "127.0.0.1:7070");
    if !token.is_empty() {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if json {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response.into_body().collect().await.unwrap().to_bytes(),
    )
}
impl Device {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, bytes) = match body {
            Some(value) => {
                send(
                    &self.app,
                    &self.token,
                    method,
                    path,
                    Body::from(value.to_string()),
                    true,
                )
                .await
            }
            None => send(&self.app, &self.token, method, path, Body::empty(), false).await,
        };
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.call(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }
    async fn upload(&self, bytes: &[u8]) -> String {
        let hash = hex::encode(Sha256::digest(bytes));
        let (status, body) = send(
            &self.app,
            &self.token,
            "PUT",
            &format!("/sync/v1/blobs/{hash}"),
            Body::from(bytes.to_vec()),
            false,
        )
        .await;
        assert!(status.is_success(), "upload: {status} {body:?}");
        hash
    }
    async fn commit(&self, folder: &str, changes: Value, on_conflict: &str) -> Vec<Value> {
        self.ok(
            "POST",
            &format!("/sync/v1/folders/{folder}/commit"),
            Some(json!({"changes":changes,"on_conflict":on_conflict})),
        )
        .await["results"]
            .as_array()
            .unwrap()
            .clone()
    }
    async fn changes(&self, folder: &str, since: i64) -> Value {
        self.ok(
            "GET",
            &format!("/sync/v1/folders/{folder}/changes?since={since}"),
            None,
        )
        .await
    }
}

async fn installation() -> (tempfile::TempDir, State, Browser) {
    let tmp = tempfile::tempdir().unwrap();
    let state = State::open(tmp.path().join("data"), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let mut browser = Browser {
        app: web::router(state.clone()),
        cookie: String::new(),
        csrf: String::new(),
    };
    browser.session().await;
    let token = state.0.setup_token.lock().await.clone().unwrap();
    browser.post("/api/setup", json!({"token":token,"username":"owner","password":"owner-password-123","name":"Test universe","public_url":"http://127.0.0.1:7070"})).await;
    browser
        .post(
            "/api/login",
            json!({"username":"owner","password":"owner-password-123"}),
        )
        .await;
    browser.session().await;
    (tmp, state, browser)
}
async fn login_device(state: &State, username: &str, password: &str, name: &str) -> Device {
    let app = web::router(state.clone());
    let (status, bytes) = send(
        &app,
        "",
        "POST",
        "/sync/v1/login",
        Body::from(
            json!({"username":username,"password":password,"name":name,"platform":"test"})
                .to_string(),
        ),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bytes:?}");
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    Device {
        app,
        token: value["token"].as_str().unwrap().into(),
    }
}
/// Registers, approves, and grants sync to a second person.
async fn member(state: &State, owner: &mut Browser, username: &str) -> (Browser, String) {
    let mut member = Browser {
        app: web::router(state.clone()),
        cookie: String::new(),
        csrf: String::new(),
    };
    member.session().await;
    member
        .post(
            "/api/register",
            json!({"username":username,"password":"member-password-123"}),
        )
        .await;
    member
        .post(
            "/api/login",
            json!({"username":username,"password":"member-password-123"}),
        )
        .await;
    let uid = member.session().await["user"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    owner
        .action("user-status", &uid, json!({"status":"approved"}))
        .await;
    owner.action("resource-access", "sync", json!({"visibility":"restricted","grants":[{"subject_kind":"user","subject":uid,"role":"access"}]})).await;
    // Approval ends existing sessions, so sign in again.
    member.session().await;
    member
        .post(
            "/api/login",
            json!({"username":username,"password":"member-password-123"}),
        )
        .await;
    member.session().await;
    (member, uid)
}

#[tokio::test]
async fn pairing_with_a_code_issues_a_single_device_token() {
    let (_tmp, state, mut owner) = installation().await;
    let device = Device {
        app: web::router(state.clone()),
        token: String::new(),
    };
    let pairing = device
        .ok(
            "POST",
            "/sync/v1/pair",
            Some(json!({"name":"AYN Thor","platform":"android"})),
        )
        .await;
    let code = pairing["user_code"].as_str().unwrap();
    assert!(
        pairing["verification_uri_complete"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/pair?code={code}"))
    );
    let poll = json!({"device_code":pairing["device_code"]});
    let (status, value) = device
        .call("POST", "/sync/v1/pair/poll", Some(poll.clone()))
        .await;
    assert_eq!(
        (status, value["code"].as_str()),
        (
            StatusCode::PRECONDITION_REQUIRED,
            Some("authorization_pending")
        )
    );
    let (status, value) = device
        .call("POST", "/sync/v1/pair/poll", Some(poll.clone()))
        .await;
    assert_eq!(
        (status, value["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("slow_down"))
    );
    let shown = owner
        .action(
            "sync-pair-lookup",
            "",
            json!({"code":code.to_lowercase().replace('-', " ")}),
        )
        .await;
    assert_eq!(shown["name"], "AYN Thor");
    owner
        .action("sync-pair-approve", "", json!({"code":code}))
        .await;
    sqlx::query("UPDATE sync_pairings SET polled=0")
        .execute(&state.0.db)
        .await
        .unwrap();
    let issued = device
        .ok("POST", "/sync/v1/pair/poll", Some(poll.clone()))
        .await;
    assert_eq!(issued["user"]["username"], "owner");
    let (status, value) = device.call("POST", "/sync/v1/pair/poll", Some(poll)).await;
    assert_eq!(
        (status, value["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("expired_token"))
    );
    let paired = Device {
        token: issued["token"].as_str().unwrap().into(),
        ..device.clone()
    };
    let me = paired.ok("GET", "/sync/v1/me", None).await;
    assert_eq!(me["device"]["name"], "AYN Thor");
    assert_eq!(me["quota"]["used"], 0);
    // Declined codes tell the device instead of leaving it waiting.
    let declined = device
        .ok(
            "POST",
            "/sync/v1/pair",
            Some(json!({"name":"Unknown","platform":""})),
        )
        .await;
    owner
        .action("sync-pair-deny", "", json!({"code":declined["user_code"]}))
        .await;
    let (status, value) = device
        .call(
            "POST",
            "/sync/v1/pair/poll",
            Some(json!({"device_code":declined["device_code"]})),
        )
        .await;
    assert_eq!(
        (status, value["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("access_denied"))
    );
    // Signing out from the dashboard ends the token.
    owner
        .action(
            "sync-device-revoke",
            me["device"]["id"].as_str().unwrap(),
            json!({}),
        )
        .await;
    assert_eq!(
        paired.call("GET", "/sync/v1/me", None).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn two_devices_sync_resolve_conflicts_and_restore_history() {
    let (_tmp, state, mut owner) = installation().await;
    let thor = login_device(&state, "owner", "owner-password-123", "Thor").await;
    let desktop = login_device(&state, "owner", "owner-password-123", "Desktop").await;
    let folder = thor
        .ok(
            "POST",
            "/sync/v1/folders",
            Some(json!({"name":"GBA saves","kind":"retroarch-saves"})),
        )
        .await;
    let id = folder["id"].as_str().unwrap();
    let (status, value) = desktop
        .call(
            "POST",
            "/sync/v1/folders",
            Some(json!({"name":"GBA saves"})),
        )
        .await;
    assert_eq!(
        (status, value["code"].as_str()),
        (StatusCode::CONFLICT, Some("exists"))
    );
    assert_eq!(
        desktop.ok("GET", "/sync/v1/folders", None).await[0]["id"],
        id
    );

    let first = thor.upload(b"emerald, badge 1").await;
    let result = thor.commit(id, json!([{"path":"Pokemon Emerald.srm","base_version":0,"hash":first,"size":16,"mtime":100}]), "reject").await;
    assert_eq!(
        (result[0]["status"].as_str(), result[0]["version"].as_i64()),
        (Some("ok"), Some(1))
    );
    // Retrying the same commit is harmless.
    assert_eq!(
        thor.commit(
            id,
            json!([{"path":"Pokemon Emerald.srm","base_version":0,"hash":first}]),
            "reject"
        )
        .await[0]["version"],
        1
    );

    let feed = desktop.changes(id, 0).await;
    assert_eq!(feed["changes"][0]["hash"], first.as_str());
    let seq = feed["seq"].as_i64().unwrap();
    let (status, bytes) = send(
        &desktop.app,
        &desktop.token,
        "GET",
        &format!("/sync/v1/blobs/{first}"),
        Body::empty(),
        false,
    )
    .await;
    assert_eq!(
        (status, bytes.as_ref()),
        (StatusCode::OK, b"emerald, badge 1".as_slice())
    );
    assert!(
        desktop.changes(id, seq).await["changes"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Both devices played offline from version 1.
    let thor_save = thor.upload(b"emerald, badge 2").await;
    let desktop_save = desktop.upload(b"emerald, badge 3").await;
    thor.commit(
        id,
        json!([{"path":"Pokemon Emerald.srm","base_version":1,"hash":thor_save}]),
        "reject",
    )
    .await;
    let rejected = desktop
        .commit(
            id,
            json!([{"path":"Pokemon Emerald.srm","base_version":1,"hash":desktop_save}]),
            "reject",
        )
        .await;
    assert_eq!(rejected[0]["status"], "conflict");
    assert_eq!(rejected[0]["current"]["hash"], thor_save.as_str());
    let stashed = desktop
        .commit(
            id,
            json!([{"path":"Pokemon Emerald.srm","base_version":1,"hash":desktop_save}]),
            "stash",
        )
        .await;
    assert_eq!(stashed[0]["status"], "stashed");
    let overwritten = desktop
        .commit(
            id,
            json!([{"path":"Pokemon Emerald.srm","base_version":1,"hash":desktop_save}]),
            "overwrite",
        )
        .await;
    assert_eq!(
        (
            overwritten[0]["status"].as_str(),
            overwritten[0]["version"].as_i64(),
            overwritten[0]["conflict"].as_bool()
        ),
        (Some("ok"), Some(3), Some(true))
    );
    // The overwritten version is kept as an unresolved conflict, next to the stash.
    let conflicts = owner
        .request(
            "GET",
            &format!("/api/sync/folders/{id}/history?conflicts=true"),
            None,
        )
        .await
        .1;
    let conflicts = conflicts.as_array().unwrap();
    assert_eq!(conflicts.len(), 2);
    assert!(
        conflicts
            .iter()
            .any(|h| h["hash"] == thor_save.as_str() && h["device_name"] == "Thor")
    );
    assert_eq!(
        owner.request("GET", "/api/state", None).await.1["sync"]["folders"][0]["conflicts"],
        2
    );

    // Choosing Thor's version makes it current everywhere as version 4.
    let thor_conflict = conflicts
        .iter()
        .find(|h| h["hash"] == thor_save.as_str())
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    owner
        .action(
            "sync-conflict-resolve",
            id,
            json!({"history_id":thor_conflict,"keep":"this"}),
        )
        .await;
    let feed = thor.changes(id, 0).await;
    assert_eq!(
        (
            feed["changes"][0]["version"].as_i64(),
            feed["changes"][0]["hash"].as_str()
        ),
        (Some(4), Some(thor_save.as_str()))
    );
    let stash = conflicts
        .iter()
        .find(|h| {
            h["reason"] == "conflict" && h["hash"] == desktop_save.as_str() && h["version"] == 1
        })
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    owner
        .action(
            "sync-conflict-resolve",
            id,
            json!({"history_id":stash,"keep":"current"}),
        )
        .await;
    assert!(
        owner
            .request(
                "GET",
                &format!("/api/sync/folders/{id}/history?conflicts=true"),
                None
            )
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty()
    );

    // History restores the very first save, and the browser can download it.
    let history = thor
        .ok(
            "GET",
            &format!("/sync/v1/folders/{id}/history?path=Pokemon%20Emerald.srm"),
            None,
        )
        .await;
    let original = history
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["hash"] == first.as_str())
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let restored = thor
        .ok(
            "POST",
            &format!("/sync/v1/folders/{id}/restore"),
            Some(json!({"history_id":original})),
        )
        .await;
    assert_eq!(restored["version"], 5);
    let (status, _) = owner
        .request(
            "GET",
            &format!("/api/sync/folders/{id}/file?path=Pokemon%20Emerald.srm"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Deletes propagate as tombstones; case and file/folder clashes are refused.
    let other = thor.upload(b"ruby").await;
    let clash = thor.commit(id, json!([{"path":"pokemon emerald.srm","base_version":0,"hash":other},{"path":"Pokemon Emerald.srm/inner","base_version":0,"hash":other},{"path":"gba/Ruby.srm","base_version":0,"hash":other}]), "reject").await;
    assert_eq!(clash[0]["code"], "case_conflict");
    assert_eq!(clash[1]["code"], "path_conflict");
    assert_eq!(clash[2]["status"], "ok");
    let deleted = thor
        .commit(
            id,
            json!([{"path":"Pokemon Emerald.srm","base_version":5,"hash":null}]),
            "reject",
        )
        .await;
    assert_eq!(deleted[0]["version"], 6);
    let tail = desktop.changes(id, seq).await;
    assert!(
        tail["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["path"] == "Pokemon Emerald.srm" && c["hash"].is_null())
    );
    let listing = owner
        .request("GET", &format!("/api/sync/folders/{id}"), None)
        .await
        .1;
    assert_eq!(listing["entries"].as_array().unwrap().len(), 1);
    assert_eq!(listing["entries"][0]["name"], "gba");
    state.0.db.close().await;
}

#[tokio::test]
async fn people_cannot_reach_each_others_files_or_sync_without_access() {
    let (_tmp, state, mut owner) = installation().await;
    let mine = login_device(&state, "owner", "owner-password-123", "Owner PC").await;
    let folder = mine
        .ok("POST", "/sync/v1/folders", Some(json!({"name":"Private"})))
        .await;
    let id = folder["id"].as_str().unwrap();
    let secret = mine.upload(b"owner secret").await;
    mine.commit(
        id,
        json!([{"path":"secret.sav","base_version":0,"hash":secret}]),
        "reject",
    )
    .await;

    // Members cannot sync until the owner grants it.
    let mut stranger = Browser {
        app: web::router(state.clone()),
        cookie: String::new(),
        csrf: String::new(),
    };
    stranger.session().await;
    stranger
        .post(
            "/api/register",
            json!({"username":"stranger","password":"member-password-123"}),
        )
        .await;
    let uid: String = sqlx::query_scalar("SELECT id FROM users WHERE username='stranger'")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    owner
        .action("user-status", &uid, json!({"status":"approved"}))
        .await;
    let app = web::router(state.clone());
    let (status, _) = send(
        &app,
        "",
        "POST",
        "/sync/v1/login",
        Body::from(
            json!({"username":"stranger","password":"member-password-123","name":"x"}).to_string(),
        ),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (mut member, _) = member(&state, &mut owner, "member").await;
    let theirs = login_device(&state, "member", "member-password-123", "Member phone").await;
    assert_eq!(
        theirs
            .ok("GET", "/sync/v1/folders", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        theirs
            .call("GET", &format!("/sync/v1/folders/{id}/changes"), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        member
            .request("GET", &format!("/api/sync/folders/{id}"), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // Knowing a hash is not enough to read or claim the content.
    assert_eq!(
        send(
            &app,
            &theirs.token,
            "HEAD",
            &format!("/sync/v1/blobs/{secret}"),
            Body::empty(),
            false
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            &theirs.token,
            "GET",
            &format!("/sync/v1/blobs/{secret}"),
            Body::empty(),
            false
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let own = theirs
        .ok("POST", "/sync/v1/folders", Some(json!({"name":"Mine"})))
        .await;
    let (status, value) = theirs
        .call(
            "POST",
            &format!("/sync/v1/folders/{}/commit", own["id"].as_str().unwrap()),
            Some(json!({"changes":[{"path":"stolen.sav","hash":secret}]})),
        )
        .await;
    assert_eq!(
        (status, value["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("unknown_blob"))
    );

    // The API never accepts the browser cookie, so it needs no CSRF token.
    assert_eq!(
        member.request("GET", "/sync/v1/me", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    // Removing the grant stops the device; changing the password signs it out.
    owner
        .action(
            "resource-access",
            "sync",
            json!({"visibility":"restricted","grants":[]}),
        )
        .await;
    assert_eq!(
        theirs.call("GET", "/sync/v1/me", None).await.0,
        StatusCode::FORBIDDEN
    );
    owner
        .action(
            "resource-access",
            "sync",
            json!({"visibility":"members","grants":[]}),
        )
        .await;
    assert_eq!(
        theirs.call("GET", "/sync/v1/me", None).await.0,
        StatusCode::OK
    );
    let (status, _) = owner.request("POST", "/api/action", Some(json!({"action":"resource-access","id":"sync","data":{"visibility":"public","grants":[]}}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    member
        .action(
            "password",
            "",
            json!({"current_password":"member-password-123","password":"member-password-456"}),
        )
        .await;
    assert_eq!(
        theirs.call("GET", "/sync/v1/me", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    state.0.db.close().await;
}

#[tokio::test]
async fn uploads_are_verified_and_limited() {
    let (_tmp, state, mut owner) = installation().await;
    let device = login_device(&state, "owner", "owner-password-123", "PC").await;
    let wrong = hex::encode(Sha256::digest(b"something else"));
    let (status, body) = send(
        &device.app,
        &device.token,
        "PUT",
        &format!("/sync/v1/blobs/{wrong}"),
        Body::from("actual bytes"),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("hash_mismatch"));
    assert_eq!(
        std::fs::read_dir(state.0.data.join("sync/staging"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        send(
            &device.app,
            &device.token,
            "PUT",
            "/sync/v1/blobs/NOTAHASH",
            Body::from("x"),
            false
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    let mut settings = owner.request("GET", "/api/state", None).await.1["settings"].clone();
    settings["sync_max_file_mb"] = json!(1);
    owner.action("settings", "", settings).await;
    let big = vec![7u8; 1024 * 1024 + 1];
    let hash = hex::encode(Sha256::digest(&big));
    let (status, body) = send(
        &device.app,
        &device.token,
        "PUT",
        &format!("/sync/v1/blobs/{hash}"),
        Body::from(big),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(String::from_utf8_lossy(&body).contains("too_large"));

    let uid: String = sqlx::query_scalar("SELECT id FROM users WHERE owner=1")
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    owner.action("sync-quota", &uid, json!({"mb":0})).await;
    let hash = hex::encode(Sha256::digest(b"tiny"));
    let (status, body) = send(
        &device.app,
        &device.token,
        "PUT",
        &format!("/sync/v1/blobs/{hash}"),
        Body::from("tiny"),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(String::from_utf8_lossy(&body).contains("quota_exceeded"));
    owner.action("sync-quota", &uid, json!({"mb":null})).await;
    device.upload(b"tiny").await;
    assert_eq!(
        device.ok("GET", "/sync/v1/me", None).await["quota"]["used"],
        4
    );

    let folder = device
        .ok("POST", "/sync/v1/folders", Some(json!({"name":"Docs"})))
        .await;
    let (status, value) = device
        .call(
            "POST",
            &format!("/sync/v1/folders/{}/commit", folder["id"].as_str().unwrap()),
            Some(json!({"changes":[{"path":"../escape","hash":hash}]})),
        )
        .await;
    assert_eq!(
        (status, value["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_path"))
    );
    state.0.db.close().await;
}

#[tokio::test]
async fn long_polls_wake_when_another_device_commits() {
    let (_tmp, state, _owner) = installation().await;
    let waiting = login_device(&state, "owner", "owner-password-123", "Thor").await;
    let writer = login_device(&state, "owner", "owner-password-123", "PC").await;
    let folder = writer
        .ok("POST", "/sync/v1/folders", Some(json!({"name":"Saves"})))
        .await;
    let id = folder["id"].as_str().unwrap().to_string();
    let path = format!("/sync/v1/folders/{id}/changes?since=0&wait=30");
    let started = std::time::Instant::now();
    let poll = tokio::spawn(async move { waiting.ok("GET", &path, None).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let hash = writer.upload(b"save").await;
    writer
        .commit(&id, json!([{"path":"a.sav","hash":hash}]), "reject")
        .await;
    let feed = tokio::time::timeout(std::time::Duration::from_secs(10), poll)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(feed["changes"][0]["path"], "a.sav");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    state.0.db.close().await;
}

#[tokio::test]
async fn backups_carry_synced_files_and_collection_spares_referenced_blobs() {
    let (tmp, state, mut owner) = installation().await;
    let device = login_device(&state, "owner", "owner-password-123", "PC").await;
    let folder = device
        .ok("POST", "/sync/v1/folders", Some(json!({"name":"Saves"})))
        .await;
    let id = folder["id"].as_str().unwrap();
    let kept = device.upload(b"kept save").await;
    device
        .commit(id, json!([{"path":"kept.sav","hash":kept}]), "reject")
        .await;
    let abandoned = device.upload(b"never committed").await;
    let file = owner.action("backup", "", json!({})).await["file"]
        .as_str()
        .unwrap()
        .to_string();

    // Abandoned uploads survive a day, so a slow client can still commit them.
    assert_eq!(loki_server::sync::collect(&state).await.unwrap(), 0);
    sqlx::query("UPDATE sync_uploads SET created=0")
        .execute(&state.0.db)
        .await
        .unwrap();
    assert_eq!(loki_server::sync::collect(&state).await.unwrap(), 1);
    let blob =
        |data: &std::path::Path, hash: &str| data.join("sync/blobs").join(&hash[..2]).join(hash);
    assert!(blob(&state.0.data, &kept).exists());
    assert!(!blob(&state.0.data, &abandoned).exists());
    assert_eq!(
        device.ok("GET", "/sync/v1/me", None).await["quota"]["used"],
        9
    );

    let restored = tmp.path().join("restored");
    std::fs::create_dir(&restored).unwrap();
    loki_server::storage::restore(&state.0.data.join("backups").join(file), &restored).unwrap();
    assert_eq!(std::fs::read(blob(&restored, &kept)).unwrap(), b"kept save");
    let copy = State::open(restored, "127.0.0.1:7071".parse().unwrap())
        .await
        .unwrap();
    let path: String = sqlx::query_scalar("SELECT path FROM sync_files WHERE hash=?")
        .bind(&kept)
        .fetch_one(&copy.0.db)
        .await
        .unwrap();
    assert_eq!(path, "kept.sav");
    copy.0.db.close().await;
    state.0.db.close().await;
}
