//! Serving an app's HTTP requests: routing by host, sign-in, forms, uploads, and streamed files.
use super::{
    FileSource, Host, Permit, Runtime, ServeOptions, component_log, enter, new_store, running,
    with_runtime,
};
use crate::{Error, Result, State, apps, auth, files, realtime};
use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};

use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use wasmtime::Store;
use wasmtime_wasi_http::p2::{
    WasiHttpView,
    bindings::{
        Proxy,
        http::types::{ErrorCode, Scheme},
    },
};
struct UploadCleanup(Vec<PathBuf>);
impl Drop for UploadCleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}
/// A file `serve-file` chose to answer with: its path, where it lives, and how.
pub(crate) type ServedFile = Arc<std::sync::Mutex<Option<(String, FileSource, ServeOptions)>>>;
/// An idle instance kept for an app's next request.
pub(crate) struct Warm {
    store: Store<Host>,
    proxy: Proxy,
    uses: u32,
    _slot: tokio::sync::OwnedSemaphorePermit,
}
/// Requests one kept instance serves before it is replaced, bounding leaks.
const WARM_USES: u32 = 1000;
/// The most a streamed response body may carry.
const RESPONSE_BYTES: usize = 1024 * 1024 * 1024;

/// Runs one request in an instance of the app. The response streams as the app writes
/// it; the instance and `permit` stay held until the body ends or the time limit passes.
pub(super) async fn execute(
    _state: &State,
    runtime: Arc<Runtime>,
    mut request: Request<Body>,
    info: Value,
    form: Value,
    pending: Vec<PathBuf>,
    permit: Permit,
) -> Result<Response> {
    let cleanup = UploadCleanup(pending.clone());
    if !runtime.active.load(Ordering::SeqCst) {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This application is stopped.",
        ));
    }
    let limits = runtime.manifest.limits.clone();
    // Health checks always get a fresh instance, so they prove the app can start.
    let reuse = limits.reuse_instances && request.uri().path() != runtime.manifest.health;
    let warm = if reuse {
        runtime.warm.lock().unwrap().pop()
    } else {
        None
    };
    let (mut store, proxy, uses) = match warm {
        Some(warm) => {
            let mut store = warm.store;
            let host = store.data_mut();
            host.info = with_runtime(&runtime, info);
            host.form = form;
            host.pending = pending;
            (store, Some(warm.proxy), warm.uses)
        }
        None => (new_store(&runtime, info, form, pending, None)?, None, 0),
    };
    store.set_fuel(limits.fuel)?;
    let served = store.data().file.clone();
    // Authentication credentials stay in the host; identity comes from context.
    request.headers_mut().remove(header::COOKIE);
    request.headers_mut().remove(header::AUTHORIZATION);
    let headers = request.headers().clone();
    let request = request.map(|body| {
        http_body_util::Limited::new(body, limits.request_bytes)
            .map_err(|_| ErrorCode::HttpRequestBodySize(None))
    });
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let incoming = store.data_mut().http().new_incoming_request(
        if runtime.origin.starts_with("https:") {
            Scheme::Https
        } else {
            Scheme::Http
        },
        request,
    )?;
    let out = store.data_mut().http().new_response_outparam(sender)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(limits.timeout_seconds);
    let (done, finished) = tokio::sync::oneshot::channel();
    let task_runtime = runtime.clone();
    // The guest writes its body while it runs, so it runs beside the response.
    tokio::spawn(async move {
        let _permit = permit;
        let _cleanup = cleanup;
        let runtime = task_runtime;
        let result = tokio::time::timeout_at(deadline, async {
            let proxy = match proxy {
                Some(proxy) => proxy,
                None => runtime.pre.instantiate_async(&mut store).await?,
            };
            proxy
                .wasi_http_incoming_handler()
                .call_handle(&mut store, incoming, out)
                .await?;
            Ok::<_, anyhow::Error>(proxy)
        })
        .await;
        let result = match result {
            Ok(Ok(proxy)) => {
                if reuse && uses + 1 < WARM_USES {
                    keep(&runtime, store, proxy, uses + 1);
                }
                Ok(())
            }
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err("Component request exceeded its time limit".to_string()),
        };
        if let Err(detail) = &result {
            component_log(&runtime, detail);
        }
        let _ = done.send(result);
    });
    let response = match receiver.await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            component_log(&runtime, &format!("Component HTTP error: {e:?}"));
            return Err(failed());
        }
        // The instance ended without answering; a failure has been logged already.
        Err(_) => {
            if let Ok(Ok(())) = finished.await {
                component_log(&runtime, "Component did not return a response");
            }
            return Err(failed());
        }
    };
    let (parts, body) = response.into_parts();
    let mut response = Response::from_parts(parts, buffer(body).await.ok_or_else(failed)?);
    let file = served.lock().unwrap().take();
    if let Some((path, asset, options)) = file
        && response.status().is_success()
    {
        response = stream_file(&runtime, &path, asset, &options, &headers).await?;
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    response.headers_mut().remove(header::SET_COOKIE);
    Ok(response)
}
/// Waits briefly for the body to finish, so an ordinary response is sent whole with
/// its length. A large or slow one streams instead. None when the app failed first.
async fn buffer<B>(mut body: B) -> Option<Body>
where
    B: http_body::Body<Data = bytes::Bytes> + Send + Unpin + 'static,
    B::Error: Into<axum::BoxError>,
{
    let mut prefix = bytes::BytesMut::new();
    let wait = tokio::time::sleep(Duration::from_millis(100));
    tokio::pin!(wait);
    loop {
        tokio::select! {
            frame = body.frame() => match frame {
                None => return Some(Body::from(prefix.freeze())),
                Some(Err(_)) => return None,
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        prefix.extend_from_slice(&data);
                    }
                    if prefix.len() > 64 * 1024 {
                        break;
                    }
                }
            },
            () = &mut wait => break,
        }
    }
    let first = futures_util::stream::once(std::future::ready(Ok::<_, axum::BoxError>(
        http_body::Frame::data(prefix.freeze()),
    )));
    let rest = http_body_util::BodyStream::new(http_body_util::Limited::new(body, RESPONSE_BYTES));
    Some(Body::new(http_body_util::StreamBody::new(first.chain(rest))))
}
fn failed() -> Error {
    Error::new(
        StatusCode::BAD_GATEWAY,
        "The application request failed or exceeded its resource limit. LOKI is still available.",
    )
}
/// Keeps a finished instance for the next request, if LOKI has room for it.
fn keep(runtime: &Arc<Runtime>, mut store: Store<Host>, proxy: Proxy, uses: u32) {
    if !runtime.active.load(Ordering::SeqCst) {
        return;
    }
    let Ok(slot) = runtime.warm_slots.clone().try_acquire_owned() else {
        return;
    };
    let mut warm = runtime.warm.lock().unwrap();
    if warm.len() >= runtime.manifest.limits.concurrent_requests {
        return;
    }
    // Uploads were retained or removed with the request.
    store.data_mut().pending.clear();
    *store.data().file.lock().unwrap() = None;
    warm.push(Warm {
        store,
        proxy,
        uses,
        _slot: slot,
    });
}
async fn stream_file(
    runtime: &Runtime,
    path: &str,
    source: FileSource,
    options: &ServeOptions,
    headers: &HeaderMap,
) -> Result<Response> {
    let asset = matches!(source, FileSource::Asset);
    let relative = std::path::Path::new(path);
    let file = if matches!(source, FileSource::Storage) {
        runtime
            .storage
            .as_ref()
            .ok_or_else(Error::forbidden)?
            .open_file(path)
            .await?
    } else {
        let root = if asset {
            runtime.package.clone()
        } else {
            runtime.data.join("uploads")
        };
        let (dir, relative) = files::confined(&root.to_string_lossy(), path)?;
        dir.open(&relative).map_err(|_| Error::not_found())?
    };
    if !file.metadata()?.is_file() {
        return Err(Error::not_found());
    }
    let length = file.metadata()?.len();
    let mut status = StatusCode::OK;
    let (start, end) = if let Some(range) = headers.get(header::RANGE) {
        if let Some(range) = range.to_str().ok().and_then(|r| files::range(r, length)) {
            status = StatusCode::PARTIAL_CONTENT;
            range
        } else {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{length}"))],
            )
                .into_response());
        }
    } else {
        (0, length.saturating_sub(1))
    };
    let count = if length == 0 { 0 } else { end - start + 1 };
    let mut file = tokio::fs::File::from_std(file.into_std());
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, count)
        .header(header::ACCEPT_RANGES, "bytes");
    let content_type = if asset {
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string()
    } else {
        files::preview_type(relative).unwrap_or_else(|| "application/octet-stream".into())
    };
    response = response.header(header::CONTENT_TYPE, content_type);
    if !asset {
        let filename = options.filename.as_deref().unwrap_or("download");
        let encoded = url::form_urlencoded::byte_serialize(filename.as_bytes())
            .collect::<String>()
            .replace('+', "%20");
        let download = options.download || files::preview_type(relative).is_none();
        response = response.header(
            header::CONTENT_DISPOSITION,
            format!(
                "{}; filename=\"download\"; filename*=UTF-8''{encoded}",
                if download { "attachment" } else { "inline" }
            ),
        );
        if download {
            response = response.header("Content-Security-Policy", "sandbox; default-src 'none'");
        }
    }
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{length}"),
        );
    }
    let body = if headers.get("x-loki-head").is_some() {
        Body::empty()
    } else {
        Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
            file.take(count),
            65536,
        ))
    };
    Ok(response.body(body)?)
}

pub async fn resolve(state: &State, headers: &HeaderMap) -> Result<Option<apps::App>> {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return Ok(None);
    };
    let url = url::Url::parse(&format!("http://{host}"))?;
    let hostname = url.host_str().unwrap_or("");
    let local = hostname.strip_suffix(".localhost");
    let candidates: Vec<apps::App> = if let Some(id) = local {
        if !apps::valid_id(id) || url.port_or_known_default() != Some(state.0.bind.port()) {
            return Ok(None);
        }
        sqlx::query_as("SELECT * FROM apps WHERE id=? AND hostname='' ")
            .bind(id)
            .fetch_all(&state.0.db)
            .await?
    } else {
        sqlx::query_as("SELECT * FROM apps WHERE hostname=? AND hostname!=''")
            .bind(hostname)
            .fetch_all(&state.0.db)
            .await?
    };
    Ok(candidates.into_iter().next())
}
fn session_cookie(origin: &str) -> &'static str {
    if origin.starts_with("https:") {
        "__Host-loki_component"
    } else {
        "loki_component"
    }
}
pub async fn serve(state: &State, app: apps::App, mut request: Request<Body>) -> Result<Response> {
    let origin = apps::public_url(&app);
    let path = request.uri().path().to_string();
    if !app.enabled {
        return Err(Error::not_found());
    }
    if path == "/_loki/start" {
        let query = request.uri().query().unwrap_or("").to_string();
        return login_start(state, &app, &query, request.headers()).await;
    }
    if path == "/_loki/signed-out" {
        return sign_in_page(state, &app, "/", true).await;
    }
    if path == "/_loki/callback" {
        return login_callback(state, &app, request).await;
    }
    // The rest of /_loki/ is LOKI's own conversation with the app, such as its panel.
    if path.starts_with("/_loki/") {
        return Err(Error::not_found());
    }
    // A device signed in with a code sends its access token instead of a cookie.
    let bearer = match request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        Some(token) => {
            crate::identity::token_user(state, token, Some(&format!("component-{}", app.id)))
                .await?
                .map(|(user, ..)| user)
        }
        None => None,
    };
    let jar = axum_extra::extract::cookie::CookieJar::from_headers(request.headers());
    let raw = jar
        .get(session_cookie(&origin))
        .map(|c| c.value().to_string())
        .filter(|_| bearer.is_none());
    let uid = if let Some(raw) = &raw {
        sqlx::query_scalar::<_, String>(
            "SELECT user_id FROM gateway_sessions WHERE id=? AND resource=? AND expires>?",
        )
        .bind(auth::digest(raw))
        .bind(&app.id)
        .bind(auth::now())
        .fetch_optional(&state.0.db)
        .await?
    } else {
        None
    };
    let user = if let Some(uid) = uid {
        sqlx::query_as::<_, auth::User>("SELECT * FROM users WHERE id=? AND status='approved'")
            .bind(uid)
            .fetch_optional(&state.0.db)
            .await?
    } else {
        bearer.clone()
    };
    let manifest: apps::Manifest = serde_json::from_str(&app.manifest)?;
    let allowed = auth::allowed(state, user.as_ref(), &app.id, "access").await?;
    // Realtime sockets are open to anyone who can reach the app, so players need no
    // LOKI account. A signed-in, same-origin browser also says who it is.
    if let Some(settings) = manifest.realtime()
        && let Some(room) = realtime::room_of(&settings, &path)
        && request.method() == axum::http::Method::GET
        && realtime::is_upgrade(request.headers())
    {
        let same_origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|o| o == origin);
        let user = match user
            .as_ref()
            .filter(|_| allowed && (same_origin || bearer.is_some()))
        {
            Some(user) => user_info(state, user, &app.id, &manifest).await?,
            None => Value::Null,
        };
        return realtime::upgrade(state, &app.id, room, request, user, true).await;
    }
    if !allowed {
        // A browser gets a page of the app's own to sign in from, not an instant
        // trip to LOKI, so signing out of the app sticks until you sign in again.
        return match &user {
            None if opens_page(&request) => {
                let here = safe_next(request.uri().path_and_query().map_or("/", |p| p.as_str()));
                sign_in_page(state, &app, &here, false).await
            }
            None => Ok(Redirect::to("/_loki/start").into_response()),
            Some(_) => Err(Error::forbidden()),
        };
    }
    let csrf = raw
        .as_ref()
        .map(|r| auth::digest(&format!("component-csrf:{r}")))
        .unwrap_or_default();
    let user_value = match &user {
        Some(user) => user_info(state, user, &app.id, &manifest).await?,
        None => Value::Null,
    };
    let info = json!({"user":user_value,"csrf":csrf,"issuer":state.0.settings.read().await.public_url,"dashboard_url":dashboard_url(state,&origin).await});
    let runtime = running(state, &app.id).await?;
    let permit = enter(state, &runtime).await?;
    let mut form = Value::Null;
    let mut pending = Vec::new();
    let method = request.method().clone();
    if !matches!(
        method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    ) {
        if user.is_none() {
            return Err(Error::unauthorized());
        }
        // Bearer tokens are never sent by browsers on their own, so need no CSRF token.
        let cookie = bearer.is_none();
        if cookie
            && request
                .headers()
                .get(header::ORIGIN)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|o| o != origin)
        {
            return Err(Error::forbidden());
        }
        if manifest.capabilities.iter().any(|c| c == "forms")
            && request
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|t| {
                    t.starts_with("application/x-www-form-urlencoded")
                        || t.starts_with("multipart/form-data")
                })
        {
            let (parts, body) = request.into_parts();
            let parsed = tokio::time::timeout(
                Duration::from_secs(manifest.limits.timeout_seconds),
                parse_form(&runtime, &parts.headers, body),
            )
            .await
            .map_err(|_| Error::bad("Upload timed out."))??;
            form = parsed.0;
            pending = parsed.1;
            let mut cleanup = UploadCleanup(pending.clone());
            if cookie && !auth::equal(form["fields"]["csrf_token"].as_str().unwrap_or(""), &csrf) {
                return Err(Error::forbidden());
            }
            cleanup.0.clear();
            request = Request::from_parts(parts, Body::empty());
        } else if cookie
            && !auth::equal(
                request
                    .headers()
                    .get("x-csrf-token")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or(""),
                &csrf,
            )
        {
            return Err(Error::forbidden());
        }
    }
    if path == "/logout" && method == axum::http::Method::POST {
        if let Some(raw) = raw {
            sqlx::query("DELETE FROM gateway_sessions WHERE id=? AND resource=?")
                .bind(auth::digest(&raw))
                .bind(&app.id)
                .execute(&state.0.db)
                .await?;
        }
        return Ok(Redirect::to("/_loki/signed-out").into_response());
    }
    request.headers_mut().remove("x-loki-head");
    if method == axum::http::Method::HEAD {
        request
            .headers_mut()
            .insert("x-loki-head", "1".parse().unwrap());
    }
    let mut response = execute(state, runtime, request, info, form, pending, permit).await?;
    if method == axum::http::Method::HEAD {
        *response.body_mut() = Body::empty();
    }
    Ok(response)
}

/// The signed-in user as the guest sees them, with the app roles they hold.
async fn user_info(
    state: &State,
    user: &auth::User,
    app: &str,
    manifest: &apps::Manifest,
) -> Result<Value> {
    let mut roles = vec!["access"];
    for role in &manifest.roles {
        if auth::allowed(state, Some(user), app, role).await? {
            roles.push(role.as_str());
        }
    }
    Ok(
        json!({"id":user.id,"username":user.username,"name":user.display_name,"email":user.email,"bio":user.bio,"avatar":user.avatar,"roles":roles}),
    )
}
/// Asks the app for its dashboard panel, sending one action first when given. Uploads
/// come as a multipart form; buttons and forms as JSON.
pub async fn panel(
    state: &State,
    app: &str,
    user: &auth::User,
    request: Option<Request<Body>>,
) -> Result<Value> {
    let runtime = running(state, app).await?;
    if !runtime.manifest.panel || !auth::allowed(state, Some(user), app, "access").await? {
        return Err(Error::forbidden());
    }
    let mut info = json!({"user":user_info(state, user, app, &runtime.manifest).await?,"csrf":"","issuer":state.0.settings.read().await.public_url});
    let permit = enter(state, &runtime).await?;
    let mut pending = Vec::new();
    let method = if let Some(request) = request {
        let (parts, body) = request.into_parts();
        let multipart = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|t| t.starts_with("multipart/form-data"));
        let (mut fields, files) = if multipart {
            let (form, files) = tokio::time::timeout(
                Duration::from_secs(runtime.manifest.limits.timeout_seconds),
                parse_form(&runtime, &parts.headers, body),
            )
            .await
            .map_err(|_| Error::bad("Upload timed out."))??;
            pending = files;
            (form["fields"].clone(), form["files"].clone())
        } else {
            let bytes = axum::body::to_bytes(body, 65536)
                .await
                .map_err(|_| Error::bad("Panel action is too large."))?;
            let value: Value = serde_json::from_slice(&bytes)?;
            let mut fields = value["fields"].clone();
            if !fields.is_object() {
                fields = json!({});
            }
            fields["action"] = value["action"].clone();
            fields["value"] = value["value"].clone();
            (fields, json!([]))
        };
        let text = |key: &str| fields[key].as_str().unwrap_or("").to_string();
        let (action, value) = (text("action"), text("value"));
        if action.is_empty() || action.len() > 64 {
            return Err(Error::bad("Choose a panel action."));
        }
        if let Some(fields) = fields.as_object_mut() {
            fields.remove("action");
            fields.remove("value");
            if fields.values().any(|v| !v.is_string()) {
                return Err(Error::bad("Panel fields must be text."));
            }
        }
        info["panel_action"] = json!({"action":action,"value":value,"fields":fields,"files":files});
        state
            .audit(&user.id, "app-panel-action", &format!("{app}: {action}"))
            .await?;
        axum::http::Method::POST
    } else {
        axum::http::Method::GET
    };
    let request = Request::builder()
        .method(method)
        .uri("/_loki/panel")
        .header(header::HOST, url::Url::parse(&runtime.origin)?.authority())
        .body(Body::empty())?;
    let response = execute(state, runtime, request, info, Value::Null, pending, permit).await?;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .map_err(|_| Error::new(StatusCode::BAD_GATEWAY, "The app's panel is too large."))?;
    if !status.is_success() {
        // The app's own error message, such as a missing role, when it gave one.
        let message = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| "The app could not show its panel.".into());
        return Err(Error::new(status, message));
    }
    let panel: loki_sdk::panel::PanelResponse = serde_json::from_slice(&bytes)
        .ok()
        .filter(loki_sdk::panel::PanelResponse::valid)
        .ok_or_else(|| {
            Error::new(
                StatusCode::BAD_GATEWAY,
                "The app sent a panel LOKI can't show.",
            )
        })?;
    Ok(serde_json::to_value(panel)?)
}
async fn parse_form(
    runtime: &Runtime,
    headers: &HeaderMap,
    body: Body,
) -> Result<(Value, Vec<PathBuf>)> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("application/x-www-form-urlencoded") {
        let bytes = axum::body::to_bytes(body, 65536)
            .await
            .map_err(|_| Error::bad("Form is too large."))?;
        let mut fields = serde_json::Map::new();
        for (key, value) in url::form_urlencoded::parse(&bytes) {
            if fields.insert(key.into_owned(), json!(value)).is_some() {
                return Err(Error::bad("Duplicate form field."));
            }
        }
        return Ok((json!({"fields":fields,"files":[]}), Vec::new()));
    }
    if !runtime.manifest.capabilities.iter().any(|c| c == "uploads") {
        return Err(Error::forbidden());
    }
    let boundary =
        multer::parse_boundary(content_type).map_err(|_| Error::bad("Invalid multipart form."))?;
    let max = runtime.config["max-upload"]
        .as_u64()
        .unwrap_or(runtime.manifest.limits.request_bytes as u64)
        .min(runtime.manifest.limits.request_bytes as u64);
    let constraints = multer::Constraints::new().size_limit(
        multer::SizeLimit::new()
            .whole_stream(runtime.manifest.limits.request_bytes as u64)
            .per_field(max),
    );
    let mut multipart =
        multer::Multipart::with_constraints(body.into_data_stream(), boundary, constraints);
    let mut cleanup = UploadCleanup(Vec::new());
    let mut fields = serde_json::Map::new();
    let mut uploads = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| Error::bad("Upload exceeds its limit or is malformed."))?
    {
        let name = field.name().unwrap_or("").to_string();
        if seen.len() >= 64 || name.len() > 64 || !seen.insert(name.clone()) {
            return Err(Error::bad("Too many or duplicate form fields."));
        }
        if let Some(filename) = field
            .file_name()
            .filter(|n| !n.is_empty())
            .map(str::to_string)
        {
            if uploads.len() >= 8 || filename.len() > 1024 {
                return Err(Error::bad("Too many files or filename too long."));
            }
            let token = format!(".pending-{}", auth::id());
            let path = runtime.data.join("uploads").join(&token);
            let content_type = field
                .content_type()
                .map(ToString::to_string)
                .unwrap_or_else(|| "application/octet-stream".into());
            let mut output = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await?;
            cleanup.0.push(path);
            let mut size = 0u64;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|_| Error::bad("Upload exceeds its limit or is malformed."))?
            {
                size += chunk.len() as u64;
                if size > max {
                    return Err(Error::bad("File exceeds the configured upload limit."));
                }
                output.write_all(&chunk).await?;
            }
            output.flush().await?;
            uploads.push(json!({"field":name,"name":filename,"token":token,"content_type":content_type,"size":size}));
        } else {
            let mut text = Vec::new();
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|_| Error::bad("Invalid form field."))?
            {
                if text.len() + chunk.len() > 65536 {
                    return Err(Error::bad("Form field is too large."));
                }
                text.extend_from_slice(&chunk);
            }
            fields.insert(
                name,
                json!(
                    String::from_utf8(text)
                        .map_err(|_| Error::bad("Form fields must be UTF-8."))?
                ),
            );
        }
    }
    let pending = std::mem::take(&mut cleanup.0);
    Ok((json!({"fields":fields,"files":uploads}), pending))
}

async fn dashboard_url(state: &State, origin: &str) -> String {
    if origin.starts_with("http:") {
        format!("http://127.0.0.1:{}", state.0.bind.port())
    } else {
        state.0.settings.read().await.public_url.clone()
    }
}
/// Starts signing in at LOKI. `next` is where to return; `popup` means a small
/// sign-in window opened by the app's page.
async fn login_start(
    state: &State,
    app: &apps::App,
    query: &str,
    headers: &HeaderMap,
) -> Result<Response> {
    use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    let query: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
    let next = safe_next(query.get("next").map_or("/", String::as_str));
    let popup = query.get("popup").is_some_and(|v| v == "1");
    let raw = auth::random();
    let verifier = auth::random();
    let origin = apps::public_url(app);
    sqlx::query(
        "INSERT INTO gateway_flows(id,resource,verifier,expires,next,popup) VALUES(?,?,?,?,?,?)",
    )
    .bind(auth::digest(&raw))
    .bind(&app.id)
    .bind(&verifier)
    .bind(auth::now() + 300)
    .bind(&next)
    .bind(popup)
    .execute(&state.0.db)
    .await?;
    let mut url = url::Url::parse(&format!(
        "{}/oidc/authorize",
        dashboard_url(state, &origin).await
    ))?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", format!("component-{}", app.id)),
        ("redirect_uri", format!("{origin}/_loki/callback")),
        ("response_type", "code".into()),
        ("scope", "openid".into()),
        ("state", raw.clone()),
        ("nonce", auth::random()),
        (
            "code_challenge",
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        ),
        ("code_challenge_method", "S256".into()),
    ]);
    let name = if origin.starts_with("https:") {
        "__Host-loki_component_flow"
    } else {
        "loki_component_flow"
    };
    let cookie = Cookie::build((name, raw))
        .http_only(true)
        .secure(origin.starts_with("https:"))
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::minutes(5))
        .build();
    Ok((
        CookieJar::from_headers(headers).add(cookie),
        Redirect::to(url.as_str()),
    )
        .into_response())
}
async fn login_callback(
    state: &State,
    app: &apps::App,
    request: Request<Body>,
) -> Result<Response> {
    use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    let query: std::collections::HashMap<_, _> =
        url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let raw = query.get("state").ok_or_else(Error::forbidden)?;
    let code = query.get("code").ok_or_else(Error::forbidden)?;
    let origin = apps::public_url(app);
    let jar = CookieJar::from_headers(request.headers());
    let flow_name = if origin.starts_with("https:") {
        "__Host-loki_component_flow"
    } else {
        "loki_component_flow"
    };
    if !jar
        .get(flow_name)
        .is_some_and(|c| auth::equal(c.value(), raw))
    {
        return Err(Error::forbidden());
    }
    let mut tx = state.0.db.begin().await?;
    let (verifier, next, popup): (String, String, bool) = sqlx::query_as(
        "DELETE FROM gateway_flows WHERE id=? AND resource=? AND expires>? RETURNING verifier,next,popup",
    )
    .bind(auth::digest(raw))
    .bind(&app.id)
    .bind(auth::now())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(Error::forbidden)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let (uid, login_session): (String, Option<String>) = sqlx::query_as("DELETE FROM codes WHERE id=? AND client=? AND redirect=? AND challenge=? AND expires>? AND auth_version=(SELECT auth_version FROM users WHERE users.id=codes.user_id) RETURNING user_id,login_session").bind(auth::digest(code)).bind(format!("component-{}",app.id)).bind(format!("{origin}/_loki/callback")).bind(challenge).bind(auth::now()).fetch_optional(&mut *tx).await?.ok_or_else(Error::forbidden)?;
    let session = auth::random();
    let expires =
        crate::auth::app_session(&mut tx, &session, &app.id, &uid, login_session.as_deref())
            .await?;
    tx.commit().await?;
    let user: auth::User = sqlx::query_as("SELECT * FROM users WHERE id=?")
        .bind(&uid)
        .fetch_one(&state.0.db)
        .await?;
    if !auth::allowed(state, Some(&user), &app.id, "access").await? {
        return Err(Error::forbidden());
    }
    let cookie = Cookie::build((session_cookie(&origin), session))
        .http_only(true)
        .secure(origin.starts_with("https:"))
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::seconds(expires - auth::now()))
        .build();
    let jar = jar
        .remove(Cookie::build((flow_name, "")).path("/").build())
        .add(cookie);
    let next = safe_next(&next);
    if !popup {
        return Ok((jar, Redirect::to(&next)).into_response());
    }
    let dashboard = dashboard_url(state, &origin).await;
    let nonce = auth::random();
    let page = SignedInPage {
        app: resource_name(state, &app.id).await?,
        dashboard: dashboard.clone(),
        assets: crate::assets::base(),
        // JSON in a script must not be able to close the element.
        next_json: serde_json::to_string(&next)?
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026"),
        next,
        nonce: nonce.clone(),
    };
    Ok((
        jar,
        host_page(askama::Template::render(&page)?, &nonce, &dashboard),
    )
        .into_response())
}
/// LOKI's own pages on an app's address: signing in, and having signed out.
#[derive(askama::Template)]
#[template(path = "app_sign_in.html")]
struct SignInPage {
    app: String,
    server: String,
    dashboard: String,
    assets: &'static str,
    /// Where to return after signing in, URL-encoded.
    next: String,
    signed_out: bool,
    nonce: String,
}
/// Ends a sign-in made in a small window by handing the session to the app's page.
#[derive(askama::Template)]
#[template(path = "app_signed_in.html")]
struct SignedInPage {
    app: String,
    dashboard: String,
    assets: &'static str,
    next: String,
    next_json: String,
    nonce: String,
}
/// A page LOKI draws on an app's address. Only its own inline script may run.
fn host_page(html: String, nonce: &str, dashboard: &str) -> Response {
    let policy = format!(
        "default-src 'none'; style-src {dashboard}; img-src {dashboard}; font-src {dashboard}; script-src 'nonce-{nonce}'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    let mut response = axum::response::Html(html).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    if let Ok(policy) = policy.parse() {
        headers.insert(header::CONTENT_SECURITY_POLICY, policy);
    }
    response
}
async fn sign_in_page(
    state: &State,
    app: &apps::App,
    next: &str,
    signed_out: bool,
) -> Result<Response> {
    let origin = apps::public_url(app);
    let dashboard = dashboard_url(state, &origin).await;
    let nonce = auth::random();
    let page = SignInPage {
        app: resource_name(state, &app.id).await?,
        server: state.0.settings.read().await.name.clone(),
        dashboard: dashboard.clone(),
        assets: crate::assets::base(),
        next: url::form_urlencoded::byte_serialize(next.as_bytes()).collect(),
        signed_out,
        nonce: nonce.clone(),
    };
    Ok(host_page(
        askama::Template::render(&page)?,
        &nonce,
        &dashboard,
    ))
}
async fn resource_name(state: &State, id: &str) -> Result<String> {
    Ok(sqlx::query_scalar("SELECT name FROM resources WHERE id=?")
        .bind(id)
        .fetch_one(&state.0.db)
        .await?)
}
/// A path on the app to return to after signing in. Anything else is `/`.
fn safe_next(value: &str) -> String {
    let safe = value.starts_with('/')
        && !value.starts_with("//")
        && !value.starts_with("/_loki/")
        && value.len() <= 2048
        && !value.chars().any(|c| c == '\\' || c.is_control());
    if safe { value.into() } else { "/".into() }
}
/// Whether a browser is opening a page, rather than a script fetching data.
fn opens_page(request: &Request<Body>) -> bool {
    matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) && request
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"))
}
