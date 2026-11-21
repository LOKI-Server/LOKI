//! Versioned API for explicitly approved tools and desktop clients.
use crate::{Error, Result, State, auth, device_codes, devices};
use axum::{
    Extension, Json, Router,
    extract::{Path, Request, State as ExtractState},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) fn router(state: State) -> Router<State> {
    let private = crate::vcs::api_router()
        .route("/api/v1/me", get(me))
        .route("/api/v1/resources", get(resources))
        .route("/api/v1/devices", get(list_devices))
        .route(
            "/api/v1/devices/{id}",
            post(rename_device).delete(revoke_device),
        )
        .route("/api/v1/admin/users", get(users))
        .route("/api/v1/admin/groups", get(groups))
        .route("/api/v1/admin/activity", get(activity))
        .route("/api/v1/admin/grants", get(grants))
        .route("/api/v1/admin/backups", get(backups))
        .route("/api/v1/admin/settings", get(settings))
        .route("/api/v1/admin/actions", post(action))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    private
        .route("/api/v1", get(discovery))
        .route("/api/v1/auth/pair", post(pair))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/pair/poll", post(poll))
        .layer(middleware::from_fn_with_state(state, secure))
}

async fn secure(
    ExtractState(state): ExtractState<State>,
    request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>();
    let headers = request.headers();
    if headers.contains_key(header::ORIGIN)
        || headers.contains_key("sec-fetch-site")
        || peer.is_some_and(|peer| !peer.0.ip().is_loopback())
        || (state
            .0
            .settings
            .read()
            .await
            .public_url
            .starts_with("https://")
            && headers
                .get("x-forwarded-proto")
                .is_none_or(|proto| proto != "https"))
    {
        return Error::forbidden().into_response();
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

#[derive(Clone)]
struct Grant(String);

async fn authenticate(
    ExtractState(state): ExtractState<State>,
    mut request: Request,
    next: Next,
) -> Response {
    match devices::caller(&state, request.headers()).await {
        Ok(devices::Caller {
            user,
            api_scope: Some(scope),
            device,
        }) => {
            request.extensions_mut().insert(user);
            request.extensions_mut().insert(devices::ApiDevice(device));
            request.extensions_mut().insert(Grant(scope));
            next.run(request).await
        }
        Ok(_) => Error::coded(
            StatusCode::FORBIDDEN,
            "api_pairing_required",
            "Pair this client for the LOKI API.",
        )
        .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn discovery() -> Json<Value> {
    Json(
        json!({"version":1,"scopes":["repositories","owner"],"pair":"/api/v1/auth/pair","poll":"/api/v1/auth/pair/poll"}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Scope {
    Repositories,
    Owner,
}
impl Scope {
    fn kind(&self) -> &str {
        match self {
            Self::Repositories => device_codes::API_REPOSITORIES,
            Self::Owner => device_codes::API_OWNER,
        }
    }
}

#[derive(Deserialize)]
struct Pair {
    scope: Scope,
    #[serde(flatten)]
    device: devices::DeviceInfo,
}
async fn pair(
    ExtractState(state): ExtractState<State>,
    Extension(ip): Extension<auth::ClientIp>,
    Json(input): Json<Pair>,
) -> Result<Json<Value>> {
    devices::pair_for(&state, &ip, input.device, input.scope.kind()).await
}

#[derive(Deserialize)]
struct Login {
    scope: Scope,
    #[serde(flatten)]
    credentials: devices::Login,
}

async fn login(
    ExtractState(state): ExtractState<State>,
    Extension(ip): Extension<auth::ClientIp>,
    Json(input): Json<Login>,
) -> Result<Json<Value>> {
    if matches!(input.scope, Scope::Owner) {
        return Err(Error::coded(
            StatusCode::FORBIDDEN,
            "owner_pairing_required",
            "Pair and approve owner API access in the dashboard.",
        ));
    }
    devices::login_with_scope(&state, &ip, input.credentials, false, Some("repositories")).await
}

#[derive(Deserialize)]
struct Poll {
    scope: Scope,
    device_code: String,
}
async fn poll(
    ExtractState(state): ExtractState<State>,
    Json(input): Json<Poll>,
) -> Result<Json<Value>> {
    devices::poll_for(&state, input.scope.kind(), &input.device_code).await
}

async fn me(
    Extension(user): Extension<auth::User>,
    Extension(grant): Extension<Grant>,
) -> Json<Value> {
    Json(
        json!({"user":{"id":user.id,"username":user.username,"display_name":user.display_name,"owner":user.owner},"scope":grant.0}),
    )
}

async fn resources(
    ExtractState(state): ExtractState<State>,
    Extension(user): Extension<auth::User>,
) -> Result<Json<Value>> {
    let records: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT id,name,kind,visibility FROM resources WHERE kind NOT IN ('git-repo','git-create','vcs-repo') ORDER BY kind,name,id",
    ).fetch_all(&state.0.db).await?;
    let mut result = Vec::new();
    for (id, name, kind, visibility) in records {
        if auth::allowed(&state, Some(&user), &id, "access").await? {
            result.push(json!({"id":id,"name":name,"kind":kind,"visibility":visibility}));
        }
    }
    Ok(Json(json!(result)))
}

async fn list_devices(
    ExtractState(state): ExtractState<State>,
    Extension(user): Extension<auth::User>,
) -> Result<Json<Value>> {
    Ok(Json(devices::list(&state, &user).await?))
}

#[derive(Deserialize)]
struct DeviceName {
    name: String,
}

async fn rename_device(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    Extension(user): Extension<auth::User>,
    Json(input): Json<DeviceName>,
) -> Result<Json<Value>> {
    devices::rename(&state, &user, &id, &input.name).await?;
    state.audit(&user.id, "device-rename", &id).await?;
    Ok(Json(json!({"ok":true})))
}

async fn revoke_device(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    Extension(user): Extension<auth::User>,
) -> Result<Json<Value>> {
    devices::revoke(&state, &user, &id).await?;
    state.audit(&user.id, "device-revoke", &id).await?;
    Ok(Json(json!({"ok":true})))
}

fn owner(user: &auth::User, grant: &Grant) -> Result<()> {
    if !user.owner || grant.0 != "owner" {
        return Err(Error::coded(
            StatusCode::FORBIDDEN,
            "owner_scope_required",
            "This operation requires a paired owner API credential.",
        ));
    }
    Ok(())
}

async fn users(
    ExtractState(state): ExtractState<State>,
    Extension(user): Extension<auth::User>,
    Extension(grant): Extension<Grant>,
) -> Result<Json<Value>> {
    owner(&user, &grant)?;
    let users: Vec<auth::User> = sqlx::query_as("SELECT * FROM users ORDER BY owner DESC,username")
        .fetch_all(&state.0.db)
        .await?;
    Ok(Json(json!(users)))
}

async fn groups(
    ExtractState(state): ExtractState<State>,
    Extension(user): Extension<auth::User>,
    Extension(grant): Extension<Grant>,
) -> Result<Json<Value>> {
    owner(&user, &grant)?;
    let groups: Vec<(String, String)> =
        sqlx::query_as("SELECT id,name FROM groups ORDER BY name,id")
            .fetch_all(&state.0.db)
            .await?;
    let memberships: Vec<(String, String)> =
        sqlx::query_as("SELECT user_id,group_id FROM memberships ORDER BY user_id,group_id")
            .fetch_all(&state.0.db)
            .await?;
    Ok(Json(
        json!({"groups":groups.into_iter().map(|(id,name)|json!({"id":id,"name":name})).collect::<Vec<_>>(),"memberships":memberships}),
    ))
}

async fn activity(
    ExtractState(state): ExtractState<State>,
    Extension(user): Extension<auth::User>,
    Extension(grant): Extension<Grant>,
) -> Result<Json<Value>> {
    owner(&user, &grant)?;
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT actor,action,detail,created FROM audit WHERE action!='refresh-spent' ORDER BY id DESC LIMIT 100",
    ).fetch_all(&state.0.db).await?;
    Ok(Json(json!(rows.into_iter().map(|(actor,action,detail,created)|json!({"actor":actor,"action":action,"detail":detail,"created":created})).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct Action {
    action: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    data: Value,
}

async fn action(
    ExtractState(state): ExtractState<State>,
    Extension(user): Extension<auth::User>,
    Extension(grant): Extension<Grant>,
    Json(input): Json<Action>,
) -> Result<Json<Value>> {
    owner(&user, &grant)?;
    let result = match input.action.as_str() {
        "user-create" | "user-status" | "reset-issue" | "group-create" | "group-delete"
        | "membership" | "resource-access" | "share-save" | "app-action" | "backup" => {
            super::actions::owner_action(&state, &user, &input.action, &input.id, input.data)
                .await?
        }
        "sync-quota" => {
            crate::sync::action(&state, &user, &input.action, &input.id, input.data).await?
        }
        _ => return Err(Error::bad("This action is not available on the owner API.")),
    };
    state.audit(&user.id, &input.action, &input.id).await?;
    Ok(Json(result))
}

async fn grants(
    ExtractState(state): ExtractState<State>,
    Extension(user): Extension<auth::User>,
    Extension(grant): Extension<Grant>,
) -> Result<Json<Value>> {
    owner(&user, &grant)?;
    let entries: Vec<(String,String,String,String)> = sqlx::query_as("SELECT resource,subject_kind,subject,role FROM grants ORDER BY resource,subject_kind,subject,role").fetch_all(&state.0.db).await?;
    Ok(Json(
        json!(entries.into_iter().map(|(resource,subject_kind,subject,role)|
        json!({"resource":resource,"subject_kind":subject_kind,"subject":subject,"role":role})
    ).collect::<Vec<_>>()),
    ))
}

async fn backups(
    ExtractState(state): ExtractState<State>,
    Extension(user): Extension<auth::User>,
    Extension(grant): Extension<Grant>,
) -> Result<Json<Value>> {
    owner(&user, &grant)?;
    let mut directory = tokio::fs::read_dir(state.0.data.join("backups")).await?;
    let mut files = Vec::new();
    while let Some(file) = directory.next_entry().await? {
        let name = file.file_name();
        if let Some(name) = name
            .to_str()
            .filter(|name| name.starts_with("loki-") && name.ends_with(".zip"))
            && file.file_type().await?.is_file()
        {
            files.push((name.to_owned(), file.metadata().await?.len()));
        }
    }
    files.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    Ok(Json(json!(
        files
            .into_iter()
            .map(|(name, size)| json!({"file":name,"size":size}))
            .collect::<Vec<_>>()
    )))
}

async fn settings(
    ExtractState(state): ExtractState<State>,
    Extension(user): Extension<auth::User>,
    Extension(grant): Extension<Grant>,
) -> Result<Json<Value>> {
    owner(&user, &grant)?;
    let settings = state.0.settings.read().await;
    Ok(Json(json!({
        "name":settings.name,"public_url":settings.public_url,"registration":settings.registration,
        "sync_quota_mb":settings.sync_quota_mb,"sync_max_file_mb":settings.sync_max_file_mb,
        "vcs_repo_mib":settings.vcs_repo_mib,"vcs_total_mib":settings.vcs_total_mib,
    })))
}
