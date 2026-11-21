use super::{Access, PROTOCOL_PATH, credentials, host, invalid};
use crate::{Error, Result, State, auth};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{DefaultBodyLimit, Path, Query, Request, State as ExtractState},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use skald_vcs_core::{
    Id, MAX_OBJECT, VERSION,
    protocol::{MAX_NEGOTIATION, Objects, Update},
};

pub(super) fn router() -> Router<State> {
    Router::new()
        .route(PROTOCOL_PATH, get(discovery))
        .nest(PROTOCOL_PATH, repositories())
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn(policy))
}

fn repositories() -> Router<State> {
    Router::new()
        .route("/repos", get(list))
        .route("/repos/{id}", get(metadata))
        .route("/repos/{id}/refs", get(refs))
        .route("/repos/{id}/history", get(history))
        .route("/repos/{id}/missing", post(missing))
        .route("/repos/{id}/objects/{object}", get(download).put(upload))
        .route("/repos/{id}/refs/{kind}/{name}", put(update))
        .route("/repos/{id}/locks", get(locks).put(lock))
}

async fn policy(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    response
}

async fn discovery() -> Json<Value> {
    Json(
        json!({"version":VERSION,"repository_discovery":format!("{PROTOCOL_PATH}/repos"),"max_object":MAX_OBJECT,
        "max_negotiation":MAX_NEGOTIATION,"clone":"complete","locks":"advisory"}),
    )
}

async fn list(ExtractState(state): ExtractState<State>, request: Request) -> Result<Json<Value>> {
    let (user, repo, write) = credentials::identity(&state, &request).await?;
    let mut repos = super::web::listings(&state, &user).await?;
    if let Some(repo) = repo {
        repos.retain(|r| r["id"] == repo);
        for entry in &mut repos {
            if let Some(entry) = entry.as_object_mut() {
                entry.remove("upstream");
            }
        }
    }
    if !write {
        for repo in &mut repos {
            repo["role"] = json!("read");
        }
    }
    Ok(Json(json!({"version":VERSION,"repositories":repos})))
}

async fn metadata(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let _guard = state.0.vcs_files.read().await;
    let (repo, actor, _) = credentials::authorize(&state, &id, &request, Access::Read).await?;
    let refs = host(&state, &id)?.refs()?;
    Ok(Json(
        json!({"version":VERSION,"id":repo.id,"name":repo.name,"default_branch":refs.default_branch,
        "role":actor,"locks":"advisory"}),
    ))
}

async fn refs(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<skald_vcs_core::protocol::Refs>> {
    let _guard = state.0.vcs_files.read().await;
    credentials::authorize(&state, &id, &request, Access::Read).await?;
    Ok(Json(host(&state, &id)?.refs()?))
}

#[derive(Deserialize)]
struct History {
    branch: Option<String>,
    limit: Option<usize>,
}

async fn history(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    Query(query): Query<History>,
    request: Request,
) -> Result<Json<Value>> {
    let guard = state.0.vcs_files.clone().read_owned().await;
    credentials::authorize(&state, &id, &request, Access::Read).await?;
    let host = host(&state, &id)?;
    let refs = host.refs()?;
    let branch = query.branch.as_deref().unwrap_or(&refs.default_branch);
    let tip = refs.branches.get(branch).copied();
    if tip.is_none() && branch != refs.default_branch {
        return Err(Error::not_found());
    }
    let limit = query.limit.unwrap_or(30).min(100);
    let history = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        host.store().history(tip, limit)
    })
    .await??;
    Ok(Json(json!({"revisions":history})))
}

async fn parse<T: serde::de::DeserializeOwned>(request: Request, max: usize) -> Result<T> {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        != Some("application/json")
    {
        return Err(Error::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Send application/json.",
        ));
    }
    let bytes = body(request.into_body(), max).await?;
    serde_json::from_slice(&bytes).map_err(|_| Error::bad("Invalid JSON request."))
}

async fn body(body: Body, max: usize) -> Result<bytes::Bytes> {
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(60), to_bytes(body, max))
        .await
        .map_err(|_| Error::new(StatusCode::REQUEST_TIMEOUT, "Upload timed out."))?
        .map_err(|_| {
            Error::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body exceeded its limit.",
            )
        })?;
    Ok(bytes)
}

async fn missing(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Objects>> {
    let headers = request.headers().clone();
    let peer = credentials::peer(&request);
    credentials::authenticate(&state, &headers, peer, Some(&id), Access::Write).await?;
    let Objects { ids } = parse(request, 40 * 1024).await?;
    let guard = state.0.vcs_files.clone().read_owned().await;
    credentials::authenticate(&state, &headers, peer, Some(&id), Access::Write).await?;
    let host = host(&state, &id)?;
    let ids = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        host.missing(ids)
    })
    .await?
    .map_err(invalid)?;
    Ok(Json(Objects { ids }))
}

async fn download(
    ExtractState(state): ExtractState<State>,
    Path((id, object)): Path<(String, Id)>,
    request: Request,
) -> Result<Response> {
    let slot = state
        .0
        .vcs_transfers
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy())?;
    let guard = state.0.vcs_files.clone().read_owned().await;
    credentials::authorize(&state, &id, &request, Access::Read).await?;
    let host = host(&state, &id)?;
    if !host.store().contains(object) {
        return Err(Error::not_found());
    }
    let bytes = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        host.store().read(object)
    })
    .await??;
    // Keep the permit until the body is consumed or dropped.
    let stream = futures_util::stream::once(async move {
        let _slot = slot;
        Ok::<_, std::convert::Infallible>(bytes)
    });
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from_stream(stream))?)
}

fn busy() -> Error {
    Error::new(
        StatusCode::TOO_MANY_REQUESTS,
        "Repository transfers are busy. Retry shortly.",
    )
}

async fn upload(
    ExtractState(state): ExtractState<State>,
    Path((id, object)): Path<(String, Id)>,
    request: Request,
) -> Result<StatusCode> {
    let slot = state
        .0
        .vcs_transfers
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy())?;
    let auth_headers = request.headers().clone();
    let peer = credentials::peer(&request);
    credentials::authenticate(&state, &auth_headers, peer, Some(&id), Access::Write).await?;
    if auth_headers
        .get(header::CONTENT_TYPE)
        .is_none_or(|value| value != "application/octet-stream")
    {
        return Err(Error::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Send application/octet-stream.",
        ));
    }
    let raw = body(request.into_body(), MAX_OBJECT).await?;
    let guard = state.0.vcs_files.clone().write_owned().await;
    credentials::authenticate(&state, &auth_headers, peer, Some(&id), Access::Write).await?;
    let host = host(&state, &id)?;
    let (raw, host) = tokio::task::spawn_blocking(move || -> Result<_> {
        skald_vcs_core::Object::parse(object, &raw).map_err(invalid)?;
        Ok((raw, host))
    })
    .await??;
    let key = object.to_string();
    let limits = state.0.settings.read().await.clone();
    let size = raw.len() as i64 + 1;
    let mut tx = state.0.db.begin().await?;
    let accounted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM skald_objects WHERE repo_id=? AND object=?)",
    )
    .bind(&id)
    .bind(&key)
    .fetch_one(&mut *tx)
    .await?;
    if !accounted {
        let (local, count, total): (i64, i64, i64) = sqlx::query_as(
            "SELECT bytes,object_count,(SELECT coalesce(sum(bytes),0) FROM skald_repos) FROM skald_repos WHERE id=?"
        ).bind(&id).fetch_one(&mut *tx).await?;
        if count >= skald_vcs_core::MAX_GRAPH as i64
            || local.saturating_add(size) > (limits.vcs_repo_mib * 1_048_576) as i64
            || total.saturating_add(size) > (limits.vcs_total_mib * 1_048_576) as i64
        {
            return Err(Error::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "Repository storage quota reached.",
            ));
        }
        sqlx::query("INSERT INTO skald_objects(repo_id,object,size,created) VALUES(?,?,?,?)")
            .bind(&id)
            .bind(key)
            .bind(size)
            .bind(auth::now())
            .execute(&mut *tx)
            .await?;
    }
    if !host.store().contains(object) {
        crate::storage::ensure_disk_space(&state.0.data, size as u64)?;
    }
    tx.commit().await?;
    tokio::task::spawn_blocking(move || {
        let (_guard, _slot) = (guard, slot);
        host.receive(object, &raw, |_| Ok(()))
    })
    .await?
    .map_err(invalid)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn update(
    ExtractState(state): ExtractState<State>,
    Path((id, kind, name)): Path<(String, String, String)>,
    request: Request,
) -> Result<StatusCode> {
    let headers = request.headers().clone();
    let peer = credentials::peer(&request);
    credentials::authenticate(&state, &headers, peer, Some(&id), Access::Write).await?;
    let change: Update = parse(request, 4096).await?;
    let tags = match kind.as_str() {
        "branches" => false,
        "tags" => true,
        _ => return Err(Error::not_found()),
    };
    let guard = state.0.vcs_files.clone().write_owned().await;
    let (repo, _, user) = credentials::authenticate(
        &state,
        &headers,
        peer,
        Some(&id),
        if change.target.is_none() {
            Access::Manage
        } else {
            Access::Write
        },
    )
    .await?;
    let host = host(&state, &id)?;
    let refs = host.refs()?;
    if !tags
        && repo.protect_default
        && name == refs.default_branch
        && refs.branches.contains_key(&name)
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "The default branch requires a reviewed pull request.",
        ));
    }
    let detail = format!("{id}/{kind}/{name}");
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        host.update(tags, &name, change)
    })
    .await?
    .map_err(invalid)?;
    state.audit(&user, "vcs-reference", &detail).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn locks(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let _guard = state.0.vcs_files.read().await;
    credentials::authorize(&state, &id, &request, Access::Read).await?;
    Ok(Json(
        json!({"policy":"advisory","locks":host(&state, &id)?.locks()?}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lock {
    path: String,
    seconds: Option<u32>,
}

async fn lock(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>> {
    let headers = request.headers().clone();
    let peer = credentials::peer(&request);
    credentials::authenticate(&state, &headers, peer, Some(&id), Access::Write).await?;
    let input: Lock = parse(request, 4096).await?;
    let _guard = state.0.vcs_files.write().await;
    let (_, _, user) =
        credentials::authenticate(&state, &headers, peer, Some(&id), Access::Write).await?;
    let lock = host(&state, &id)?
        .lock(&input.path, &user, input.seconds)
        .map_err(invalid)?;
    Ok(Json(json!({"policy":"advisory","lock":lock})))
}
