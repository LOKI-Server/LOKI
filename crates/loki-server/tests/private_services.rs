use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use loki_server::{State, auth, web};
use serde_json::{Value, json};
use std::io::Read;
use tower::ServiceExt;

async fn fixture() -> (tempfile::TempDir, State) {
    let folder = tempfile::tempdir().unwrap();
    let state = State::open(folder.path().to_owned(), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    let password = auth::hash(&state, "test password for LOKI".into())
        .await
        .unwrap();
    for (id, owner) in [("owner", true), ("alice", false), ("bob", false)] {
        sqlx::query("INSERT INTO users(id,username,display_name,email,password,owner,status,created) VALUES(?1,?1,?1,?2,?3,?4,'approved',1)")
            .bind(id).bind(format!("{id}@example.test")).bind(&password).bind(owner).execute(&state.0.db).await.unwrap();
    }
    (folder, state)
}

struct Browser {
    app: Router,
    cookie: String,
    csrf: String,
}

impl Browser {
    async fn new(state: &State, user: &str) -> Self {
        let (raw, csrf) = auth::new_session(state, Some((user, 0)), "test")
            .await
            .unwrap();
        Self {
            app: web::router(state.clone()),
            cookie: format!("loki_session={raw}"),
            csrf,
        }
    }

    async fn request(&self, method: &str, path: &str, data: Value) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::HOST, "127.0.0.1:7070")
                    .header(header::COOKIE, &self.cookie)
                    .header("x-csrf-token", &self.csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(data.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn shares_containing_the_data_directory_hide_all_of_it() {
    let outer = tempfile::tempdir().unwrap();
    let data = outer.path().join("loki-data");
    let state = State::open(data.clone(), "127.0.0.1:7070".parse().unwrap())
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,username,display_name,password,owner,status,created) VALUES('alice','alice','alice','x',0,'approved',1)")
        .execute(&state.0.db).await.unwrap();
    std::fs::write(outer.path().join("public.txt"), "shared").unwrap();
    std::fs::create_dir_all(data.join("sync/blobs/ab")).unwrap();
    std::fs::write(data.join("sync/blobs/ab/save"), "another user's save").unwrap();
    std::fs::create_dir_all(data.join("caddy")).unwrap();
    std::fs::write(data.join("caddy/key.pem"), "tls key").unwrap();
    sqlx::query(
        "INSERT INTO resources(id,kind,name,visibility) VALUES('outer','share','Outer','members')",
    )
    .execute(&state.0.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO shares(id,root) VALUES('outer',?)")
        .bind(outer.path().to_string_lossy().as_ref())
        .execute(&state.0.db)
        .await
        .unwrap();
    let alice = Browser::new(&state, "alice").await;
    let response = alice.request("GET", "/api/shares/outer", Value::Null).await;
    assert_eq!(response.status(), StatusCode::OK);
    let listing = String::from_utf8(
        to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(listing.contains("public.txt"), "{listing}");
    assert!(!listing.contains("loki-data"), "{listing}");
    let file = |path: &str| format!("/api/shares/outer/file?path={path}");
    assert_eq!(
        alice
            .request("GET", &file("public.txt"), Value::Null)
            .await
            .status(),
        StatusCode::OK
    );
    for hidden in [
        "loki-data",
        "loki-data/loki.db",
        "loki-data/identity.pem",
        "loki-data/caddy/key.pem",
        "loki-data/sync/blobs/ab/save",
        "LOKI-DATA/caddy/key.pem",
    ] {
        let status = alice
            .request("GET", &file(hidden), Value::Null)
            .await
            .status();
        assert!(
            matches!(status, StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST),
            "{hidden}: {status}"
        );
        let status = alice
            .request(
                "GET",
                &format!("/api/shares/outer?path={hidden}"),
                Value::Null,
            )
            .await
            .status();
        assert!(
            matches!(status, StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST),
            "listing {hidden}: {status}"
        );
    }
    state.0.db.close().await;
}

#[tokio::test]
async fn selected_exports_are_private_and_shares_cannot_serve_personal_bodies() {
    let (_folder, state) = fixture().await;
    let app = "notes";
    let bodies = [
        ("alice", auth::id(), "Alice's file"),
        ("bob", auth::id(), "Bob's file"),
    ];
    for (user, blob, text) in &bodies {
        std::fs::write(state.0.data.join("personal-files").join(blob), text).unwrap();
        sqlx::query(
            "INSERT INTO personal_files(user_id,app_id,path,blob,size) VALUES(?,?,'hello.txt',?,?)",
        )
        .bind(user)
        .bind(app)
        .bind(blob)
        .bind(text.len() as i64)
        .execute(&state.0.db)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO resources(id,kind,name,visibility) VALUES('parent','share','Parent','members');").execute(&state.0.db).await.unwrap();
    sqlx::query("INSERT INTO shares(id,root) VALUES('parent',?)")
        .bind(state.0.data.to_string_lossy().as_ref())
        .execute(&state.0.db)
        .await
        .unwrap();
    let alice = Browser::new(&state, "alice").await;
    let response = alice
        .request(
            "GET",
            "/api/personal/export?app=notes&profile=1",
            Value::Null,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(zip.len(), 3);
    let mut note = String::new();
    zip.by_name("apps/notes/hello.txt")
        .unwrap()
        .read_to_string(&mut note)
        .unwrap();
    assert_eq!(note, "Alice's file");
    let profile: Value = serde_json::from_reader(zip.by_name("profile.json").unwrap()).unwrap();
    assert_eq!(profile["username"], "alice");
    assert!(profile.get("password").is_none());
    assert_eq!(
        alice
            .request(
                "GET",
                &format!(
                    "/api/shares/parent/file?path=personal-files/{}",
                    bodies[1].1
                ),
                Value::Null
            )
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = alice.request("GET", "/api/personal", Value::Null).await;
    assert_eq!(response.status(), StatusCode::OK);
    let summary: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
    assert_eq!(summary, json!({"folders":["notes"],"saves":[]}));

    let owner = Browser::new(&state, "owner").await;
    let response = owner
        .request("GET", "/api/personal/export?app=notes", Value::Null)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(zip.len(), 1);

    // A body replaced or removed after the export listed it is reported, not fatal.
    sqlx::query("INSERT INTO personal_files(user_id,app_id,path,blob,size) VALUES('alice','notes','gone.txt',?,4)")
        .bind(auth::id()).execute(&state.0.db).await.unwrap();
    let response = alice
        .request("GET", "/api/personal/export?app=notes", Value::Null)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let manifest: Value = serde_json::from_reader(zip.by_name("manifest.json").unwrap()).unwrap();
    assert_eq!(manifest["files"], json!(["apps/notes/hello.txt"]));
    assert_eq!(manifest["skipped"], json!(["apps/notes/gone.txt"]));
}

#[tokio::test]
async fn management_pairing_keeps_browser_native_and_owner_permissions_separate() {
    let (_folder, state) = fixture().await;
    let owner = Browser::new(&state, "owner").await;
    let alice = Browser::new(&state, "alice").await;
    let json_body = |response: axum::response::Response| async {
        serde_json::from_slice::<Value>(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap()
    };
    let call = |method: &str, path: &str, token: &str, data: Value| {
        owner.app.clone().oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(data.to_string()))
                .unwrap(),
        )
    };
    let repo = json_body(
        call(
            "POST",
            "/api/v1/auth/pair",
            "",
            json!({"scope":"repositories","name":"Test CLI","platform":"desktop"}),
        )
        .await
        .unwrap(),
    )
    .await;
    let approval = owner
        .request(
            "POST",
            "/api/action",
            json!({"action":"device-approve","data":{"code":repo["user_code"]}}),
        )
        .await;
    assert_eq!(approval.status(), StatusCode::OK);
    let wrong_flow = call(
        "POST",
        "/auth/v1/pair/poll",
        "",
        json!({"device_code":repo["device_code"]}),
    )
    .await
    .unwrap();
    assert!(!wrong_flow.status().is_success());
    let repo_session = json_body(
        call(
            "POST",
            "/api/v1/auth/pair/poll",
            "",
            json!({"scope":"repositories","device_code":repo["device_code"]}),
        )
        .await
        .unwrap(),
    )
    .await;
    let repository_token = repo_session["token"].as_str().unwrap();
    assert_eq!(
        call("GET", "/api/v1/repos", repository_token, Value::Null)
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        call("GET", "/api/v1/admin/users", repository_token, Value::Null)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        owner
            .request(
                "POST",
                "/api/v1/admin/actions",
                json!({"action":"user-create","data":{"username":"blocked"}})
            )
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let old = json_body(
        call(
            "POST",
            "/auth/v1/login",
            "",
            json!({"username":"owner","password":"test password for LOKI","name":"Old client"}),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        call(
            "GET",
            "/api/v1/repos",
            old["token"].as_str().unwrap(),
            Value::Null
        )
        .await
        .unwrap()
        .status(),
        StatusCode::FORBIDDEN
    );
    let password_login = call(
        "POST", "/api/v1/auth/login", "", json!({"scope":"repositories","username":"alice","password":"test password for LOKI","name":"Repository CLI"})
    ).await.unwrap();
    assert_eq!(password_login.status(), StatusCode::OK);
    let password_session = json_body(password_login).await;
    assert_eq!(password_session["scope"], "repositories");
    assert_eq!(
        call(
            "GET",
            "/api/v1/repos",
            password_session["token"].as_str().unwrap(),
            Value::Null
        )
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
    assert_eq!(call("POST", "/api/v1/auth/login", "", json!({"scope":"owner","username":"owner","password":"test password for LOKI","name":"Repository CLI"})).await.unwrap().status(), StatusCode::FORBIDDEN);
    let admin = json_body(
        call(
            "POST",
            "/api/v1/auth/pair",
            "",
            json!({"scope":"owner","name":"Admin CLI"}),
        )
        .await
        .unwrap(),
    )
    .await;
    let decide = |password: &str| json!({"action":"device-approve","data":{"code":admin["user_code"],"proof":{"current_password":password}}});
    assert_eq!(
        alice
            .request("POST", "/api/action", decide("test password for LOKI"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        owner
            .request(
                "POST",
                "/api/action",
                json!({"action":"device-approve","data":{"code":admin["user_code"]}})
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        owner
            .request("POST", "/api/action", decide("test password for LOKI"))
            .await
            .status(),
        StatusCode::OK
    );
    let admin_session = json_body(
        call(
            "POST",
            "/api/v1/auth/pair/poll",
            "",
            json!({"scope":"owner","device_code":admin["device_code"]}),
        )
        .await
        .unwrap(),
    )
    .await;
    let administrator = admin_session["token"].as_str().unwrap();
    let users = json_body(
        call("GET", "/api/v1/admin/users", administrator, Value::Null)
            .await
            .unwrap(),
    )
    .await;
    assert!(
        users
            .as_array()
            .unwrap()
            .iter()
            .all(|user| user.get("password").is_none() && user.get("totp").is_none())
    );
    assert_eq!(
        call(
            "POST",
            "/api/v1/admin/actions",
            administrator,
            json!({"action":"user-status","id":"alice","data":{"status":"suspended"}})
        )
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        call(
            "POST",
            "/api/v1/admin/actions",
            administrator,
            json!({"action":"storage"})
        )
        .await
        .unwrap()
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call("DELETE", "/auth/v1/device", administrator, Value::Null)
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        call("GET", "/api/v1/admin/users", administrator, Value::Null)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn folder_downloads_and_updates_follow_share_access() {
    async fn json_body(response: axum::response::Response) -> Value {
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert!(
            status.is_success(),
            "{status}: {}",
            String::from_utf8_lossy(&bytes)
        );
        serde_json::from_slice(&bytes).unwrap()
    }
    let (_data, state) = fixture().await;
    let files = tempfile::tempdir().unwrap();
    for folder in ["Game/bin", "Game/data/empty"] {
        std::fs::create_dir_all(files.path().join(folder)).unwrap();
    }
    for (file, body) in [
        ("Game/bin/game.exe", "old"),
        ("Game/data/level.dat", "level"),
        ("Game/data/secret.bak", "hidden"),
    ] {
        std::fs::write(files.path().join(file), body).unwrap();
    }

    let owner = Browser::new(&state, "owner").await;
    let save = |id: &str, enabled: Option<bool>| {
        owner.request("POST", "/api/action", json!({"action":"share-save","id":id,
            "data":{"name":"Games","root":files.path(),"supports_updates":enabled,"excluded_extensions":"bak"}}))
    };
    let created = json_body(save("", None).await).await;
    let id = created["id"].as_str().unwrap();
    let api = |method: &str, path: &str, token: &str, body: Value| {
        owner.app.clone().oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
    };
    let token = |username: &str| {
        api(
            "POST",
            "/auth/v1/login",
            "",
            json!({"username":username,
            "password":"test password for LOKI","name":"File downloads test"}),
        )
    };
    let owner_auth = json_body(token("owner").await.unwrap()).await;
    let alice_auth = json_body(token("alice").await.unwrap()).await;
    let owner_token = owner_auth["token"].as_str().unwrap();
    let alice_token = alice_auth["token"].as_str().unwrap();

    let list = json_body(
        api("GET", "/files/v1/shares", owner_token, Value::Null)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list["shares"][0]["id"], id);
    assert_eq!(list["shares"][0]["supports_updates"], false);

    let manifest = format!("/files/v1/shares/{id}/manifest?path=Game");
    let check = format!("{manifest}&update=true");
    assert_eq!(
        api("GET", &manifest, alice_token, Value::Null)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        api("GET", &check, owner_token, Value::Null)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let before = json_body(
        api("GET", &manifest, owner_token, Value::Null)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(before["directory"], true);
    let entries = before["entries"].as_array().unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "bin",
            "bin/game.exe",
            "data",
            "data/empty",
            "data/level.dat"
        ]
    );

    let archive_path = format!("/api/shares/{id}/archive?path=Game");
    let response = owner.request("GET", &archive_path, Value::Null).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert!(archive.by_name("Game/data/empty/").unwrap().is_dir());
    assert!(archive.by_name("Game/bin/game.exe").is_ok());
    assert!(archive.by_name("Game/data/level.dat").is_ok());
    assert!(archive.by_name("Game/data/secret.bak").is_err());

    assert_eq!(save(id, Some(true)).await.status(), StatusCode::OK);
    std::fs::write(files.path().join("Game/bin/game.exe"), "new").unwrap();
    let after = json_body(api("GET", &check, owner_token, Value::Null).await.unwrap()).await;
    assert_ne!(before["revision"], after["revision"]);
    let hash = after["entries"][1]["sha256"].as_str().unwrap();
    let chunks = format!("/files/v1/shares/{id}/chunks?path=Game/bin/game.exe&sha256={hash}");
    let response = json_body(api("GET", &chunks, owner_token, Value::Null).await.unwrap()).await;
    assert_eq!(
        response["chunks"],
        json!([{"offset":0,"size":3,"sha256":hash}])
    );
    assert_eq!(response["next_offset"], Value::Null);
    assert_eq!(save(id, Some(false)).await.status(), StatusCode::OK);
    assert_eq!(
        api("GET", &chunks, owner_token, Value::Null)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        api("GET", &manifest, owner_token, Value::Null)
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires real loopback TCP"]
async fn native_repository_push_clone_review_and_backup() {
    use skald_vcs_core::client::{Repository, WorkspaceChange};
    use skald_vcs_core::{Id, Kind, Settings, Store};
    async fn json_body(response: axum::response::Response) -> Value {
        let status = response.status();
        let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        assert!(
            status.is_success(),
            "{status}: {}",
            String::from_utf8_lossy(&body)
        );
        serde_json::from_slice(&body).unwrap()
    }

    let (folder, state) = fixture().await;
    let owner = Browser::new(&state, "owner").await;
    let alice = Browser::new(&state, "alice").await;
    let repo = json_body(
        owner
            .request("POST", "/api/repos", json!({"name":"test-project"}))
            .await,
    )
    .await;
    let id = repo["id"].as_str().unwrap();
    let browser_url = format!("/api/repos/{id}");
    assert_eq!(
        alice
            .request("GET", &browser_url, Value::Null)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        owner
            .request(
                "POST",
                &format!("{browser_url}/permissions"),
                json!({"subject_kind":"user","subject":"alice","role":"read"})
            )
            .await
            .status()
            .is_success()
    );

    let token_path = "/api/account/vcs-tokens";
    let issue = |label: &str, write| json!({"label":label,"write":write,"current_password":"test password for LOKI"});
    let owner_token = json_body(
        owner
            .request("POST", token_path, issue("writer", true))
            .await,
    )
    .await;
    let alice_token = json_body(
        alice
            .request("POST", token_path, issue("reader", false))
            .await,
    )
    .await;
    let writer = owner_token["token"].as_str().unwrap().to_owned();
    let reader = alice_token["token"].as_str().unwrap().to_owned();
    assert!(writer.starts_with("skald_"));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = web::router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let url = format!("{origin}/skald/v1/repos/{id}");
    let client = reqwest::Client::new();
    let discovery: Value = client
        .get(format!("{origin}/skald/v1"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(discovery["repository_discovery"], "/skald/v1/repos");
    assert_eq!(
        client
            .get(format!("{url}/refs"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let paired: Value = client
        .post(format!("{origin}/api/v1/auth/login"))
        .json(&json!({
            "scope":"repositories","username":"alice","password":"test password for LOKI",
            "name":"Native integration test"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let scoped: Value = client
        .post(format!("{origin}/api/v1/repos/{id}/credential"))
        .bearer_auth(paired["token"].as_str().unwrap())
        .json(&json!({"write":false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        scoped["remote_url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/skald/v1/repos/{id}"))
    );
    let scoped = scoped["token"].as_str().unwrap();
    let visible: Value = client
        .get(format!("{origin}/skald/v1/repos"))
        .bearer_auth(scoped)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(visible["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(visible["repositories"][0]["id"], id);
    let test_id = Id::of(Kind::Chunk, b"true bytes");
    let object = format!("{url}/objects/{test_id}");
    for (token, wanted) in [
        (&reader, StatusCode::FORBIDDEN),
        (&writer, StatusCode::BAD_REQUEST),
    ] {
        let response = client
            .put(&object)
            .bearer_auth(token)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body("wrong bytes")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), wanted);
    }
    let working = tempfile::tempdir().unwrap();
    let path = working.path().to_owned();
    let clone_url = url.clone();
    let write_token = writer.clone();
    let read_token = reader.clone();
    let first = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let source = Repository::init(path.join("source"), Settings::default())?;
        let bytes = [0, 250, 249, 99].repeat(400_000);
        std::fs::write(source.root().join("asset.bin"), &bytes)?;
        source.stage(&[".".into()])?;
        let first = source.commit("Owner", "first asset")?;
        source.remote("origin", Some(&clone_url))?;
        source.push("origin", &write_token, None, false, false)?;
        source.branch("feature/texture", None)?;
        source.checkout("feature/texture", false)?;
        std::fs::write(source.root().join("notes.txt"), "Updated asset notes.\n")?;
        source.stage(&["notes.txt".into()])?;
        source.commit("Owner", "add notes")?;
        source.push("origin", &write_token, None, false, false)?;
        let clone = Repository::clone_remote(&path.join("clone"), &clone_url, &read_token)?;
        assert_eq!(std::fs::read(clone.root().join("asset.bin"))?, bytes);
        assert_eq!(clone.history(None, 30)?.len(), 1);
        assert!(clone.branches()?.contains_key("feature/texture"));

        let sparse = Repository::clone_sparse(
            &path.join("sparse"),
            &clone_url,
            &read_token,
            &["notes.txt".into()],
        )?;
        let store = Store::open(&sparse.root().join(".skald/objects"), Settings::default())?;
        let excluded = store.snapshot(Some(first))?["asset.bin"].id;
        let excluded_chunks = store.manifest(excluded)?.chunks;
        assert!(!sparse.root().join("asset.bin").exists());
        assert!(
            excluded_chunks
                .iter()
                .all(|chunk| !store.contains(chunk.id))
        );
        assert!(sparse.status()?.unstaged.is_empty());

        sparse.pull("origin", &read_token, Some("feature/texture"), "Owner")?;
        assert_eq!(
            std::fs::read_to_string(sparse.root().join("notes.txt"))?,
            "Updated asset notes.\n"
        );
        sparse.branch("feature/sparse", None)?;
        sparse.checkout("feature/sparse", false)?;
        std::fs::write(sparse.root().join("notes.txt"), "Sparse checkout notes.\n")?;
        sparse.stage(&[".".into()])?;
        assert_eq!(
            sparse.status()?.staged.keys().collect::<Vec<_>>(),
            ["notes.txt"]
        );
        let sparse_tip = sparse.commit("Owner", "edit from sparse workspace")?;
        assert!(
            excluded_chunks
                .iter()
                .all(|chunk| !store.contains(chunk.id))
        );
        sparse.push("origin", &write_token, None, false, false)?;
        assert_eq!(store.snapshot(Some(sparse_tip))?["asset.bin"].id, excluded);
        let full = Repository::clone_remote(&path.join("full-check"), &clone_url, &read_token)?;
        full.checkout("feature/sparse", false)?;
        assert_eq!(std::fs::read(full.root().join("asset.bin"))?, bytes);

        let add = WorkspaceChange::Add(vec!["asset.bin".into()]);
        assert_eq!(
            sparse.change_workspace(&add, true, None)?.add,
            ["asset.bin"]
        );
        assert!(sparse.change_workspace(&add, false, None).is_err());
        sparse.change_workspace(&add, false, Some(&read_token))?;
        assert_eq!(std::fs::read(sparse.root().join("asset.bin"))?, bytes);
        assert!(excluded_chunks.iter().all(|chunk| store.contains(chunk.id)));
        sparse.change_workspace(
            &WorkspaceChange::Remove(vec!["asset.bin".into()]),
            false,
            None,
        )?;
        assert!(sparse.prune_workspace(&read_token)? > 0);
        assert!(
            excluded_chunks
                .iter()
                .all(|chunk| !store.contains(chunk.id))
        );
        sparse.verify()?;
        sparse.change_workspace(&add, false, Some(&read_token))?;
        assert_eq!(std::fs::read(sparse.root().join("asset.bin"))?, bytes);

        let ours = [0, 81, 82, 83].repeat(400_000);
        std::fs::write(sparse.root().join("asset.bin"), &ours)?;
        sparse.stage(&["asset.bin".into()])?;
        sparse.commit("Owner", "local asset edits")?;
        let remove = WorkspaceChange::Remove(vec!["asset.bin".into()]);
        sparse.change_workspace(&remove, false, None)?;
        sparse.prune_workspace(&read_token)?;
        sparse.change_workspace(&add, false, None)?;
        assert_eq!(std::fs::read(sparse.root().join("asset.bin"))?, ours);
        sparse.change_workspace(&remove, false, None)?;

        full.branch("feature/other-asset", None)?;
        full.checkout("feature/other-asset", false)?;
        let theirs = [0, 61, 62, 63].repeat(400_000);
        std::fs::write(full.root().join("asset.bin"), &theirs)?;
        full.stage(&["asset.bin".into()])?;
        full.commit("Owner", "remote asset edits")?;
        full.push("origin", &write_token, None, false, false)?;
        let merge = sparse.pull("origin", &read_token, Some("feature/other-asset"), "Owner")?;
        assert_eq!(
            merge.conflicts.into_iter().collect::<Vec<_>>(),
            ["asset.bin"]
        );
        assert_eq!(std::fs::read(sparse.root().join("asset.bin"))?, ours);
        sparse.restore_with_token(
            "remotes/origin/branches/feature/other-asset",
            &["asset.bin".into()],
            Some(&read_token),
        )?;
        assert_eq!(std::fs::read(sparse.root().join("asset.bin"))?, theirs);
        sparse.stage(&[".".into()])?;
        sparse.commit("Owner", "resolve sparse asset conflict")?;
        sparse.verify()?;
        Ok(first)
    })
    .await
    .unwrap()
    .unwrap();
    let raw = alice
        .request(
            "GET",
            &format!("{browser_url}/file?path=asset.bin&raw=true"),
            Value::Null,
        )
        .await;
    assert_eq!(
        to_bytes(raw.into_body(), 2_000_000).await.unwrap().len(),
        1_600_000
    );

    assert!(
        owner
            .request(
                "POST",
                &format!("{browser_url}/settings"),
                json!({"default_branch":"main","protect_default":true})
            )
            .await
            .status()
            .is_success()
    );
    let review = json_body(alice.request("POST", &format!("{browser_url}/items"),
        json!({"kind":"pull","title":"Asset notes","body":"Review notes","branch":"feature/texture"})).await).await;
    let review_url = format!("{browser_url}/items/{}", review["id"].as_str().unwrap());
    let detail = json_body(owner.request("GET", &review_url, Value::Null).await).await;
    assert_eq!(detail["item"]["base"], first.to_string());
    for action in ["approve", "merge"] {
        assert!(owner.request("POST", &review_url,
            json!({"action":action,"head":detail["item"]["head"],"base":detail["item"]["base"]})).await.status().is_success());
    }
    let stale = client
        .put(format!("{url}/refs/branches/main"))
        .bearer_auth(&writer)
        .json(&json!({"expected":first,"target":detail["item"]["head"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::FORBIDDEN);
    let path = working.path().join("clone");
    let pull_token = reader.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let clone = Repository::open(&path)?;
        assert!(
            clone
                .pull("origin", &pull_token, None, "Reader")?
                .conflicts
                .is_empty()
        );
        assert_eq!(
            std::fs::read(clone.root().join("notes.txt"))?,
            b"Updated asset notes.\n"
        );
        clone.verify()
    })
    .await
    .unwrap()
    .unwrap();
    let fork = json_body(
        owner
            .request(
                "POST",
                "/api/repos",
                json!({"name":"test-fork","upstream":id}),
            )
            .await,
    )
    .await;
    let fork_id = fork["id"].as_str().unwrap();
    let fork_url = format!("/api/repos/{fork_id}");
    let fork_detail = json_body(owner.request("GET", &fork_url, Value::Null).await).await;
    assert_eq!(fork_detail["upstream"]["id"], id);
    assert_eq!(fork_detail["revision"], detail["item"]["head"]);
    assert_eq!(
        alice.request("GET", &fork_url, Value::Null).await.status(),
        StatusCode::NOT_FOUND
    );
    let usage: (i64, i64) = sqlx::query_as("SELECT bytes,object_count FROM skald_repos WHERE id=?")
        .bind(fork_id)
        .fetch_one(&state.0.db)
        .await
        .unwrap();
    let original_usage: (i64, i64) =
        sqlx::query_as("SELECT bytes,object_count FROM skald_repos WHERE id=?")
            .bind(id)
            .fetch_one(&state.0.db)
            .await
            .unwrap();
    assert_eq!(usage, original_usage);
    assert!(usage.0 > 0 && usage.1 >= 8);
    assert!(
        owner
            .request(
                "POST",
                &format!("{fork_url}/permissions"),
                json!({"subject_kind":"user","subject":"alice","role":"read"})
            )
            .await
            .status()
            .is_success()
    );
    let fork_refs = format!("{origin}/skald/v1/repos/{fork_id}/refs");
    assert!(
        client
            .get(&fork_refs)
            .bearer_auth(&reader)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(
        client
            .get(&fork_refs)
            .bearer_auth(scoped)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    server.abort();
    let clone = Repository::open(working.path().join("clone")).unwrap();
    assert_eq!(clone.history(None, 30).unwrap().len(), 2);
    assert!(clone.status().unwrap().unstaged.is_empty());

    let old = folder.path().join("git-repos").join(format!("{id}.git"));
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let asset_bytes = b"legacy large-file content";
    let oid = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(asset_bytes));
    let legacy_asset = std::path::Path::new("git-lfs").join(&oid[..2]).join(&oid);
    let asset = folder.path().join(&legacy_asset);
    std::fs::create_dir_all(asset.parent().unwrap()).unwrap();
    std::fs::write(asset, asset_bytes).unwrap();
    let backup = web::backup(&state).await.unwrap();
    let restore = tempfile::tempdir().unwrap();
    loki_server::storage::restore(&folder.path().join("backups").join(backup), restore.path())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(
            restore
                .path()
                .join("git-repos")
                .join(format!("{id}.git"))
                .join("HEAD")
        )
        .unwrap(),
        "ref: refs/heads/main\n"
    );
    assert_eq!(
        std::fs::read(restore.path().join(legacy_asset)).unwrap(),
        asset_bytes
    );
    let hosted =
        skald_vcs_core::server::Hosted::open(&restore.path().join("skald-repos").join(id)).unwrap();
    assert_eq!(
        hosted.refs().unwrap().branches["main"].to_string(),
        detail["item"]["head"]
    );
    hosted.verify().unwrap();
}

#[tokio::test]
async fn group_repositories_and_transfers_pause_changes_until_accepted() {
    async fn json_body(response: axum::response::Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
    }
    let (_folder, state) = fixture().await;
    let owner = Browser::new(&state, "owner").await;
    let alice = Browser::new(&state, "alice").await;
    let bob = Browser::new(&state, "bob").await;
    let team = "0f9a3b5e-2c4d-4e6f-8a1b-3c5d7e9f1a2b";
    sqlx::query("INSERT INTO groups(id,name) VALUES(?,'Team')")
        .bind(team)
        .execute(&state.0.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO memberships(user_id,group_id) VALUES('alice',?)")
        .bind(team)
        .execute(&state.0.db)
        .await
        .unwrap();

    // Members need the server's permission before creating repositories in their group.
    let in_group = json!({"name":"game","group":team});
    assert_eq!(
        alice
            .request("POST", "/api/repos", in_group.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    sqlx::query("INSERT INTO grants(resource,subject_kind,subject,role) VALUES('vcs-group-create','user','alice','access')")
        .execute(&state.0.db)
        .await
        .unwrap();
    let game = json_body(alice.request("POST", "/api/repos", in_group).await).await;
    let game = game["id"].as_str().unwrap().to_owned();
    let game_url = format!("/api/repos/{game}");

    // Group members write; outsiders see nothing; only the server owner gives it away.
    let detail = json_body(alice.request("GET", &game_url, Value::Null).await).await;
    assert_eq!(detail["role"], "write");
    assert_eq!(detail["owner"]["kind"], "group");
    assert_eq!(detail["can_transfer"], false);
    assert_eq!(
        bob.request("GET", &game_url, Value::Null).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        alice.request("DELETE", &game_url, json!({})).await.status(),
        StatusCode::FORBIDDEN
    );
    let start = |kind: &str, target: &str| json!({"action":"start","kind":kind,"target":target});
    assert_eq!(
        alice
            .request(
                "POST",
                &format!("{game_url}/transfer"),
                start("user", "alice")
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    // A group that owns repositories can't be removed.
    let removed = owner
        .request(
            "POST",
            "/api/action",
            json!({"action":"group-delete","id":team,"data":{}}),
        )
        .await;
    assert_eq!(removed.status(), StatusCode::BAD_REQUEST);

    // Giving a repository to someone else waits for them, and pauses pushes meanwhile.
    sqlx::query("INSERT INTO grants(resource,subject_kind,subject,role) VALUES('vcs-create','user','alice','access')")
        .execute(&state.0.db)
        .await
        .unwrap();
    let solo = json_body(
        alice
            .request("POST", "/api/repos", json!({"name":"solo"}))
            .await,
    )
    .await;
    let solo = solo["id"].as_str().unwrap().to_owned();
    let solo_url = format!("/api/repos/{solo}");
    let requested = json_body(
        alice
            .request(
                "POST",
                &format!("{solo_url}/transfer"),
                start("user", "bob"),
            )
            .await,
    )
    .await;
    assert_eq!(requested["transferred"], false);
    let token = json_body(
        alice
            .request(
                "POST",
                "/api/account/vcs-tokens",
                json!({"label":"pc","write":true,"current_password":"test password for LOKI"}),
            )
            .await,
    )
    .await;
    let push = |token: String| {
        let app = web::router(state.clone());
        let solo = solo.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/skald/v1/repos/{solo}/missing"))
                    .header(header::HOST, "127.0.0.1:7070")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({"ids":[]}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };
    let token = token["token"].as_str().unwrap().to_owned();
    assert_eq!(push(token.clone()).await, StatusCode::LOCKED);
    assert_eq!(
        alice.request("DELETE", &solo_url, json!({})).await.status(),
        StatusCode::CONFLICT
    );
    let waiting = json_body(bob.request("GET", "/api/repos", Value::Null).await).await;
    assert_eq!(waiting["transfers"][0]["can_decide"], true);
    assert_eq!(waiting["transfers"][0]["repo_name"], "solo");
    // Only the recipient decides.
    assert_eq!(
        alice
            .request(
                "POST",
                &format!("{solo_url}/transfer"),
                json!({"action":"accept"})
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        bob.request(
            "POST",
            &format!("{solo_url}/transfer"),
            json!({"action":"accept"})
        )
        .await
        .status()
        .is_success()
    );
    let mine = json_body(bob.request("GET", &solo_url, Value::Null).await).await;
    assert_eq!(mine["owner"]["id"], "bob");
    assert_eq!(mine["role"], "manage");
    assert_eq!(
        alice.request("GET", &solo_url, Value::Null).await.status(),
        StatusCode::NOT_FOUND
    );

    // A declined transfer resumes changes for the current owner.
    let again = json_body(
        alice
            .request("POST", "/api/repos", json!({"name":"again"}))
            .await,
    )
    .await;
    let again_url = format!("/api/repos/{}", again["id"].as_str().unwrap());
    alice
        .request(
            "POST",
            &format!("{again_url}/transfer"),
            start("group", team),
        )
        .await;
    assert!(
        owner
            .request(
                "POST",
                &format!("{again_url}/transfer"),
                json!({"action":"decline"})
            )
            .await
            .status()
            .is_success()
    );
    let kept = json_body(alice.request("GET", &again_url, Value::Null).await).await;
    assert!(kept["transfer"].is_null());
    assert_eq!(kept["owner"]["id"], "alice");

    // The server owner moves repositories into groups without waiting.
    let moved = json_body(
        owner
            .request(
                "POST",
                &format!("{again_url}/transfer"),
                start("group", team),
            )
            .await,
    )
    .await;
    assert_eq!(moved["transferred"], true);
    let now = json_body(alice.request("GET", &again_url, Value::Null).await).await;
    assert_eq!(now["owner"]["kind"], "group");
    assert_eq!(now["role"], "write");
}

#[tokio::test]
async fn owners_set_and_clear_repository_limits_for_people_and_groups() {
    async fn json_body(response: axum::response::Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
    }
    let (_folder, state) = fixture().await;
    let owner = Browser::new(&state, "owner").await;
    let alice = Browser::new(&state, "alice").await;
    let set = |data: Value| json!({"action":"vcs-quota","id":"alice","data":data});

    let limits = json!({"repo_mib":2048,"total_mib":10240});
    assert_eq!(
        alice
            .request("POST", "/api/action", set(limits.clone()))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        owner
            .request(
                "POST",
                "/api/action",
                set(json!({"repo_mib":0,"total_mib":null}))
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(
        owner
            .request("POST", "/api/action", set(limits.clone()))
            .await
            .status()
            .is_success()
    );
    let dashboard = json_body(owner.request("GET", "/api/state", Value::Null).await).await;
    assert_eq!(dashboard["vcs_quotas"]["alice"], limits);
    let bytes: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT repo_bytes,total_bytes FROM vcs_quotas WHERE user_id='alice'")
            .fetch_one(&state.0.db)
            .await
            .unwrap();
    assert_eq!(bytes, (Some(2048 << 20), Some(10240 << 20)));

    // Clearing both limits returns alice to the server defaults.
    let clear = json!({"repo_mib":null,"total_mib":null});
    assert!(
        owner
            .request("POST", "/api/action", set(clear))
            .await
            .status()
            .is_success()
    );
    let dashboard = json_body(owner.request("GET", "/api/state", Value::Null).await).await;
    assert_eq!(dashboard["vcs_quotas"], json!({}));
    assert!(
        json_body(alice.request("GET", "/api/state", Value::Null).await)
            .await
            .get("vcs_quotas")
            .is_none()
    );

    // Groups get limits of their own, for the repositories they own.
    let team = "0f9a3b5e-2c4d-4e6f-8a1b-3c5d7e9f1a2b";
    sqlx::query("INSERT INTO groups(id,name) VALUES(?,'Team')")
        .bind(team)
        .execute(&state.0.db)
        .await
        .unwrap();
    let group = |id: &str, data: Value| json!({"action":"vcs-group-quota","id":id,"data":data});
    let only_total = json!({"repo_mib":null,"total_mib":4096});
    assert_eq!(
        alice
            .request("POST", "/api/action", group(team, only_total.clone()))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        owner
            .request(
                "POST",
                "/api/action",
                group("no-such-group", only_total.clone())
            )
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(
        owner
            .request("POST", "/api/action", group(team, only_total.clone()))
            .await
            .status()
            .is_success()
    );
    let dashboard = json_body(owner.request("GET", "/api/state", Value::Null).await).await;
    assert_eq!(dashboard["vcs_group_quotas"][team], only_total);
    assert_eq!(dashboard["vcs_quotas"], json!({}));
}

#[tokio::test]
async fn rejected_accounts_are_removed_three_days_later_unless_they_own_work() {
    async fn json_body(response: axum::response::Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
    }
    let (_folder, state) = fixture().await;
    let owner = Browser::new(&state, "owner").await;
    let alice = Browser::new(&state, "alice").await;
    sqlx::query("INSERT INTO users(id,username,display_name,password,status,created) VALUES('carol','carol','carol','x','pending',1)")
        .execute(&state.0.db)
        .await
        .unwrap();
    // Alice owns a repository, so she keeps her account even once rejected.
    sqlx::query("INSERT INTO grants(resource,subject_kind,subject,role) VALUES('vcs-create','user','alice','access')")
        .execute(&state.0.db)
        .await
        .unwrap();
    assert!(
        alice
            .request("POST", "/api/repos", json!({"name":"keep"}))
            .await
            .status()
            .is_success()
    );
    for id in ["alice", "bob", "carol"] {
        let reject = json!({"action":"user-status","id":id,"data":{"status":"rejected"}});
        assert!(
            owner
                .request("POST", "/api/action", reject)
                .await
                .status()
                .is_success()
        );
    }
    let dashboard = json_body(owner.request("GET", "/api/state", Value::Null).await).await;
    let due = dashboard["removals"]["bob"].as_i64().unwrap();
    assert!((due - loki_server::auth::now() - loki_server::auth::REJECTED_FOR).abs() < 60);

    // Nothing goes before three days have passed.
    assert_eq!(loki_server::auth::remove_rejected(&state).await.unwrap(), 0);
    let long_ago = loki_server::auth::now() - loki_server::auth::REJECTED_FOR - 1;
    sqlx::query("UPDATE users SET rejected_at=? WHERE id IN('alice','bob')")
        .bind(long_ago)
        .execute(&state.0.db)
        .await
        .unwrap();
    assert_eq!(loki_server::auth::remove_rejected(&state).await.unwrap(), 1);
    let left: Vec<String> =
        sqlx::query_scalar("SELECT id FROM users WHERE id IN('alice','bob','carol') ORDER BY id")
            .fetch_all(&state.0.db)
            .await
            .unwrap();
    assert_eq!(left, ["alice", "carol"]);

    // Approving again stops the countdown.
    let approve = json!({"action":"user-status","id":"carol","data":{"status":"approved"}});
    assert!(
        owner
            .request("POST", "/api/action", approve)
            .await
            .status()
            .is_success()
    );
    let rejected: Option<i64> =
        sqlx::query_scalar("SELECT rejected_at FROM users WHERE id='carol'")
            .fetch_one(&state.0.db)
            .await
            .unwrap();
    assert_eq!(rejected, None);
}
