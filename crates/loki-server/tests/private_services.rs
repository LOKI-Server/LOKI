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
    let password = auth::hash(&state, "test password for Git".into())
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
#[ignore = "requires Git 2.54+ (LOKI_GIT_EXECUTABLE) and loopback networking, optional Caddy TLS"]
async fn real_git_clients_enforce_private_access_and_merge_reviewed_forks() {
    use sha2::{Digest, Sha256};
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
        if let Some(certificate) = folder
            .ancestors()
            .map(|dir| dir.join("test-ca.pem"))
            .find(|path| path.is_file())
        {
            git.env("GIT_SSL_CAINFO", certificate);
            if cfg!(windows) {
                git.args(["-c", "http.sslBackend=openssl"]);
            }
        }
        if let Some(secret) = secret {
            git.env("LOKI_TEST_GIT_TOKEN", secret).args([
                "-c",
                r#"credential.helper=!f() { if test "$1" = get; then printf 'username=test\npassword=%s\n' "$LOKI_TEST_GIT_TOKEN"; fi; }; f"#,
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
    fn available_address() -> std::net::SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }
    let bind = available_address();
    let mut http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15));
    let caddy = if let Some(binary) = std::env::var_os("LOKI_TEST_CADDY_EXECUTABLE") {
        let address = available_address();
        let storage = folder.path().join("caddy-test-data");
        let caddyfile = folder.path().join("Caddyfile.test");
        std::fs::write(&caddyfile, format!(
            "{{\n admin off\n persist_config off\n auto_https disable_redirects\n servers {{\n protocols h1 h2\n }}\n skip_install_trust\n storage file_system {{\n root \"{}\"\n }}\n}}\nhttps://{address} {{\n bind 127.0.0.1\n tls internal\n reverse_proxy {bind}\n}}\n",
            storage.to_string_lossy().replace('\\', "/")
        )).unwrap();
        let mut child = tokio::process::Command::new(binary)
            .arg("run")
            .arg("--config")
            .arg(caddyfile)
            .args(["--adapter", "caddyfile"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let root = storage.join("pki/authorities/local/root.crt");
        for _ in 0..100 {
            if root.is_file() {
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "test HTTPS proxy failed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let pem = std::fs::read(root).unwrap();
        std::fs::write(folder.path().join("test-ca.pem"), &pem).unwrap();
        http = http.add_root_certificate(reqwest::Certificate::from_pem(&pem).unwrap());
        Some((child, format!("https://{address}")))
    } else {
        None
    };
    let origin = caddy
        .as_ref()
        .map(|(_, origin)| origin.clone())
        .unwrap_or_else(|| format!("http://{bind}"));
    let cookie_name = if origin.starts_with("https://") {
        "__Host-loki_session"
    } else {
        "loki_session"
    };
    let mut settings = state.0.settings.read().await.clone();
    settings.public_url = origin.clone();
    state.save_settings(settings).await.unwrap();
    let (owner, csrf) = auth::new_session(&state, Some(("owner", 0)), "test")
        .await
        .unwrap();
    let (alice, alice_csrf) = auth::new_session(&state, Some(("alice", 0)), "test")
        .await
        .unwrap();
    state.0.db.close().await;
    drop(state);
    let start_server = |path: &Path| {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_loki-server"))
            .env("LOKI_GIT_EXECUTABLE", &bin)
            .arg("--data-dir")
            .arg(path)
            .args(["--bind", &bind.to_string(), "--no-tray"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let mut child = start_server(folder.path());
    let http = http.build().unwrap();
    for _ in 0..100 {
        if http
            .get(format!("{origin}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
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
            .header("cookie", format!("{cookie_name}={token}"))
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
        post(
            "/api/account/git-tokens".into(),
            true,
            json!({"label":"Owner PC","write":true,"current_password":"test password for Git"}),
        )
        .await
        .unwrap(),
    )
    .await;
    let token = creds["token"].as_str().unwrap();
    json_response(
        post(
            format!("/api/git/{id}/settings"),
            true,
            json!({"default_branch":"main","protect_default":true}),
        )
        .await
        .unwrap(),
    )
    .await;
    let empty = json_response(
        post(
            "/api/git".into(),
            true,
            json!({"name":"empty","default_branch":"trunk"}),
        )
        .await
        .unwrap(),
    )
    .await;
    let empty_id = empty["id"].as_str().unwrap();
    let empty_url = format!("{origin}/git-http/{empty_id}.git");
    success(git(&bin, folder.path(), &["ls-remote", &empty_url], Some(token)).await);
    let clone_url = format!("{origin}/git-http/{id}.git");
    let old_token = format!("loki_git_{}", auth::random());
    let migration_db = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(folder.path().join("loki.db")),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO git_tokens(id,digest,user_id,repo_id,write,created,expires,auth_version) VALUES(?,?,'owner',?,0,?,?,0)")
        .bind(auth::id()).bind(auth::digest(&old_token)).bind(id).bind(auth::now()).bind(auth::now() + 3600)
        .execute(&migration_db).await.unwrap();
    migration_db.close().await;
    success(
        git(
            &bin,
            folder.path(),
            &["ls-remote", &clone_url],
            Some(&old_token),
        )
        .await,
    );
    assert!(
        !git(
            &bin,
            folder.path(),
            &["ls-remote", &empty_url],
            Some(&old_token)
        )
        .await
        .status
        .success()
    );
    let solo = json_response(
        post(
            "/api/git".into(),
            true,
            json!({"name":"solo","default_branch":"trunk"}),
        )
        .await
        .unwrap(),
    )
    .await;
    let solo_id = solo["id"].as_str().unwrap();
    let solo_url = format!("{origin}/git-http/{solo_id}.git");
    let solo_work = folder.path().join("solo");
    std::fs::create_dir(&solo_work).unwrap();
    success(git(&bin, &solo_work, &["init", "-b", "trunk"], None).await);
    success(
        git(
            &bin,
            &solo_work,
            &["commit", "--allow-empty", "-m", "Initial"],
            None,
        )
        .await,
    );
    success(git(&bin, &solo_work, &["push", &solo_url, "trunk"], Some(token)).await);
    success(
        git(
            &bin,
            &solo_work,
            &["commit", "--allow-empty", "-m", "Direct update"],
            None,
        )
        .await,
    );
    success(git(&bin, &solo_work, &["push", &solo_url, "trunk"], Some(token)).await);
    success(
        git(
            &bin,
            &solo_work,
            &["push", &solo_url, "HEAD:refs/heads/release"],
            Some(token),
        )
        .await,
    );
    json_response(
        post(
            format!("/api/git/{solo_id}/settings"),
            true,
            json!({"default_branch":"release","protect_default":false}),
        )
        .await
        .unwrap(),
    )
    .await;
    let advertised = git(
        &bin,
        &solo_work,
        &["ls-remote", "--symref", &solo_url, "HEAD"],
        Some(token),
    )
    .await;
    assert!(String::from_utf8_lossy(&advertised.stdout).starts_with("ref: refs/heads/release"));
    assert!(
        !git(&bin, folder.path(), &["ls-remote", &clone_url], None)
            .await
            .status
            .success()
    );
    let work = folder.path().join("work");
    std::fs::create_dir(&work).unwrap();
    success(git(&bin, &work, &["init", "-b", "main"], None).await);
    let lfs = git(&bin, &work, &["lfs", "version"], None)
        .await
        .status
        .success();
    if lfs {
        success(git(&bin, &work, &["lfs", "install", "--local"], None).await);
        success(git(&bin, &work, &["lfs", "track", "*.asset"], None).await);
        std::fs::write(work.join("level.asset"), b"initial binary asset\x00\xff").unwrap();
        success(git(&bin, &work, &["add", ".gitattributes", "level.asset"], None).await);
    }
    std::fs::write(work.join("readme.txt"), "initial").unwrap();
    success(git(&bin, &work, &["add", "readme.txt"], None).await);
    success(git(&bin, &work, &["commit", "-m", "initial"], None).await);
    if lfs {
        let missing = git(
            &bin,
            &work,
            &["push", "--no-verify", &clone_url, "main"],
            Some(token),
        )
        .await;
        assert!(!missing.status.success());
        assert!(
            String::from_utf8_lossy(&missing.stderr).contains("Missing Git LFS object"),
            "{}",
            String::from_utf8_lossy(&missing.stderr)
        );
    }
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
    if lfs {
        let checkout = folder.path().join("checkout");
        success(git(&bin, &checkout, &["lfs", "install", "--local"], None).await);
        success(git(&bin, &checkout, &["lfs", "pull"], Some(token)).await);
        assert_eq!(
            std::fs::read(checkout.join("level.asset")).unwrap(),
            b"initial binary asset\x00\xff"
        );
        std::fs::write(work.join("level.asset"), b"updated binary asset\x00\xff").unwrap();
        success(git(&bin, &work, &["add", "level.asset"], None).await);
    }
    if lfs {
        let oid = hex::encode(Sha256::digest(b"initial binary asset\x00\xff"));
        let path = format!("/info/lfs/objects/{oid}");
        let denied = http
            .get(format!("{empty_url}{path}"))
            .basic_auth("owner", Some(token))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        let ranged = http
            .get(format!("{clone_url}{path}"))
            .basic_auth("owner", Some(token))
            .header(header::RANGE, "bytes=0-6")
            .send()
            .await
            .unwrap();
        assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(ranged.bytes().await.unwrap(), &b"initial"[..]);
        let wrong = http
            .put(format!("{empty_url}{path}"))
            .basic_auth("owner", Some(token))
            .body("wrong hash")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    std::fs::write(work.join("readme.txt"), "change").unwrap();
    success(git(&bin, &work, &["commit", "-am", "change"], None).await);
    let protect = git(
        &bin,
        &work,
        &["push", "--no-verify", &clone_url, "main"],
        Some(token),
    )
    .await;
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
            "/api/account/git-tokens".into(),
            false,
            json!({"label":"Alice PC","write":true,"current_password":"test password for Git"}),
        )
        .await
        .unwrap(),
    )
    .await;
    let fork_token = creds["token"].as_str().unwrap();
    let fork_url = format!("{origin}/git-http/{fork}.git");
    success(git(&bin, folder.path(), &["ls-remote", &fork_url], Some(token)).await);
    assert!(
        !git(
            &bin,
            folder.path(),
            &["ls-remote", &empty_url],
            Some(fork_token)
        )
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
            .header("cookie", format!("{cookie_name}={owner}"))
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
    let user_cookie = format!("{cookie_name}={alice}");
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
            .header("cookie", format!("{cookie_name}={owner}"))
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
            "/api/account/git-tokens".into(),
            false,
            json!({"label":"Alice read only","write":false,"current_password":"test password for Git"}),
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
    let list = json_response(
        http.get(format!("{origin}/api/account/git-tokens"))
            .header("cookie", format!("{cookie_name}={alice}"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let read_id = list
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["label"] == "Alice read only")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    assert!(
        !git(
            &bin,
            &work,
            &[
                "push",
                "--no-verify",
                &fork_url,
                "HEAD:refs/heads/read-only"
            ],
            Some(alice_token)
        )
        .await
        .status
        .success()
    );
    json_response(
        http.delete(format!("{origin}/api/account/git-tokens/{read_id}"))
            .header("cookie", format!("{cookie_name}={alice}"))
            .header("x-csrf-token", &alice_csrf)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        !git(
            &bin,
            folder.path(),
            &["ls-remote", &fork_url],
            Some(alice_token)
        )
        .await
        .status
        .success()
    );

    if let Some(cli) = std::env::var_os("LOKI_TEST_CLI_EXECUTABLE") {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        assert!(
            origin.starts_with("http://"),
            "Run the desktop CLI fixture on loopback HTTP"
        );
        let config = folder.path().join("cli-gitconfig");
        std::fs::write(
            &config,
            b"[filter \"lfs\"]\nprocess = git-lfs filter-process\nrequired = true\n",
        )
        .unwrap();
        let profile = folder.path().join("cli-profile");
        let open = |directory: &Path, args: &[&str]| {
            let mut command = tokio::process::Command::new(&cli);
            command
                .current_dir(directory)
                .args(args)
                .env("LOKI_GIT_CONFIG_DIR", &profile)
                .env_remove("LOKI_CONFIG_DIR")
                .env_remove("LOKI_TOKEN")
                .env_remove("LOKI_SERVER")
                .env("GIT_CONFIG_GLOBAL", &config)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .kill_on_drop(true);
            command
        };
        let mut pairing = open(&work, &["auth", "login", &origin])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(pairing.stdout.take().unwrap()).lines();
        let code = tokio::time::timeout(Duration::from_secs(15), async {
            assert!(
                lines
                    .next_line()
                    .await
                    .unwrap()
                    .unwrap()
                    .starts_with("Open ")
            );
            let line = lines.next_line().await.unwrap().unwrap();
            line.strip_prefix("Approve code ")
                .unwrap()
                .split_ascii_whitespace()
                .next()
                .unwrap()
                .to_owned()
        })
        .await
        .unwrap();
        json_response(
            post(
                "/api/action".into(),
                true,
                json!({"action":"device-approve","data":{"code":code}}),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_secs(20), pairing.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        success(open(&work, &["auth", "status"]).output().await.unwrap());
        success(
            open(&work, &["repo", "create", "cli-game", "--push"])
                .output()
                .await
                .unwrap(),
        );
        success(
            open(
                folder.path(),
                &["repo", "clone", "cli-game", "cli-checkout"],
            )
            .output()
            .await
            .unwrap(),
        );
        let checkout = folder.path().join("cli-checkout");
        assert_eq!(
            std::fs::read_to_string(checkout.join("readme.txt")).unwrap(),
            "change"
        );
        if lfs {
            assert_eq!(
                std::fs::read(checkout.join("level.asset")).unwrap(),
                b"updated binary asset\x00\xff"
            );
        }
        std::fs::write(work.join("readme.txt"), "pulled update").unwrap();
        success(
            git(
                &bin,
                &work,
                &["commit", "-am", "Advance the CLI repository"],
                None,
            )
            .await,
        );
        success(
            open(&work, &["repo", "push", "cli-game"])
                .output()
                .await
                .unwrap(),
        );
        success(
            open(&checkout, &["repo", "fetch", "cli-game"])
                .output()
                .await
                .unwrap(),
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("readme.txt")).unwrap(),
            "change"
        );
        success(
            open(&checkout, &["repo", "pull", "cli-game"])
                .output()
                .await
                .unwrap(),
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("readme.txt")).unwrap(),
            "pulled update"
        );
        success(open(&work, &["auth", "logout"]).output().await.unwrap());
        let stored: Value =
            serde_json::from_slice(&std::fs::read(profile.join("config.json")).unwrap()).unwrap();
        assert_eq!(stored["profile"], "git");
        assert_eq!(stored["token"], "");
        assert!(!profile.join("index").exists());
        let mut password = open(
            &work,
            &["auth", "login", &origin, "--password-stdin", "owner"],
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
        password
            .stdin
            .take()
            .unwrap()
            .write_all(b"test password for Git\n")
            .await
            .unwrap();
        success(password.wait_with_output().await.unwrap());
        success(open(&work, &["repo", "list"]).output().await.unwrap());
        success(open(&work, &["auth", "logout"]).output().await.unwrap());
    }
    let repository = folder.path().join("git-repos").join(format!("{id}.git"));
    success(
        git(
            &bin,
            folder.path(),
            &[
                "--git-dir",
                repository.to_str().unwrap(),
                "pack-refs",
                "--all",
                "--prune",
            ],
            None,
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
    loki_server::storage::restore(&backup_path, &restored)
        .await
        .unwrap();
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
    let restored_empty = restored.join("git-repos").join(format!("{empty_id}.git"));
    let output = git(
        &bin,
        &restored,
        &[
            "--git-dir",
            restored_empty.to_str().unwrap(),
            "symbolic-ref",
            "HEAD",
        ],
        None,
    )
    .await;
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "refs/heads/trunk"
    );
    child.kill().await.unwrap();
    if lfs {
        let mut restored_server = start_server(&restored);
        for _ in 0..100 {
            if http
                .get(format!("{origin}/health"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            assert!(
                restored_server.try_wait().unwrap().is_none(),
                "restored server exited"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        success(
            git(
                &bin,
                folder.path(),
                &["clone", &clone_url, "restored-checkout"],
                Some(token),
            )
            .await,
        );
        let checkout = folder.path().join("restored-checkout");
        success(git(&bin, &checkout, &["lfs", "install", "--local"], None).await);
        success(git(&bin, &checkout, &["lfs", "pull"], Some(token)).await);
        assert_eq!(
            std::fs::read(checkout.join("level.asset")).unwrap(),
            b"updated binary asset\x00\xff"
        );
        success(git(&bin, &checkout, &["checkout", "HEAD^1"], Some(token)).await);
        assert_eq!(
            std::fs::read(checkout.join("level.asset")).unwrap(),
            b"initial binary asset\x00\xff"
        );
        restored_server.kill().await.unwrap();
    }
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
            json!({"username":"owner","password":"test password for Git","name":"Old client"}),
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
        "POST", "/api/v1/auth/login", "", json!({"scope":"repositories","username":"alice","password":"test password for Git","name":"Git CLI"})
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
    assert_eq!(call("POST", "/api/v1/auth/login", "", json!({"scope":"owner","username":"owner","password":"test password for Git","name":"Git CLI"})).await.unwrap().status(), StatusCode::FORBIDDEN);
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
            .request("POST", "/api/action", decide("test password for Git"))
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
            .request("POST", "/api/action", decide("test password for Git"))
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
async fn app_repositories_respect_access_and_keep_their_creation_type() {
    let (_data, state) = fixture().await;
    let files = tempfile::tempdir().unwrap();
    for folder in ["Launcher", "Editor/windows", "Editor/linux", "Pinned"] {
        std::fs::create_dir_all(files.path().join(folder)).unwrap();
    }
    for (file, body) in [
        ("Launcher/launcher.exe", "old"),
        ("Editor/windows/editor.zip", "windows"),
        ("Editor/linux/editor.tar.gz", "linux"),
        ("Editor/linux/private.bak", "hidden"),
        ("Pinned/current.bin", "release"),
        (
            "Pinned/app.json",
            r#"{"name":"Pinned release","version":"2.4","builds":{"windows":"current.bin"}}"#,
        ),
    ] {
        std::fs::write(files.path().join(file), body).unwrap();
    }

    let owner = Browser::new(&state, "owner").await;
    let read = |response: axum::response::Response| async {
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice::<Value>(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap()
    };
    let save = |id: &str, name: &str, kind: Option<&str>| {
        owner.request("POST", "/api/action", json!({"action":"share-save","id":id,
            "data":{"name":name,"root":files.path(),"share_type":kind,"excluded_extensions":"bak"}}))
    };
    let normal = read(save("", "Files", None).await).await;
    let app = read(save("", "Builds", Some("apps")).await).await;
    let normal = normal["id"].as_str().unwrap();
    let app = app["id"].as_str().unwrap();
    assert_eq!(
        save(normal, "Changed", Some("apps")).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(save(app, "Renamed", None).await.status(), StatusCode::OK);
    assert_eq!(
        save(app, "Changed", Some("files")).await.status(),
        StatusCode::BAD_REQUEST
    );
    let web = read(owner.request("GET", "/api/state", Value::Null).await).await;
    for (id, kind) in [(normal, "files"), (app, "apps")] {
        let share = web["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|share| share["id"] == id)
            .unwrap();
        assert_eq!(share["share_type"], kind);
    }
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
            json!({"username":username,"password":"test password for Git","name":"Catalog test"}),
        )
    };
    let owner_auth = read(token("owner").await.unwrap()).await;
    let alice_auth = read(token("alice").await.unwrap()).await;
    let owner_token = owner_auth["token"].as_str().unwrap();
    let alice_token = alice_auth["token"].as_str().unwrap();

    let list = read(
        api("GET", "/files/v1/shares", owner_token, Value::Null)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list["shares"][0]["id"], normal);
    assert_eq!(list["shares"].as_array().unwrap().len(), 1);
    let list = read(
        api(
            "GET",
            "/files/v1/app-repositories",
            owner_token,
            Value::Null,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(list["shares"][0]["id"], app);
    let list = read(
        api(
            "GET",
            "/files/v1/app-repositories",
            alice_token,
            Value::Null,
        )
        .await
        .unwrap(),
    )
    .await;
    assert!(list["shares"].as_array().unwrap().is_empty());
    let catalog = format!("/files/v1/app-repositories/{app}");
    assert_eq!(
        api("GET", &catalog, alice_token, Value::Null)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let listing = read(
        api("GET", &catalog, owner_token, Value::Null)
            .await
            .unwrap(),
    )
    .await;
    let apps = listing["apps"].as_array().unwrap();
    let lookup = |id: &str| apps.iter().find(|app| app["id"] == id).unwrap();
    assert_eq!(
        lookup("Launcher")["builds"]["windows"]["path"],
        "Launcher/launcher.exe"
    );
    assert_eq!(
        lookup("Editor")["builds"]["linux"]["path"],
        "Editor/linux/editor.tar.gz"
    );
    assert_eq!(
        lookup("Editor")["builds"]["windows"]["path"],
        "Editor/windows/editor.zip"
    );
    assert_eq!(lookup("Pinned")["version"], "2.4");
    assert_eq!(
        lookup("Pinned")["builds"]["windows"]["path"],
        "Pinned/current.bin"
    );

    let path = format!("/files/v1/shares/{app}/metadata?path=Launcher/launcher.exe&checksum=true");
    let original = read(api("GET", &path, owner_token, Value::Null).await.unwrap()).await;
    std::fs::write(files.path().join("Launcher/launcher.exe"), "new").unwrap();
    let updated = read(api("GET", &path, owner_token, Value::Null).await.unwrap()).await;
    assert_eq!(original["size"], updated["size"]);
    assert_ne!(original["sha256"], updated["sha256"]);
    let response = api(
        "GET",
        &format!("/files/v1/shares/{app}/file?path=Launcher/launcher.exe"),
        owner_token,
        Value::Null,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 100).await.unwrap(),
        &b"new"[..]
    );
}
