use super::{Access, Repo, access, credentials, directory, host, invalid, require};
use crate::{Error, Result, State, auth};
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, Query, State as ExtractState},
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use skald_vcs_core::{Id, Kind, reference, valid_path};
use skald_vcs_server::Hosted;

pub(crate) fn router() -> Router<State> {
    let router = Router::new()
        .route("/api/repos", get(index).post(create))
        .route("/api/repos/{id}", get(detail).delete(delete_repo))
        .route("/api/repos/{id}/file", get(file))
        .route("/api/repos/{id}/settings", post(settings))
        .route("/api/repos/{id}/permissions", get(permissions).post(grant))
        .route("/api/repos/{id}/maintenance", post(maintenance))
        .route(
            "/api/account/vcs-tokens",
            get(credentials::list).post(credentials::issue),
        )
        .route("/api/account/vcs-tokens/{id}", delete(credentials::revoke))
        .merge(super::protocol::router());
    super::collaboration::routes(router, "/api/repos")
}

pub(crate) fn api_router() -> Router<State> {
    let router = Router::new()
        .route("/api/v1/repos", get(index).post(create))
        .route("/api/v1/repos/{id}", get(detail).delete(delete_repo))
        .route("/api/v1/repos/{id}/file", get(file))
        .route(
            "/api/v1/repos/{id}/credential",
            post(credentials::issue_transfer),
        )
        .route(
            "/api/v1/repos/{id}/permissions",
            get(permissions).post(grant),
        )
        .route("/api/v1/repos/{id}/settings", post(settings))
        .route("/api/v1/repos/{id}/maintenance", post(maintenance));
    super::collaboration::routes(router, "/api/v1/repos")
}

pub(super) async fn actor(
    state: &State,
    headers: &HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<auth::User> {
    if let Some(Extension(user)) = native {
        return Ok(user);
    }
    Ok(auth::context(state, headers).await?.approved()?.clone())
}

async fn index(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    Ok(Json(json!({
        "repositories": listings(&state, &user.id).await?,
        "create": auth::allowed(&state, Some(&user), "vcs-create", "access").await?
    })))
}

pub(super) async fn listings(state: &State, user: &str) -> Result<Vec<Value>> {
    let repos: Vec<Repo> = sqlx::query_as(
        "SELECT id,owner_id,name,created,protect_default,upstream FROM skald_repos ORDER BY name,created",
    )
    .fetch_all(&state.0.db)
    .await?;
    let public = state.0.settings.read().await.public_url.clone();
    let mut visible = Vec::new();
    for repo in repos {
        if let Some(role) = access(&state.0.db, &repo, user).await? {
            let upstream = if let Some(id) = &repo.upstream {
                require(state, id, user, Access::Read)
                    .await
                    .ok()
                    .map(|_| id.clone())
            } else {
                None
            };
            visible.push(
                json!({"id":repo.id,"name":repo.name,"role":role,"owner_id":repo.owner_id,"upstream":upstream,
                "remote_url":remote_url(&public, &repo.id)}),
            );
        }
    }
    Ok(visible)
}

pub(super) fn remote_url(public: &str, id: &str) -> String {
    format!(
        "{}{}/repos/{id}",
        public.trim_end_matches('/'),
        super::PROTOCOL_PATH
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRepo {
    name: String,
    default_branch: Option<String>,
    upstream: Option<String>,
}

async fn create(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<NewRepo>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.vcs_files.write().await;
    if !auth::allowed(&state, Some(&user), "vcs-create", "access").await? {
        return Err(Error::forbidden());
    }
    let name = input.name.trim();
    if !crate::apps::valid_id(name) || name.len() > 64 || name.ends_with('-') {
        return Err(Error::bad(
            "Use up to 64 lowercase letters, numbers or hyphens.",
        ));
    }
    if let Some(default) = &input.default_branch {
        reference(default).map_err(invalid)?;
    }
    let upstream = if let Some(id) = &input.upstream {
        require(&state, id, &user.id, Access::Read).await?;
        Some(host(&state, id)?)
    } else {
        None
    };
    let mut tx = state.0.db.begin().await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM skald_repos")
        .fetch_one(&mut *tx)
        .await?;
    if count >= 500 {
        return Err(Error::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "Repository limit reached.",
        ));
    }
    let id = auth::id();
    sqlx::query(
        "INSERT INTO resources(id,kind,name,visibility) VALUES(?,'vcs-repo',?,'restricted')",
    )
    .bind(&id)
    .bind(name)
    .execute(&mut *tx)
    .await?;
    let result =
        sqlx::query("INSERT INTO skald_repos(id,owner_id,name,created,upstream) VALUES(?,?,?,?,?)")
            .bind(&id)
            .bind(&user.id)
            .bind(name)
            .bind(auth::now())
            .bind(&input.upstream)
            .execute(&mut *tx)
            .await;
    if result.as_ref().is_err_and(|error| {
        error
            .as_database_error()
            .is_some_and(|error| error.is_unique_violation())
    }) {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "You already have a repository with this name.",
        ));
    }
    result?;
    let target = directory(&state.0.data, &id);
    if let Some(upstream) = upstream {
        let limits = state.0.settings.read().await.clone();
        let current: i64 = sqlx::query_scalar("SELECT coalesce(sum(bytes),0) FROM skald_repos")
            .fetch_one(&mut *tx)
            .await?;
        let budget = (limits.vcs_repo_mib * 1_048_576)
            .min((limits.vcs_total_mib * 1_048_576).saturating_sub(current as u64));
        let data = state.0.data.clone();
        let default = input.default_branch;
        let stage = tempfile::Builder::new()
            .prefix(".skald-fork-")
            .tempdir_in(state.0.data.join("skald-repos"))?;
        let (stage, usage) = tokio::task::spawn_blocking(move || -> Result<_> {
            let mut total = 0u64;
            let mut usage = Vec::new();
            upstream
                .fork(
                    &stage.path().join("repo"),
                    default.as_deref(),
                    |id, size| {
                        total = total.saturating_add(size);
                        if total > budget {
                            return Err(
                                std::io::Error::from(std::io::ErrorKind::StorageFull).into()
                            );
                        }
                        if let Err(error) = crate::storage::ensure_disk_space(&data, size) {
                            if error.status == StatusCode::INSUFFICIENT_STORAGE {
                                return Err(
                                    std::io::Error::from(std::io::ErrorKind::StorageFull).into()
                                );
                            }
                            return Err(anyhow::anyhow!(error.message));
                        }
                        usage.push((id, size as i64));
                        Ok(())
                    },
                )
                .map_err(|error| {
                    if let Some(space) = error.downcast_ref::<std::io::Error>()
                        && space.kind() == std::io::ErrorKind::StorageFull
                    {
                        return Error::new(
                            StatusCode::INSUFFICIENT_STORAGE,
                            "Fork exceeds the repository storage quota.",
                        );
                    }
                    invalid(error)
                })?;
            Ok((stage, usage))
        })
        .await??;
        for batch in usage.chunks(150) {
            let mut query =
                sqlx::QueryBuilder::new("INSERT INTO skald_objects(repo_id,object,size,created) ");
            query.push_values(batch, |mut row, (object, size)| {
                row.push_bind(&id)
                    .push_bind(object.to_string())
                    .push_bind(*size)
                    .push_bind(auth::now());
            });
            query.build().execute(&mut *tx).await?;
        }
        std::fs::rename(stage.path().join("repo"), &target)?;
        skald_vcs_core::sync_directory(target.parent().ok_or_else(Error::not_found)?)?;
    } else {
        Hosted::create(&target, input.default_branch.as_deref().unwrap_or("main"))?;
    }
    tx.commit().await?;
    state.audit(&user.id, "vcs-create", &id).await?;
    let public = state.0.settings.read().await.public_url.clone();
    Ok(Json(json!({"id":id,"remote_url":remote_url(&public, &id)})))
}

async fn delete_repo(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.vcs_files.write().await;
    let (repo, _) = require(&state, &id, &user.id, Access::Manage).await?;
    if !user.owner && repo.owner_id != user.id {
        return Err(Error::forbidden());
    }
    let mut tx = state.0.db.begin().await?;
    sqlx::query("DELETE FROM skald_repos WHERE id=?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM resources WHERE id=?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    if let Err(error) = tokio::fs::remove_dir_all(directory(&state.0.data, &id)).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, repo=%id, "deleted repository files remain on disk");
    }
    state.audit(&user.id, "vcs-delete", &id).await?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
struct Browse {
    branch: Option<String>,
    revision: Option<Id>,
    path: Option<String>,
    raw: Option<bool>,
    offset: Option<usize>,
}

fn revision(
    host: &Hosted,
    branch: Option<&str>,
    selected: Option<Id>,
) -> Result<(String, Option<Id>)> {
    let refs = host.refs()?;
    let branch = branch.unwrap_or(&refs.default_branch).to_owned();
    let tip = refs.branches.get(&branch).copied();
    if tip.is_none() && branch != refs.default_branch {
        return Err(Error::not_found());
    }
    if let Some(selected) = selected {
        if selected.kind() != Kind::Revision || !host.store().contains(selected) {
            return Err(Error::not_found());
        }
        return Ok((branch, Some(selected)));
    }
    Ok((branch, tip))
}

async fn detail(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    Query(query): Query<Browse>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.vcs_files.read().await;
    let (repo, role) = require(&state, &id, &user.id, Access::Read).await?;
    let host = host(&state, &id)?;
    let refs = host.refs()?;
    let (selected, revision) = revision(&host, query.branch.as_deref(), query.revision)?;
    let prefix = query.path.unwrap_or_default();
    if !prefix.is_empty() {
        valid_path(&prefix).map_err(invalid)?;
    }
    let offset = query
        .offset
        .unwrap_or_default()
        .min(skald_vcs_core::MAX_FILES);
    let (tree, next_offset, commits) = tokio::task::spawn_blocking(move || -> Result<_> {
        let Some(revision) = revision else { return Ok((Vec::new(), None, Vec::new())); };
        let root = if prefix.is_empty() { host.store().revision(revision)?.root }
            else { host.entry(revision, &prefix).map_err(|_| Error::not_found())?.id };
        if root.kind() != Kind::Tree { return Err(Error::not_found()); }
        let mut tree: Vec<_> = host.store().tree(root)?.into_iter().skip(offset).take(501).map(|(name, entry)| {
            json!({"name":name,"kind":if entry.id.kind() == Kind::Tree { "tree" } else { "file" }})
        }).collect();
        let next_offset = (tree.len() > 500).then(|| {
            tree.pop();
            offset + 500
        });
        let commits = host.store().history([revision], 30)?.into_iter().map(|(id, commit)|
            json!({"oid":id,"time":commit.timestamp,"author":commit.author,"title":commit.message.lines().next()})).collect();
        Ok((tree, next_offset, commits))
    }).await??;
    let upstream = if let Some(upstream) = &repo.upstream {
        require(&state, upstream, &user.id, Access::Read)
            .await
            .ok()
            .map(|(repo, _)| json!({"id":repo.id,"name":repo.name}))
    } else {
        None
    };
    let can_fork = auth::allowed(&state, Some(&user), "vcs-create", "access").await?;
    let public = state.0.settings.read().await.public_url.clone();
    Ok(Json(
        json!({"id":id,"name":repo.name,"role":role,"remote_url":remote_url(&public, &id),
        "branches":refs.branches.keys().collect::<Vec<_>>(),"branch":selected,"default_branch":refs.default_branch,
        "protect_default":repo.protect_default,"upstream":upstream,"can_fork":can_fork,"tags":refs.tags,"tree":tree,"next_offset":next_offset,"revision":revision,"commits":commits,"can_delete":user.owner || repo.owner_id==user.id}),
    ))
}

async fn file(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    Query(query): Query<Browse>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Response> {
    let user = actor(&state, &headers, native).await?;
    let path = query.path.ok_or_else(Error::not_found)?;
    valid_path(&path).map_err(invalid)?;
    let guard = state.0.vcs_files.clone().read_owned().await;
    require(&state, &id, &user.id, Access::Read).await?;
    let host = host(&state, &id)?;
    let (_, revision) = revision(&host, query.branch.as_deref(), query.revision)?;
    let revision = revision.ok_or_else(Error::not_found)?;
    let entry = host
        .entry(revision, &path)
        .map_err(|_| Error::not_found())?;
    if entry.id.kind() != Kind::File {
        return Err(Error::not_found());
    }
    let manifest = host.store().manifest(entry.id)?;
    let mut response = Response::builder()
        .header(header::CACHE_CONTROL, "private, no-store")
        .header("X-Content-Type-Options", "nosniff");
    if !query.raw.unwrap_or(false) {
        if manifest.size > 512 * 1024 {
            return Ok(response
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"text":null}"#))?);
        }
        let bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            let _guard = guard;
            let mut bytes = Vec::new();
            host.store().copy_file(entry.id, &mut bytes)?;
            Ok(bytes)
        })
        .await??;
        let text = String::from_utf8(bytes).ok().filter(|s| !s.contains('\0'));
        return Ok(response
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&json!({"text":text}))?))?);
    }
    let name = url::form_urlencoded::byte_serialize(
        path.rsplit('/').next().unwrap_or("download").as_bytes(),
    )
    .collect::<String>();
    let stream = futures_util::stream::try_unfold(
        (host, manifest.chunks.into_iter(), guard),
        |(host, mut chunks, guard)| async move {
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let Some(chunk) = chunks.next() else {
                    return Ok(None);
                };
                let bytes = host.store().read(chunk.id)?;
                anyhow::ensure!(bytes.len() == chunk.size as usize, "chunk size mismatch");
                Ok(Some((bytes, (host, chunks, guard))))
            })
            .await?
        },
    );
    response = response
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, manifest.size)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"download\"; filename*=UTF-8''{name}"),
        );
    Ok(response.body(Body::from_stream(stream))?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    default_branch: String,
    protect_default: Option<bool>,
}

async fn settings(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<Settings>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.vcs_files.write().await;
    require(&state, &id, &user.id, Access::Manage).await?;
    host(&state, &id)?
        .set_default_branch(&input.default_branch)
        .map_err(invalid)?;
    if let Some(protect) = input.protect_default {
        sqlx::query("UPDATE skald_repos SET protect_default=? WHERE id=?")
            .bind(protect)
            .bind(&id)
            .execute(&state.0.db)
            .await?;
    }
    state.audit(&user.id, "vcs-settings", &id).await?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
struct Maintenance {
    operation: String,
}

async fn maintenance(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<Maintenance>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let guard = state.0.vcs_files.clone().write_owned().await;
    require(&state, &id, &user.id, Access::Manage).await?;
    let host = host(&state, &id)?;
    let roots = super::collaboration::submission_roots(&state, &id).await?;
    if input.operation == "verify" {
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            host.verify_with(roots)
        })
        .await??;
        return Ok(Json(json!({"ok":true})));
    }
    if input.operation != "gc" {
        return Err(Error::bad("Unknown repository maintenance operation."));
    }
    let (removed, present, _guard) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let removed = host.gc_with(std::time::Duration::from_secs(86_400), roots)?;
        Ok((
            removed,
            host.store()
                .object_ids()?
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
            guard,
        ))
    })
    .await??;
    let mut tx = state.0.db.begin().await?;
    let candidates: Vec<String> =
        sqlx::query_scalar("SELECT object FROM skald_objects WHERE repo_id=? AND created<=?")
            .bind(&id)
            .bind(auth::now() - 86_400)
            .fetch_all(&mut *tx)
            .await?;
    for key in candidates {
        if key.parse::<Id>().is_ok_and(|id| !present.contains(&id)) {
            sqlx::query("DELETE FROM skald_objects WHERE repo_id=? AND object=?")
                .bind(&id)
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    state.audit(&user.id, "vcs-gc", &id).await?;
    Ok(Json(json!({"removed":removed})))
}

async fn permissions(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.vcs_files.write().await;
    require(&state, &id, &user.id, Access::Manage).await?;
    let users: Vec<(String, String)> = sqlx::query_as(
        "SELECT id,username FROM users WHERE status='approved' ORDER BY username LIMIT 2000",
    )
    .fetch_all(&state.0.db)
    .await?;
    let groups: Vec<(String, String)> =
        sqlx::query_as("SELECT id,name FROM groups ORDER BY name LIMIT 1000")
            .fetch_all(&state.0.db)
            .await?;
    let grants: Vec<(String,String,String)> = sqlx::query_as("SELECT subject_kind,subject,role FROM grants WHERE resource=? ORDER BY subject_kind,subject").bind(id).fetch_all(&state.0.db).await?;
    Ok(Json(json!({"users":users,"groups":groups,"grants":grants})))
}

#[derive(Deserialize)]
struct Grant {
    subject_kind: String,
    subject: String,
    role: String,
}

async fn grant(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<Grant>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.vcs_files.write().await;
    require(&state, &id, &user.id, Access::Manage).await?;
    let query = match input.subject_kind.as_str() {
        "user" => "SELECT EXISTS(SELECT 1 FROM users WHERE id=? AND status='approved')",
        "group" => "SELECT EXISTS(SELECT 1 FROM groups WHERE id=?)",
        _ => return Err(Error::bad("Choose a user or group.")),
    };
    if !matches!(input.role.as_str(), "read" | "write" | "manage" | "remove") {
        return Err(Error::bad("Unknown repository role."));
    }
    let mut tx = state.0.db.begin().await?;
    if input.role != "remove"
        && !sqlx::query_scalar::<_, bool>(query)
            .bind(&input.subject)
            .fetch_one(&mut *tx)
            .await?
    {
        return Err(Error::not_found());
    }
    sqlx::query("DELETE FROM grants WHERE resource=? AND subject_kind=? AND subject=?")
        .bind(&id)
        .bind(&input.subject_kind)
        .bind(&input.subject)
        .execute(&mut *tx)
        .await?;
    if input.role != "remove" {
        sqlx::query("INSERT INTO grants(resource,subject_kind,subject,role) VALUES(?,?,?,?)")
            .bind(&id)
            .bind(&input.subject_kind)
            .bind(&input.subject)
            .bind(&input.role)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    state.audit(&user.id, "vcs-grant", &id).await?;
    Ok(Json(json!({"ok":true})))
}
