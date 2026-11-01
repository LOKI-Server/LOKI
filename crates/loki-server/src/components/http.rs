//! Serving an app's HTTP requests: routing by host, sign-in, forms, uploads, and streamed files.
use super::{FileSource, Runtime, component_log, enter, new_store, running};
use crate::{Error, Result, State, apps, auth, files, realtime};
use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};

use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use wasmtime_wasi_http::p2::{
    WasiHttpView,
    bindings::http::types::{ErrorCode, Scheme},
};
struct UploadCleanup(Vec<PathBuf>);
impl Drop for UploadCleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}
pub(super) async fn execute(
    _state: &State,
    runtime: Arc<Runtime>,
    mut request: Request<Body>,
    info: Value,
    form: Value,
    pending: Vec<PathBuf>,
) -> Result<Response> {
    let _cleanup = UploadCleanup(pending.clone());
    if !runtime.active.load(Ordering::SeqCst) {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This application is stopped.",
        ));
    }
    let limits = runtime.manifest.limits.clone();
    let mut store = new_store(&runtime, info, form, pending, None)?;
    store.set_fuel(limits.fuel)?;
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
    let run = async {
        let proxy = runtime.pre.instantiate_async(&mut store).await?;
        let call = proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut store, incoming, out);
        tokio::pin!(call);
        // Read the response concurrently, so WASI stream backpressure cannot deadlock.
        let response = async {
            let response = receiver
                .await
                .map_err(|_| anyhow::anyhow!("Component did not return a response"))?
                .map_err(|e| anyhow::anyhow!("Component HTTP error: {e:?}"))?;
            let (parts, body) = response.into_parts();
            let bytes = http_body_util::Limited::new(body, 8 * 1024 * 1024)
                .collect()
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .to_bytes();
            Ok::<_, anyhow::Error>(Response::from_parts(parts, Body::from(bytes)))
        };
        let (_, response) =
            tokio::try_join!(async { call.await.map_err(anyhow::Error::from) }, response)?;
        Ok::<_, anyhow::Error>(response)
    };
    let result = tokio::time::timeout(Duration::from_secs(limits.timeout_seconds), run).await;
    let mut response = match result {
        Ok(Ok(response)) => response,
        failed => {
            let detail = match failed {
                Ok(Err(e)) => format!("{e:#}"),
                _ => "Component request exceeded its time limit".into(),
            };
            component_log(&runtime, &detail);
            return Err(Error::new(
                StatusCode::BAD_GATEWAY,
                "The application request failed or exceeded its resource limit. LOKI is still available.",
            ));
        }
    };
    if let Some((path, asset, options)) = store.data_mut().file.take()
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
async fn stream_file(
    runtime: &Runtime,
    path: &str,
    source: FileSource,
    options: &str,
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
    let options: Value = serde_json::from_str(options)?;
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
        let filename = options["filename"].as_str().unwrap_or("download");
        let encoded = url::form_urlencoded::byte_serialize(filename.as_bytes())
            .collect::<String>()
            .replace('+', "%20");
        let download = options["download"].as_bool().unwrap_or(false)
            || files::preview_type(relative).is_none();
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
    for app in candidates {
        if serde_json::from_str::<apps::Manifest>(&app.manifest)?.runtime == "wasi" {
            return Ok(Some(app));
        }
    }
    Ok(None)
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
        return login_start(state, &app, request.headers()).await;
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
        && path == settings.path
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
        return realtime::upgrade(state, &app.id, request, user).await;
    }
    if !allowed {
        return if user.is_none() {
            Ok(Redirect::to("/_loki/start").into_response())
        } else {
            Err(Error::forbidden())
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
    let _permit = enter(state, &runtime).await?;
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
        return Ok(Redirect::to(&dashboard_url(state, &origin).await).into_response());
    }
    request.headers_mut().remove("x-loki-head");
    if method == axum::http::Method::HEAD {
        request
            .headers_mut()
            .insert("x-loki-head", "1".parse().unwrap());
    }
    let mut response = execute(state, runtime, request, info, form, pending).await?;
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
    let _permit = enter(state, &runtime).await?;
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
    let response = execute(state, runtime, request, info, Value::Null, pending).await?;
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
async fn login_start(state: &State, app: &apps::App, headers: &HeaderMap) -> Result<Response> {
    use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    let raw = auth::random();
    let verifier = auth::random();
    let origin = apps::public_url(app);
    sqlx::query("INSERT INTO gateway_flows(id,resource,verifier,expires) VALUES(?,?,?,?)")
        .bind(auth::digest(&raw))
        .bind(&app.id)
        .bind(&verifier)
        .bind(auth::now() + 300)
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
    let verifier: String = sqlx::query_scalar(
        "DELETE FROM gateway_flows WHERE id=? AND resource=? AND expires>? RETURNING verifier",
    )
    .bind(auth::digest(raw))
    .bind(&app.id)
    .bind(auth::now())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(Error::forbidden)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let uid:String=sqlx::query_scalar("DELETE FROM codes WHERE id=? AND client=? AND redirect=? AND challenge=? AND expires>? AND auth_version=(SELECT auth_version FROM users WHERE users.id=codes.user_id) RETURNING user_id").bind(auth::digest(code)).bind(format!("component-{}",app.id)).bind(format!("{origin}/_loki/callback")).bind(challenge).bind(auth::now()).fetch_optional(&mut *tx).await?.ok_or_else(Error::forbidden)?;
    let session = auth::random();
    sqlx::query("INSERT INTO gateway_sessions(id,resource,user_id,expires) SELECT ?,?,?,? WHERE EXISTS(SELECT 1 FROM users WHERE id=? AND status='approved')").bind(auth::digest(&session)).bind(&app.id).bind(&uid).bind(auth::now()+604800).bind(&uid).execute(&mut *tx).await?;
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
        .max_age(time::Duration::days(7))
        .build();
    Ok((
        jar.remove(Cookie::build((flow_name, "")).path("/").build())
            .add(cookie),
        Redirect::to("/"),
    )
        .into_response())
}
