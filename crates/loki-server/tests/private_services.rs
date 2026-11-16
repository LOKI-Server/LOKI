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
    for (id, owner) in [("owner", true), ("alice", false), ("bob", false)] {
        sqlx::query("INSERT INTO users(id,username,display_name,email,password,owner,status,created) VALUES(?1,?1,?1,?2,'private-password-hash',?3,'approved',1)")
            .bind(id).bind(format!("{id}@example.test")).bind(owner).execute(&state.0.db).await.unwrap();
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
#[ignore = "requires Git 2.54+ (LOKI_GIT_EXECUTABLE) and loopback networking"]
async fn real_git_clients_enforce_private_access_and_merge_reviewed_forks() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::{
        path::{Path, PathBuf},
        process::Stdio,
        time::Duration,
    };

    async fn git(
        binary: &Path,
        folder: &Path,
        args: &[&str],
        secret: Option<&str>,
    ) -> std::process::Output {
        let mut git = tokio::process::Command::new(binary);
        git.current_dir(folder)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .args([
                "-c",
                "credential.helper=",
                "-c",
                "user.name=LOKI Test",
                "-c",
                "user.email=test@example.test",
            ]);
        if let Some(secret) = secret {
            git.args([
                "-c",
                &format!(
                    "http.extraHeader=Authorization: Basic {}",
                    STANDARD.encode(format!("test:{secret}"))
                ),
            ]);
        }
        git.args(args).output().await.unwrap()
    }

    fn success(result: std::process::Output) {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    let bin = std::env::var_os("LOKI_GIT_EXECUTABLE")
        .map(PathBuf::from)
        .expect("set LOKI_GIT_EXECUTABLE to a Git 2.54+ executable");
    let (folder, state) = fixture().await;
    let bind = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let origin = format!("http://{bind}");
    let (owner, csrf) = auth::new_session(&state, Some(("owner", 0)), "test")
        .await
        .unwrap();
    let (alice, alice_csrf) = auth::new_session(&state, Some(("alice", 0)), "test")
        .await
        .unwrap();
    state.0.db.close().await;
    drop(state);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_loki-server"))
        .env("LOKI_GIT_EXECUTABLE", &bin)
        .arg("--data-dir")
        .arg(folder.path())
        .args(["--bind", &bind.to_string(), "--no-tray"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    for _ in 0..100 {
        if http.get(format!("{origin}/health")).send().await.is_ok() {
            break;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "server exited during startup"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let post = |path: String, owner_account: bool, value: Value| {
        let (token, csrf) = if owner_account {
            (&owner, &csrf)
        } else {
            (&alice, &alice_csrf)
        };
        http.post(format!("{origin}{path}"))
            .header("cookie", format!("loki_session={token}"))
            .header("x-csrf-token", csrf)
            .json(&value)
            .send()
    };
    async fn json_response(response: reqwest::Response) -> Value {
        let code = response.status();
        let value: Value = response.json().await.unwrap();
        assert!(code.is_success(), "{code}: {value}");
        value
    }
    let repo = json_response(
        post("/api/git".into(), true, json!({"name":"upstream"}))
            .await
            .unwrap(),
    )
    .await;
    let id = repo["id"].as_str().unwrap();
    let creds = json_response(
        post(format!("/api/git/{id}/tokens"), true, json!({"write":true}))
            .await
            .unwrap(),
    )
    .await;
    let token = creds["token"].as_str().unwrap();
    let clone_url = format!("{origin}/git-http/{id}.git");
    assert!(
        !git(&bin, folder.path(), &["ls-remote", &clone_url], None)
            .await
            .status
            .success()
    );
    let work = folder.path().join("work");
    std::fs::create_dir(&work).unwrap();
    success(git(&bin, &work, &["init", "-b", "main"], None).await);
    std::fs::write(work.join("readme.txt"), "initial").unwrap();
    success(git(&bin, &work, &["add", "readme.txt"], None).await);
    success(git(&bin, &work, &["commit", "-m", "initial"], None).await);
    success(git(&bin, &work, &["push", &clone_url, "main"], Some(token)).await);
    success(
        git(
            &bin,
            folder.path(),
            &["clone", &clone_url, "checkout"],
            Some(token),
        )
        .await,
    );
    assert_eq!(
        std::fs::read_to_string(folder.path().join("checkout/readme.txt")).unwrap(),
        "initial"
    );
    std::fs::write(work.join("readme.txt"), "change").unwrap();
    success(git(&bin, &work, &["commit", "-am", "change"], None).await);
    let protect = git(&bin, &work, &["push", &clone_url, "main"], Some(token)).await;
    assert!(
        !protect.status.success(),
        "the default branch accepted a direct push"
    );
    assert!(String::from_utf8_lossy(&protect.stderr).contains("Open a pull request"));
    json_response(post("/api/action".into(),true,json!({"action":"resource-access","id":"git-create","data":{"visibility":"restricted","grants":[{"subject_kind":"user","subject":"alice","role":"access"}]}})).await.unwrap()).await;
    json_response(
        post(
            format!("/api/git/{id}/permissions"),
            true,
            json!({"subject_kind":"user","subject":"alice","role":"read"}),
        )
        .await
        .unwrap(),
    )
    .await;
    let fork = json_response(
        post(
            "/api/git".into(),
            false,
            json!({"name":"fork","upstream":id}),
        )
        .await
        .unwrap(),
    )
    .await;
    let fork = fork["id"].as_str().unwrap();
    let creds = json_response(
        post(
            format!("/api/git/{fork}/tokens"),
            false,
            json!({"write":true}),
        )
        .await
        .unwrap(),
    )
    .await;
    let fork_token = creds["token"].as_str().unwrap();
    let fork_url = format!("{origin}/git-http/{fork}.git");
    assert!(
        !git(&bin, folder.path(), &["ls-remote", &fork_url], Some(token))
            .await
            .status
            .success()
    );
    success(
        git(
            &bin,
            &work,
            &["push", &fork_url, "HEAD:refs/heads/topic"],
            Some(fork_token),
        )
        .await,
    );
    let pull = json_response(
        post(
            format!("/api/git/{id}/items"),
            false,
            json!({"kind":"pull","title":"Change","body":"","source":fork,"branch":"topic"}),
        )
        .await
        .unwrap(),
    )
    .await;
    let item = format!("/api/git/{id}/items/{}", pull["id"].as_str().unwrap());
    let reviewed = json_response(
        http.get(format!("{origin}{item}"))
            .header("cookie", format!("loki_session={owner}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let head = &reviewed["item"]["head"];
    let base = &reviewed["item"]["base"];
    assert_eq!(
        post(
            item.clone(),
            true,
            json!({"action":"merge","head":head,"base":base})
        )
        .await
        .unwrap()
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        post(
            item.clone(),
            true,
            json!({"action":"approve","head":"stale","base":base})
        )
        .await
        .unwrap()
        .status(),
        StatusCode::CONFLICT
    );
    json_response(
        post(
            item.clone(),
            true,
            json!({"action":"approve","head":head,"base":base}),
        )
        .await
        .unwrap(),
    )
    .await;
    json_response(
        post(
            item,
            true,
            json!({"action":"merge","head":head,"base":base}),
        )
        .await
        .unwrap(),
    )
    .await;
    success(
        git(
            &bin,
            folder.path(),
            &["clone", &clone_url, "merged"],
            Some(token),
        )
        .await,
    );
    assert_eq!(
        std::fs::read_to_string(folder.path().join("merged/readme.txt")).unwrap(),
        "change"
    );
    let user_cookie = format!("loki_session={alice}");
    let listed = json_response(
        http.get(format!("{origin}/api/git/{id}/items"))
            .header("cookie", &user_cookie)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(listed["items"][0]["status"], "merged");
    let group_name = "Git contributors";
    json_response(
        post(
            "/api/action".into(),
            true,
            json!({"action":"group-create","data":{"name":group_name}}),
        )
        .await
        .unwrap(),
    )
    .await;
    let dashboard = json_response(
        http.get(format!("{origin}/api/state"))
            .header("cookie", format!("loki_session={owner}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let group = dashboard["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["name"] == group_name)
        .unwrap()["id"]
        .as_str()
        .unwrap();
    json_response(
        post(
            format!("/api/git/{id}/permissions"),
            true,
            json!({"subject_kind":"group","subject":group,"role":"read"}),
        )
        .await
        .unwrap(),
    )
    .await;
    json_response(
        post(
            "/api/action".into(),
            true,
            json!({"action":"membership","id":"alice","data":{"group":group,"enabled":true}}),
        )
        .await
        .unwrap(),
    )
    .await;
    json_response(
        post(
            format!("/api/git/{id}/permissions"),
            true,
            json!({"subject_kind":"user","subject":"alice","role":"remove"}),
        )
        .await
        .unwrap(),
    )
    .await;
    let alice_creds = json_response(
        post(
            format!("/api/git/{id}/tokens"),
            false,
            json!({"write":false}),
        )
        .await
        .unwrap(),
    )
    .await;
    let alice_token = alice_creds["token"].as_str().unwrap();
    success(
        git(
            &bin,
            folder.path(),
            &["ls-remote", &clone_url],
            Some(alice_token),
        )
        .await,
    );
    json_response(
        post(
            "/api/action".into(),
            true,
            json!({"action":"membership","id":"alice","data":{"group":group,"enabled":false}}),
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(
        !git(
            &bin,
            folder.path(),
            &["ls-remote", &clone_url],
            Some(alice_token)
        )
        .await
        .status
        .success()
    );
    success(
        git(
            &bin,
            folder.path(),
            &["ls-remote", &fork_url],
            Some(fork_token),
        )
        .await,
    );
    let backup = json_response(
        post("/api/action".into(), true, json!({"action":"backup"}))
            .await
            .unwrap(),
    )
    .await;
    let restored = folder.path().join("restored");
    std::fs::create_dir(&restored).unwrap();
    let backup_path = folder
        .path()
        .join("backups")
        .join(backup["file"].as_str().unwrap());
    loki_server::storage::restore(&backup_path, &restored).unwrap();
    let restored_repo = restored.join("git-repos").join(format!("{id}.git"));
    success(
        git(
            &bin,
            &restored,
            &[
                "--git-dir",
                restored_repo.to_str().unwrap(),
                "fsck",
                "--no-reflogs",
            ],
            None,
        )
        .await,
    );
    child.kill().await.unwrap();
}
