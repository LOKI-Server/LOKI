use super::{
    Access, Repo, access, branch, checked_binary, collaboration, credentials, fetch, initialize,
    lfs, repo_dir, require, reserves, resolve, run, size, transport,
};
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

pub(crate) fn router() -> Router<State> {
    Router::new()
        .route(
            "/git-http/{id}/{*endpoint}",
            get(transport::route)
                .post(transport::route)
                .put(transport::route),
        )
        .route("/api/git", get(index).post(create))
        .route("/api/git/{id}", get(detail).delete(delete_repo))
        .route("/api/git/{id}/file", get(file))
        .route("/api/git/{id}/permissions", get(permissions).post(grant))
        .route(
            "/api/account/git-tokens",
            get(credentials::list).post(credentials::issue),
        )
        .route("/api/account/git-tokens/{id}", delete(credentials::revoke))
        .route("/api/git/{id}/settings", post(repository_settings))
        .route("/api/git/{id}/items", get(items).post(create_item))
        .route("/api/git/{id}/items/{item}", get(item).post(change_item))
}

pub(crate) fn api_router() -> Router<State> {
    Router::new()
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
        .route("/api/v1/repos/{id}/settings", post(repository_settings))
        .route("/api/v1/repos/{id}/items", get(items).post(create_item))
        .route(
            "/api/v1/repos/{id}/items/{item}",
            get(item).post(change_item),
        )
}

async fn actor(
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
    let repositories: Vec<Repo> = sqlx::query_as("SELECT * FROM git_repos ORDER BY name,created")
        .fetch_all(&state.0.db)
        .await?;
    let mut visible = Vec::new();
    for repo in repositories {
        if let Some(role) = access(&state.0.db, &repo, &user.id).await? {
            let upstream = match &repo.upstream {
                Some(id) if require(&state, id, &user.id, Access::Read).await.is_ok() => Some(id),
                _ => None,
            };
            visible.push(json!({"id":repo.id,"name":repo.name,"role":role,"owner_id":repo.owner_id,"upstream":upstream}));
        }
    }
    Ok(Json(
        json!({"repositories":visible,"create":auth::allowed(&state,Some(&user),"git-create","access").await?}),
    ))
}

#[derive(Deserialize)]
struct NewRepo {
    name: String,
    upstream: Option<String>,
    default_branch: Option<String>,
}

async fn delete_repo(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.git_files.write().await;
    let (repo, _) = require(&state, &id, &user.id, Access::Manage).await?;
    if !user.owner && repo.owner_id != user.id {
        return Err(Error::forbidden());
    }
    // Delete authority first. A leftover directory after interruption has no route
    // and startup recovery can remove it using the missing repository record.
    let mut tx = state.0.db.begin().await?;
    sqlx::query("DELETE FROM git_repos WHERE id=?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM resources WHERE id=?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let directory = repo_dir(&state.0.data, &id);
    if let Err(error) = tokio::fs::remove_dir_all(directory).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error,repo=%id,"repository removed but its files need startup cleanup");
    }
    if let Err(error) = lfs::collect(&state.0.data, &state.0.db).await {
        tracing::warn!(error = %error.message, "could not reclaim deleted repository's Git LFS objects");
    }
    state.audit(&user.id, "git-delete", &id).await?;
    Ok(Json(json!({"ok":true})))
}

async fn create(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<NewRepo>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    if !auth::allowed(&state, Some(&user), "git-create", "access").await? {
        return Err(Error::forbidden());
    }
    let name = input.name.trim();
    if !crate::apps::valid_id(name) || name.len() > 64 || name.ends_with('-') {
        return Err(Error::bad(
            "Use a repository name up to 64 characters with lowercase letters, numbers, or hyphens.",
        ));
    }
    let git = checked_binary(&state).await?;
    let _guard = state.0.git_files.write().await;
    if !auth::allowed(&state, Some(&user), "git-create", "access").await? {
        return Err(Error::forbidden());
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM git_repos")
        .fetch_one(&state.0.db)
        .await?;
    if count >= super::MAX_REPOS {
        return Err(Error::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "This installation reached its repository limit.",
        ));
    }
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM git_repos WHERE owner_id=? AND name=?)")
            .bind(&user.id)
            .bind(name)
            .fetch_one(&state.0.db)
            .await?;
    if exists {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "You already have a repository with this name.",
        ));
    }
    let upstream = if let Some(id) = &input.upstream {
        Some(require(&state, id, &user.id, Access::Read).await?.0)
    } else {
        None
    };
    let default_branch = input
        .default_branch
        .as_deref()
        .or_else(|| upstream.as_ref().map(|repo| repo.default_branch.as_str()))
        .unwrap_or("main");
    let default_ref = branch(default_branch)?;
    let id = auth::id();
    let directory = repo_dir(&state.0.data, &id);
    let limits = state.0.settings.read().await.clone();
    let repo_bytes = limits.git_repo_mib * super::MIB;
    let result = async {
        let upstream_path = upstream
            .as_ref()
            .map(|repo| repo_dir(&state.0.data, &repo.id));
        let budget = upstream_path
            .as_ref()
            .map(|dir| size(&dir.join("objects"), repo_bytes))
            .transpose()?
            .unwrap_or_default();
        reserves(&state.0.data, None, budget + super::MIB, &limits)?;
        initialize(&git, &directory).await?;
        super::write_head(&directory, &default_ref)?;
        if let Some(source) = upstream_path {
            resolve(&git, &source, &default_ref).await?;
            fetch(&git, &directory, &source, "refs/heads/*", "refs/heads/*").await?;
            fetch(&git, &directory, &source, "refs/tags/*", "refs/tags/*").await?;
        }
        size(&directory, repo_bytes)?;
        size(&state.0.data.join("git-repos"), limits.git_total_mib * super::MIB)?;
        let mut tx = state.0.db.begin().await?;
        sqlx::query(
            "INSERT INTO resources(id,kind,name,visibility) VALUES(?,'git-repo',?,'restricted')",
        )
        .bind(&id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO git_repos(id,owner_id,name,upstream,created,default_branch,protect_default) VALUES(?,?,?,?,?,?,0)")
            .bind(&id)
            .bind(&user.id)
            .bind(name)
            .bind(upstream.as_ref().map(|repo| &repo.id))
            .bind(auth::now())
            .bind(default_branch)
            .execute(&mut *tx)
            .await?;
        if let Some(source) = &upstream {
            let objects = lfs::repository_pointers(&git, &directory).await?;
            lfs::share(&state, &mut tx, &source.id, &id, &objects).await?;
        }
        tx.commit().await?;
        Ok::<_, Error>(())
    }
    .await;
    if let Err(error) = result {
        if let Err(io) = std::fs::remove_dir_all(&directory)
            && io.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%io,"could not remove incomplete Git repo");
        }
        return Err(error);
    }
    state.audit(&user.id, "git-create", &id).await?;
    let public = state.0.settings.read().await.public_url.clone();
    Ok(Json(
        json!({"id":id,"name":name,"default_branch":default_branch,"clone_url":format!("{public}/git-http/{id}.git")}),
    ))
}

#[derive(Deserialize)]
struct Browse {
    branch: Option<String>,
    path: Option<String>,
    raw: Option<bool>,
}

async fn detail(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    Query(query): Query<Browse>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.git_files.read().await;
    let (repo, role) = require(&state, &id, &user.id, Access::Read).await?;
    let _process = slot(&state)?;
    let git = checked_binary(&state).await?;
    let dir = repo_dir(&state.0.data, &id);
    let refs = run(
        &git,
        &dir,
        &[
            "for-each-ref",
            "--count=500",
            "--format=%(refname:lstrip=2)",
            "refs/heads",
        ],
        128 * 1024,
    )
    .await?;
    let branches: Vec<_> = String::from_utf8_lossy(&refs)
        .lines()
        .map(str::to_owned)
        .collect();
    let selected = query.branch.as_deref().unwrap_or(&repo.default_branch);
    let prefix = query.path.as_deref().unwrap_or("");
    if !prefix.is_empty() {
        safe_tree_path(prefix)?;
    }
    let mut tree = Vec::new();
    let mut commits = Vec::new();
    if !branches.is_empty()
        && (query.branch.is_some() || branches.iter().any(|name| name == selected))
    {
        let reference = branch(selected)?;
        let commit = resolve(&git, &dir, &reference).await?;
        let object = if prefix.is_empty() {
            commit.clone()
        } else {
            format!("{commit}:{prefix}")
        };
        let listing = run(&git, &dir, &["ls-tree", "-z", &object], 512 * 1024).await?;
        for entry in listing.split(|c| *c == 0).filter(|line| !line.is_empty()) {
            let Ok(entry) = std::str::from_utf8(entry) else {
                continue;
            };
            if let Some((attributes, name)) = entry.split_once('\t') {
                let parts: Vec<_> = attributes.split(' ').collect();
                if let [mode, kind, _] = parts.as_slice() {
                    tree.push(json!({"name":name,"kind":kind,"mode":mode}));
                }
            }
        }
        let output = run(
            &git,
            &dir,
            &[
                "log",
                "-n",
                "30",
                "-z",
                "--format=%H%x00%ct%x00%an%x00%s",
                &commit,
                "--",
            ],
            256 * 1024,
        )
        .await?;
        let entries: Vec<_> = output.split(|c| *c == 0).collect();
        for entry in entries.as_chunks::<4>().0 {
            commits.push(json!({"oid":String::from_utf8_lossy(entry[0]),"time":String::from_utf8_lossy(entry[1]).parse::<i64>().unwrap_or_default(),"author":String::from_utf8_lossy(entry[2]),"title":String::from_utf8_lossy(entry[3])}));
        }
    }
    let upstream = match &repo.upstream {
        Some(upstream) => require(&state, upstream, &user.id, Access::Read)
            .await
            .ok()
            .map(|(repo, _)| json!({"id":repo.id,"name":repo.name})),
        None => None,
    };
    let public = state.0.settings.read().await.public_url.clone();
    Ok(Json(
        json!({"id":repo.id,"name":repo.name,"role":role,"upstream":upstream,"clone_url":format!("{public}/git-http/{id}.git"),"branches":branches,"branch":selected,"default_branch":repo.default_branch,"protect_default":repo.protect_default,"tree":tree,"commits":commits,"can_delete":user.owner || repo.owner_id==user.id,"can_fork":auth::allowed(&state,Some(&user),"git-create","access").await?}),
    ))
}

fn safe_tree_path(path: &str) -> Result<()> {
    if path.len() > 1024
        || path.chars().any(char::is_control)
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(Error::bad("Invalid repository path."));
    }
    Ok(())
}

fn slot(state: &State) -> Result<tokio::sync::OwnedSemaphorePermit> {
    state
        .0
        .git_processes
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            Error::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Git is busy. Try again shortly.",
            )
        })
}

async fn file(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    Query(query): Query<Browse>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Response> {
    let user = actor(&state, &headers, native).await?;
    let path = query.path.as_deref().ok_or_else(Error::not_found)?;
    safe_tree_path(path)?;
    let _guard = state.0.git_files.read().await;
    let (repo, _) = require(&state, &id, &user.id, Access::Read).await?;
    let _process = slot(&state)?;
    let git = checked_binary(&state).await?;
    let dir = repo_dir(&state.0.data, &id);
    let commit = resolve(
        &git,
        &dir,
        &branch(query.branch.as_deref().unwrap_or(&repo.default_branch))?,
    )
    .await?;
    let raw = query.raw.unwrap_or_default();
    let bytes = run(
        &git,
        &dir,
        &["cat-file", "blob", &format!("{commit}:{path}")],
        if raw { 64 * 1024 * 1024 } else { 512 * 1024 },
    )
    .await?;
    let mut response = Response::builder().header(header::CACHE_CONTROL, "private, no-store");
    let body = if raw {
        let name = url::form_urlencoded::byte_serialize(
            path.rsplit('/').next().unwrap_or("download").as_bytes(),
        )
        .collect::<String>();
        response = response
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"download\"; filename*=UTF-8''{name}"),
            );
        Body::from(bytes)
    } else {
        response = response.header(header::CONTENT_TYPE, "application/json");
        Body::from(serde_json::to_vec(
            &json!({"text":String::from_utf8(bytes).ok()}),
        )?)
    };
    Ok(response.body(body)?)
}

async fn permissions(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
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
    state.audit(&user.id, "git-grant", &id).await?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
struct RepositorySettings {
    default_branch: String,
    protect_default: bool,
}

async fn repository_settings(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<RepositorySettings>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let reference = branch(&input.default_branch)?;
    let _guard = state.0.git_files.write().await;
    let (repo, _) = require(&state, &id, &user.id, Access::Manage).await?;
    let git = checked_binary(&state).await?;
    let directory = repo_dir(&state.0.data, &id);
    let changing_branch = input.default_branch != repo.default_branch;
    if changing_branch {
        resolve(&git, &directory, &reference).await?;
        super::write_head(&directory, &reference)?;
    }
    let result = async {
        let mut tx = state.0.db.begin().await?;
        sqlx::query("UPDATE git_repos SET default_branch=?,protect_default=? WHERE id=?")
            .bind(&input.default_branch).bind(input.protect_default).bind(&id)
            .execute(&mut *tx).await?;
        if changing_branch {
            sqlx::query("UPDATE git_items SET approved_head=NULL,approved_base=NULL,reviewer_id=NULL WHERE repo_id=? AND kind='pull' AND status!='merged'")
                .bind(&id).execute(&mut *tx).await?;
        }
        tx.commit().await
    }.await;
    if let Err(error) = result {
        if changing_branch {
            super::write_head(&directory, &repo.default_ref())?;
        }
        return Err(error.into());
    }
    state.audit(&user.id, "git-settings", &id).await?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
struct ItemsPage {
    before: Option<i64>,
}

async fn items(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    Query(query): Query<ItemsPage>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let (repo, _) = require(&state, &id, &user.id, Access::Read).await?;
    Ok(Json(
        collaboration::list(&state, &repo, query.before.unwrap_or(i64::MAX)).await?,
    ))
}

async fn create_item(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<collaboration::New>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.git_files.write().await;
    let (repo, _) = require(&state, &id, &user.id, Access::Read).await?;
    Ok(Json(
        json!({"id":collaboration::create(&state,&repo,&user.id,input).await?}),
    ))
}

async fn item(
    ExtractState(state): ExtractState<State>,
    Path((id, item)): Path<(String, String)>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.git_files.read().await;
    let _process = slot(&state)?;
    let (repo, role) = require(&state, &id, &user.id, Access::Read).await?;
    let mut detail = collaboration::detail(&state, &repo, &item).await?;
    detail["role"] = json!(role);
    Ok(Json(detail))
}

async fn change_item(
    ExtractState(state): ExtractState<State>,
    Path((id, item)): Path<(String, String)>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<collaboration::Change>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.git_files.write().await;
    let (repo, role) = require(&state, &id, &user.id, Access::Read).await?;
    let audit = format!("git-{}", input.action);
    collaboration::change(&state, &repo, &item, &user.id, role, input).await?;
    state.audit(&user.id, &audit, &item).await?;
    Ok(Json(json!({"ok":true})))
}
