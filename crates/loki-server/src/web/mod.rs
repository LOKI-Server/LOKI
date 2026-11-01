mod account;
mod actions;
mod admin;

use crate::{Error, Result, State, apps, auth, caddy, files, identity, platform};
pub(crate) use actions::args;
use askama::Template;
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Query, Request, State as ExtractState},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use tokio::io::AsyncWriteExt;

#[derive(Template)]
#[template(path = "shell.html")]
struct Shell {
    version: &'static str,
    assets: &'static str,
    modules: Vec<&'static str>,
    boot: Option<String>,
}
pub fn router(state: State) -> Router {
    Router::new()
        .route(
            "/health",
            get(|| async { Json(json!({"ok":true,"protocol":1})) }),
        )
        .route("/assets/{*path}", get(crate::assets::serve))
        .route("/api/session", get(auth::session))
        .route("/api/setup", post(auth::setup))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/register", post(auth::register))
        .route("/api/state", get(dashboard))
        .route("/api/action", post(actions::action))
        .route("/api/reset", post(reset))
        .route(
            "/api/apps/import",
            post(import).layer(DefaultBodyLimit::max(537919488)),
        )
        .route(
            "/api/avatar",
            post(avatar).layer(DefaultBodyLimit::max(5242880)),
        )
        .route("/avatars/{id}", get(serve_avatar))
        .route("/api/folders", get(files::folders))
        .route(
            "/api/apps/{id}/panel",
            get(panel_view)
                .post(panel_action)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/api/shares/{id}", get(files::list))
        .route("/api/shares/{id}/file", get(files::download))
        .route("/api/shares/{id}/info", get(files::info))
        .route("/api/logs/{id}", get(logs))
        .route("/api/backups/{id}", get(backup_download))
        .route(
            "/api/backups/import",
            post(backup_import).layer(DefaultBodyLimit::max(2148532224)),
        )
        .route(
            "/.well-known/openid-configuration",
            get(identity::discovery),
        )
        .route("/oidc/jwks", get(identity::jwks))
        .route("/oidc/authorize", get(identity::authorize))
        .route("/oidc/token", post(identity::token))
        .route("/oidc/device", post(identity::device))
        .route("/oidc/userinfo", get(identity::userinfo))
        .route("/oidc/introspect", post(identity::introspect))
        .route("/oidc/revoke", post(identity::revoke))
        .route("/gateway/check/{id}", get(gateway_check))
        .route("/_loki/start", get(gateway_start))
        .route("/_loki/callback", get(gateway_callback))
        .merge(crate::sync::router())
        .fallback(get(shell))
        .layer(DefaultBodyLimit::max(131072))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}
async fn shell(ExtractState(state): ExtractState<State>, headers: HeaderMap) -> Response {
    let boot = if state
        .0
        .maintenance
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        None
    } else {
        boot(&state, &headers).await
    };
    let page = Shell {
        version: env!("CARGO_PKG_VERSION"),
        assets: crate::assets::base(),
        modules: crate::assets::modules(),
        boot,
    };
    match page.render() {
        // The page can carry the visitor's session, so it is never cached.
        Ok(s) => ([(header::CACHE_CONTROL, "no-store")], Html(s)).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
/// Session and dashboard state embedded in the page, saving the dashboard's first
/// round trips. Only browsers that already hold a session get it; others fetch.
async fn boot(state: &State, headers: &HeaderMap) -> Option<String> {
    let session = auth::existing_session(state, headers).await?;
    let mut boot = json!({ "session": session });
    if boot["session"]["user"]["status"] == "approved"
        && let Ok(Json(value)) = dashboard(ExtractState(state.clone()), headers.clone()).await
    {
        boot["state"] = value;
    }
    let text = serde_json::to_string(&boot).ok()?;
    // JSON inside <script> must not be able to close the element.
    Some(
        text.replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026"),
    )
}
async fn guard(
    ExtractState(state): ExtractState<State>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let Ok(_permit) = state.0.requests.acquire().await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match crate::components::resolve(&state, request.headers()).await {
        Ok(Some(app)) => {
            if state
                .0
                .maintenance
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "LOKI is restarting. Reconnect shortly.",
                )
                    .into_response();
            }
            return match crate::components::serve(&state, app, request).await {
                Ok(response) => response,
                Err(error) => error.into_response(),
            };
        }
        Ok(None) => {}
        Err(error) => return error.into_response(),
    }
    if state
        .0
        .maintenance
        .load(std::sync::atomic::Ordering::SeqCst)
        && (path.starts_with("/api/")
            || path.starts_with("/oidc/")
            || path.starts_with("/gateway/")
            || path.starts_with("/_loki/")
            || path.starts_with("/sync/v1"))
    {
        return Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "LOKI is restarting after a storage operation. Reconnect shortly.",
        )
        .into_response();
    }
    let request_headers = request.headers().clone();
    let peer = auth::client_ip(
        request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|p| p.0),
        &request_headers,
        state.0.settings.read().await.caddy_enabled,
    );
    request
        .extensions_mut()
        .insert(auth::ClientIp(peer.clone()));
    if request.method() != axum::http::Method::GET
        && request.method() != axum::http::Method::HEAD
        && path.starts_with("/api/")
    {
        let valid = async {
            let ctx = auth::context(&state, &request_headers).await.map_err(|error| {
                if error.status == StatusCode::UNAUTHORIZED {
                    Error::new(StatusCode::UNAUTHORIZED, "Your browser session expired. Refresh this page and try again.")
                } else {
                    error
                }
            })?;
            let csrf = request_headers
                .get("x-csrf-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !auth::equal(csrf, &ctx.session.csrf) {
                return Err(Error::new(StatusCode::FORBIDDEN, "This page's security token is no longer current. Refresh this page and try again."));
            }
            if let Some(origin) = request_headers.get("origin").and_then(|v| v.to_str().ok()) {
                let public = state.0.settings.read().await.public_url.clone();
                let local = format!("http://{}", state.0.bind);
                let local_alias = url::Url::parse(origin).ok().is_some_and(|u| {
                    u.scheme() == "http"
                        && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
                        && u.port_or_known_default() == Some(state.0.bind.port())
                });
                if origin != public && origin != local && !local_alias {
                    return Err(Error::forbidden());
                }
            }
            if matches!(path.as_str(), "/api/login" | "/api/register" | "/api/reset") {
                auth::rate(&state, format!("auth-ip:{peer}"), 40, 900).await?;
            }
            Ok::<_, Error>(())
        }
        .await;
        if let Err(e) = valid {
            return e.into_response();
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers
        .entry(header::X_CONTENT_TYPE_OPTIONS)
        .or_insert(axum::http::HeaderValue::from_static("nosniff"));
    headers
        .entry(header::REFERRER_POLICY)
        .or_insert(axum::http::HeaderValue::from_static("no-referrer"));
    headers.entry(header::CONTENT_SECURITY_POLICY).or_insert(axum::http::HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' blob:; media-src 'self'; frame-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    if path.starts_with("/api/")
        || path.starts_with("/oidc/")
        || path.starts_with("/gateway/")
        || path.starts_with("/_loki/")
        || path.starts_with("/sync/v1")
    {
        headers.insert(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
    }
    response
}

async fn dashboard(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.signed_in()?;
    let rows = sqlx::query("SELECT * FROM resources ORDER BY name COLLATE NOCASE")
        .fetch_all(&state.0.db)
        .await?;
    let mut resources = Vec::new();
    for row in rows {
        let id: String = row.get("id");
        if !auth::allowed(&state, Some(user), &id, "access").await? {
            continue;
        }
        let mut resource = json!({"id":id,"kind":row.get::<String,_>("kind"),"name":row.get::<String,_>("name"),"visibility":row.get::<String,_>("visibility")});
        match resource["kind"].as_str().unwrap_or("") {
            "app" => {
                let app = apps::get(&state, &id).await?;
                let manifest: apps::Manifest = serde_json::from_str(&app.manifest)?;
                resource["enabled"] = json!(app.enabled);
                resource["url"] = json!(apps::public_url(&app));
                resource["hostname"] = json!(app.hostname);
                resource["ready"] = json!(
                    state
                        .0
                        .runtimes
                        .lock()
                        .await
                        .get(&id)
                        .is_some_and(|r| r.ready)
                );
                resource["manifest"] = json!(manifest);
                if user.owner {
                    let mut config: Value = serde_json::from_str(&app.config)?;
                    for f in &manifest.fields {
                        if f.kind == "secret" && config.get(&f.key).is_some_and(|v| !v.is_null()) {
                            config[&f.key] = json!("********");
                        }
                    }
                    resource["config"] = config;
                    resource["lan_bind"] = json!(app.lan_bind);
                    resource["error"] = json!(app.error);
                    resource["previous"] = json!(app.previous.is_some());
                }
            }
            "route" => {
                let row = sqlx::query("SELECT * FROM routes WHERE id=?")
                    .bind(&id)
                    .fetch_one(&state.0.db)
                    .await?;
                resource["url"] = json!(format!("https://{}", row.get::<String, _>("hostname")));
                if user.owner {
                    for key in ["hostname", "upstream", "service", "service_kind"] {
                        resource[key] = json!(row.get::<String, _>(key));
                    }
                    resource["enabled"] = json!(row.get::<bool, _>("enabled"));
                    resource["gated"] = json!(row.get::<bool, _>("gated"));
                }
            }
            "share" => {
                resource["url"] = json!(format!("/shares/{id}"));
                if user.owner {
                    let (root, excluded): (String, String) =
                        sqlx::query_as("SELECT root,excluded_extensions FROM shares WHERE id=?")
                            .bind(&id)
                            .fetch_one(&state.0.db)
                            .await?;
                    resource["root"] = json!(root);
                    resource["excluded_extensions"] = serde_json::from_str(&excluded)?;
                }
            }
            _ => {}
        }
        resources.push(resource);
    }
    let sessions=sqlx::query("SELECT id,label,created,expires FROM sessions WHERE user_id=? AND expires>? ORDER BY created DESC").bind(&user.id).bind(auth::now()).fetch_all(&state.0.db).await?.into_iter().map(|r|json!({"id":r.get::<String,_>("id"),"label":r.get::<String,_>("label"),"created":r.get::<i64,_>("created"),"current":r.get::<String,_>("id")==ctx.session.id})).collect::<Vec<_>>();
    let mut output = json!({"resources":resources,"sessions":sessions,"mfa":user.totp.is_some(),"data_dir":state.0.data.to_string_lossy(),"target":apps::target(),"loki_version":env!("CARGO_PKG_VERSION"),"oldest_sdk":apps::OLDEST_SDK.to_string()});
    if let Some(sync) = crate::sync::summary(&state, user).await? {
        output["sync"] = sync;
    }
    if user.owner {
        output["settings"] = json!(*state.0.settings.read().await);
        #[cfg(windows)]
        {
            output["startup"] = match state.0.startup.status() {
                Ok(status) => json!(status),
                Err(_) => {
                    json!({"error":"Could not inspect the Windows startup entry. Saving will check again."})
                }
            };
        }
        output["users"] = json!(
            sqlx::query_as::<_, auth::User>("SELECT * FROM users ORDER BY owner DESC,username")
                .fetch_all(&state.0.db)
                .await?
        );
        output["groups"] = json!(
            sqlx::query("SELECT * FROM groups ORDER BY name")
                .fetch_all(&state.0.db)
                .await?
                .into_iter()
                .map(|r| json!({"id":r.get::<String,_>("id"),"name":r.get::<String,_>("name")}))
                .collect::<Vec<_>>()
        );
        output["memberships"] = json!(
            sqlx::query_as::<_, (String, String)>("SELECT user_id,group_id FROM memberships")
                .fetch_all(&state.0.db)
                .await?
        );
        output["grants"]=json!(sqlx::query("SELECT * FROM grants").fetch_all(&state.0.db).await?.into_iter().map(|r|json!({"resource":r.get::<String,_>("resource"),"subject":r.get::<String,_>("subject"),"subject_kind":r.get::<String,_>("subject_kind"),"role":r.get::<String,_>("role")})).collect::<Vec<_>>());
        output["audit"]=json!(sqlx::query("SELECT actor,action,detail,created FROM audit WHERE action!='refresh-spent' ORDER BY id DESC LIMIT 100").fetch_all(&state.0.db).await?.into_iter().map(|r|json!({"actor":r.get::<String,_>("actor"),"action":r.get::<String,_>("action"),"detail":r.get::<String,_>("detail"),"created":r.get::<i64,_>("created")})).collect::<Vec<_>>());
        let mut backups = Vec::new();
        let mut dir = tokio::fs::read_dir(state.0.data.join("backups")).await?;
        while let Some(entry) = dir.next_entry().await? {
            if entry.file_name().to_string_lossy().ends_with(".zip") {
                backups.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        backups.sort();
        backups.reverse();
        output["backups"] = json!(backups);
    }
    Ok(Json(output))
}
#[derive(Deserialize)]
struct Reset {
    token: String,
    password: String,
}
async fn reset(
    ExtractState(state): ExtractState<State>,
    axum::Extension(ip): axum::Extension<auth::ClientIp>,
    Json(input): Json<Reset>,
) -> Result<Json<Value>> {
    auth::rate(&state, format!("resets:{}", ip.0), 30, 900).await?;
    let password = auth::hash(&state, input.password).await?;
    let mut tx = state.0.db.begin().await?;
    let uid: String =
        sqlx::query_scalar("DELETE FROM resets WHERE id=? AND expires>? RETURNING user_id")
            .bind(auth::digest(&input.token))
            .bind(auth::now())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| Error::bad("The link is invalid, expired, or already used."))?;
    sqlx::query("UPDATE users SET password=?,auth_version=auth_version+1 WHERE id=?")
        .bind(password)
        .bind(&uid)
        .execute(&mut *tx)
        .await?;
    for table in [
        "sessions",
        "tokens",
        "codes",
        "gateway_sessions",
        "sync_devices",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE user_id=?"))
            .bind(&uid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(json!({"ok":true})))
}
async fn panel_view(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.approved()?;
    Ok(Json(
        crate::components::panel(&state, &id, user, None).await?,
    ))
}
async fn panel_action(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.approved()?;
    Ok(Json(
        crate::components::panel(&state, &id, user, Some(request)).await?,
    ))
}
async fn import(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.owner()?;
    let path = state
        .0
        .data
        .join("staging")
        .join(format!("{}.upload", auth::id()));
    let mut file = tokio::fs::File::create(&path).await?;
    let mut bytes = 0u64;
    let result = async {
        let mut field = multipart
            .next_field()
            .await?
            .ok_or_else(|| Error::bad("Select an app package."))?;
        while let Some(chunk) = field.chunk().await? {
            bytes += chunk.len() as u64;
            if bytes > 536870912 {
                return Err(Error::bad("Package exceeds 512 MiB."));
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        apps::import(&state, path.clone(), &user.id).await
    }
    .await;
    let _ = tokio::fs::remove_file(&path).await;
    Ok(Json(json!({"manifest":result?})))
}
async fn avatar(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.approved()?;
    let field = multipart
        .next_field()
        .await?
        .ok_or_else(|| Error::bad("Select an image."))?;
    let bytes = field.bytes().await?;
    if bytes.len() > 4194304 {
        return Err(Error::bad("Avatar exceeds 4 MiB."));
    }
    let path = state
        .0
        .data
        .join("avatars")
        .join(format!("{}.png", user.id));
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut reader =
            image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        limits.max_alloc = Some(67108864);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|_| Error::bad("Choose a PNG, JPEG, or WebP image up to 4096 × 4096."))?;
        image
            .thumbnail(256, 256)
            .save_with_format(path, image::ImageFormat::Png)?;
        Ok(())
    })
    .await??;
    sqlx::query("UPDATE users SET avatar=1 WHERE id=?")
        .bind(&user.id)
        .execute(&state.0.db)
        .await?;
    Ok(Json(json!({"ok":true})))
}
async fn serve_avatar(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
) -> Result<Response> {
    if uuid::Uuid::parse_str(&id).is_err() {
        return Err(Error::not_found());
    }
    let bytes = tokio::fs::read(state.0.data.join("avatars").join(format!("{id}.png")))
        .await
        .map_err(|_| Error::not_found())?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response())
}
async fn logs(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    auth::context(&state, &headers).await?.owner()?;
    if id != "caddy" {
        apps::get(&state, &id).await?;
    }
    let mut output = String::new();
    for suffix in if id == "caddy" {
        vec![".log"]
    } else {
        vec![".stdout.log", ".stderr.log"]
    } {
        let path = state.0.data.join("logs").join(format!("{id}{suffix}"));
        if let Ok(mut file) = tokio::fs::File::open(path).await {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let size = file.metadata().await?.len();
            file.seek(std::io::SeekFrom::Start(size.saturating_sub(32768)))
                .await?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).await?;
            output.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    Ok(Json(json!({"text":output})))
}
pub async fn diagnostics(state: &State) -> Result<Value> {
    let settings = state.0.settings.read().await.clone();
    let url = url::Url::parse(&settings.public_url)?;
    let host = url.host_str().unwrap_or("localhost");
    let addresses = tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(443)))
        .await
        .map(|v| v.map(|a| a.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut ports = Vec::new();
    for port in [80u16, 443] {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await;
        ports.push(json!({"port":port,"local_listener":matches!(result,Ok(Ok(_)))}));
    }
    let admin = caddy::admin_origin(&settings)?;
    let caddy = platform::http()
        .get(format!("{admin}/config/"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    Ok(
        json!({"target":apps::target(),"data_dir":state.0.data.to_string_lossy(),"dns":addresses,"ports":ports,"caddy_reachable":caddy,"note":"Local checks cannot prove internet reachability. Point DNS at this computer’s public address and forward TCP 80/443 on your router. Networks behind CGNAT need a separately configured tunnel or reachable host."}),
    )
}
pub async fn backup(state: &State) -> Result<String> {
    snapshot(state, true).await
}
pub async fn snapshot(state: &State, resume: bool) -> Result<String> {
    let _guard = state.0.operations.lock().await;
    let enabled: Vec<String> = sqlx::query_scalar("SELECT id FROM apps WHERE enabled=1")
        .fetch_all(&state.0.db)
        .await?;
    for id in &enabled {
        apps::stop_inner(state, id).await?;
    }
    let result = async {
        let stage = state.0.data.join("staging").join(auth::id());
        tokio::fs::create_dir_all(&stage).await?;
        sqlx::query("VACUUM INTO ?")
            .bind(stage.join("loki.db").to_string_lossy().as_ref())
            .execute(&state.0.db)
            .await?;
        let source = state.0.data.clone();
        let output = stage.clone();
        let previous: Vec<(String, String)> =
            sqlx::query_as("SELECT id,previous FROM apps WHERE previous IS NOT NULL")
                .fetch_all(&state.0.db)
                .await?;
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            std::fs::copy(source.join("identity.pem"), output.join("identity.pem"))?;
            for name in ["apps", "app-data", "avatars", "caddy", "sync/blobs"] {
                apps::copy_tree(&source.join(name), &output.join(name))?;
            }
            for (id, path) in previous {
                apps::copy_tree(
                    std::path::Path::new(&path),
                    &output.join("rollback-data").join(id),
                )?;
            }
            Ok(())
        })
        .await??;
        let name = format!("loki-{}-{}.zip", auth::now(), auth::id());
        let dest = state.0.data.join("backups").join(&name);
        let snapshot = stage.clone();
        tokio::task::spawn_blocking(move || zip_tree(&snapshot, &dest)).await??;
        tokio::fs::remove_dir_all(stage).await?;
        Ok::<_, Error>(name)
    }
    .await;
    for id in enabled.into_iter().filter(|_| resume || result.is_err()) {
        if let Err(e) = apps::start_inner(state, &id).await {
            tracing::error!(app=%id,error=%e.message,"could not resume app after backup");
        }
    }
    let _ = caddy::apply(state).await;
    result
}
fn zip_tree(source: &std::path::Path, dest: &std::path::Path) -> anyhow::Result<()> {
    fn add(
        zip: &mut zip::ZipWriter<std::fs::File>,
        base: &std::path::Path,
        dir: &std::path::Path,
    ) -> anyhow::Result<()> {
        for item in std::fs::read_dir(dir)? {
            let item = item?;
            let path = item.path();
            if path.is_dir() {
                add(zip, base, &path)?;
            } else {
                let name = path
                    .strip_prefix(base)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated);
                #[cfg(unix)]
                let options = {
                    use std::os::unix::fs::PermissionsExt;
                    options.unix_permissions(path.metadata()?.permissions().mode())
                };
                zip.start_file(name, options)?;
                let mut file = std::fs::File::open(path)?;
                std::io::copy(&mut file, zip)?;
            }
        }
        Ok(())
    }
    let mut zip = zip::ZipWriter::new(std::fs::File::create(dest)?);
    add(&mut zip, source, source)?;
    zip.finish()?;
    Ok(())
}
async fn backup_download(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response> {
    auth::context(&state, &headers).await?.owner()?;
    if !apps::safe_relative(&id)
        || id.contains('/')
        || !id.starts_with("loki-")
        || !id.ends_with(".zip")
    {
        return Err(Error::not_found());
    }
    let file = tokio::fs::File::open(state.0.data.join("backups").join(&id)).await?;
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{id}\""),
        )
        .body(Body::from_stream(tokio_util::io::ReaderStream::new(file)))?)
}

async fn gateway_route(state: &State, id: &str) -> Result<String> {
    sqlx::query_scalar("SELECT hostname FROM routes WHERE id=? AND enabled=1 AND gated=1")
        .bind(id)
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(Error::not_found)
}
async fn gateway_check(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let host = gateway_route(&state, &id).await?;
    if auth::allowed(&state, None, &id, "access").await? {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let jar = CookieJar::from_headers(&headers);
    if let Some(cookie) = jar.get("__Host-loki_gateway") {
        let uid: Option<String> = sqlx::query_scalar(
            "SELECT user_id FROM gateway_sessions WHERE id=? AND resource=? AND expires>?",
        )
        .bind(auth::digest(cookie.value()))
        .bind(&id)
        .bind(auth::now())
        .fetch_optional(&state.0.db)
        .await?;
        if let Some(uid) = uid {
            let user: Option<auth::User> = sqlx::query_as("SELECT * FROM users WHERE id=?")
                .bind(uid)
                .fetch_optional(&state.0.db)
                .await?;
            if auth::allowed(&state, user.as_ref(), &id, "access").await? {
                return Ok(StatusCode::NO_CONTENT.into_response());
            }
        }
    }
    Ok(Redirect::temporary(&format!("https://{host}/_loki/start")).into_response())
}
async fn gateway_start(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Response> {
    let id = headers
        .get("x-loki-resource")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(Error::forbidden)?;
    let host = gateway_route(&state, id).await?;
    let raw = auth::random();
    let verifier = auth::random();
    sqlx::query("INSERT INTO gateway_flows(id,resource,verifier,expires) VALUES(?,?,?,?)")
        .bind(auth::digest(&raw))
        .bind(id)
        .bind(&verifier)
        .bind(auth::now() + 300)
        .execute(&state.0.db)
        .await?;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let issuer = state.0.settings.read().await.public_url.clone();
    let mut url = url::Url::parse(&format!("{issuer}/oidc/authorize"))?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", format!("gateway-{id}")),
        ("redirect_uri", format!("https://{host}/_loki/callback")),
        ("response_type", "code".into()),
        ("scope", "openid".into()),
        ("state", raw.clone()),
        ("nonce", auth::random()),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256".into()),
    ]);
    let cookie = Cookie::build(("__Host-loki_flow", raw))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::minutes(5))
        .build();
    Ok((
        CookieJar::from_headers(&headers).add(cookie),
        Redirect::to(url.as_str()),
    )
        .into_response())
}
#[derive(Deserialize)]
struct Callback {
    state: String,
    code: String,
}
async fn gateway_callback(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Result<Response> {
    let id = headers
        .get("x-loki-resource")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(Error::forbidden)?;
    let host = gateway_route(&state, id).await?;
    let jar = CookieJar::from_headers(&headers);
    if !jar
        .get("__Host-loki_flow")
        .is_some_and(|c| auth::equal(c.value(), &query.state))
    {
        return Err(Error::forbidden());
    }
    let mut tx = state.0.db.begin().await?;
    let verifier: String = sqlx::query_scalar(
        "DELETE FROM gateway_flows WHERE id=? AND resource=? AND expires>? RETURNING verifier",
    )
    .bind(auth::digest(&query.state))
    .bind(id)
    .bind(auth::now())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(Error::forbidden)?;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let uid:String=sqlx::query_scalar("DELETE FROM codes WHERE id=? AND client=? AND redirect=? AND challenge=? AND expires>? AND auth_version=(SELECT auth_version FROM users WHERE users.id=codes.user_id) RETURNING user_id").bind(auth::digest(&query.code)).bind(format!("gateway-{id}")).bind(format!("https://{host}/_loki/callback")).bind(challenge).bind(auth::now()).fetch_optional(&mut *tx).await?.ok_or_else(Error::forbidden)?;
    let raw = auth::random();
    sqlx::query("INSERT INTO gateway_sessions(id,resource,user_id,expires) VALUES(?,?,?,?)")
        .bind(auth::digest(&raw))
        .bind(id)
        .bind(&uid)
        .bind(auth::now() + 604800)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let user: auth::User = sqlx::query_as("SELECT * FROM users WHERE id=?")
        .bind(uid)
        .fetch_one(&state.0.db)
        .await?;
    if !auth::allowed(&state, Some(&user), id, "access").await? {
        return Err(Error::forbidden());
    }
    let cookie = Cookie::build(("__Host-loki_gateway", raw))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::days(7))
        .build();
    Ok((jar.add(cookie), Redirect::to("/")).into_response())
}
async fn backup_import(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<Value>> {
    let ctx = auth::context(&state, &headers).await?;
    let user = ctx.owner()?;
    let name = format!("loki-import-{}.zip", auth::id());
    let path = state.0.data.join("staging").join(&name);
    let result=async{
        let mut output=tokio::fs::File::create(&path).await?;let mut bytes=0u64;
        let mut field=multipart.next_field().await?.ok_or_else(||Error::bad("Select a backup archive."))?;
        while let Some(chunk)=field.chunk().await?{bytes+=chunk.len()as u64;if bytes>2147483648{return Err(Error::bad("Backup upload exceeds 2 GiB. Use the offline restore command for larger archives."));}output.write_all(&chunk).await?;}
        output.flush().await?;drop(output);
        let candidate=path.clone();tokio::task::spawn_blocking(move||crate::storage::validate_backup(&candidate)).await?.map_err(|_|Error::bad("This is not a valid LOKI backup archive."))?;
        tokio::fs::rename(&path,state.0.data.join("backups").join(&name)).await?;state.audit(&user.id,"backup-import",&name).await?;
        Ok::<_,Error>(Json(json!({"file":name})))
    }.await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(path).await;
    }
    result
}
