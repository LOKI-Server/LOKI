//! Who owns a repository, and moving it to another person or group.
//!
//! A repository belongs to a person, or to a group from People & groups (which only
//! the server owner manages). Handing one over needs the recipient's agreement unless
//! the person handing it over is also the one who would agree: moving a repository
//! to yourself, or the server owner moving one into a group. While a transfer waits,
//! code changes are paused (see `ensure_unpaused`).
use super::{Access, Repo, can_give_away, find, require, web::actor};
use crate::{Error, Result, State, auth};
use axum::{
    Extension, Json,
    extract::{Path, State as ExtractState},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{Value, json};

/// How the owner of a repository is shown: a person or a group, with a name and icon.
pub(super) async fn owner(db: &sqlx::SqlitePool, repo: &Repo) -> Result<Value> {
    Ok(match &repo.group_id {
        Some(group) => {
            let (name, icon): (String, bool) =
                sqlx::query_as("SELECT name,icon FROM groups WHERE id=?")
                    .bind(group)
                    .fetch_one(db)
                    .await?;
            json!({"kind":"group","id":group,"name":name,"icon":icon})
        }
        None => {
            let (name, avatar): (String, bool) =
                sqlx::query_as("SELECT username,avatar FROM users WHERE id=?")
                    .bind(&repo.owner_id)
                    .fetch_one(db)
                    .await?;
            json!({"kind":"user","id":repo.owner_id,"name":name,"icon":avatar})
        }
    })
}

/// Groups this person may create repositories in: every group for the server owner;
/// otherwise the groups they belong to, if the server lets members do that.
pub(super) async fn creatable_groups(state: &State, user: &auth::User) -> Result<Vec<Value>> {
    let rows: Vec<(String, String, bool)> = if user.owner {
        sqlx::query_as("SELECT id,name,icon FROM groups ORDER BY name")
            .fetch_all(&state.0.db)
            .await?
    } else if auth::allowed(state, Some(user), "vcs-group-create", "access").await? {
        sqlx::query_as("SELECT g.id,g.name,g.icon FROM groups g JOIN memberships m ON m.group_id=g.id WHERE m.user_id=? ORDER BY g.name")
            .bind(&user.id)
            .fetch_all(&state.0.db)
            .await?
    } else {
        Vec::new()
    };
    Ok(rows
        .into_iter()
        .map(|(id, name, icon)| json!({"id":id,"name":name,"icon":icon}))
        .collect())
}

/// Whether this person may create a repository owned by `group`.
pub(super) async fn may_create_in(state: &State, user: &auth::User, group: &str) -> Result<bool> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM groups WHERE id=?)")
        .bind(group)
        .fetch_one(&state.0.db)
        .await?;
    if !exists {
        return Err(Error::not_found());
    }
    if user.owner {
        return Ok(true);
    }
    let member: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE user_id=? AND group_id=?)",
    )
    .bind(&user.id)
    .bind(group)
    .fetch_one(&state.0.db)
    .await?;
    Ok(member && auth::allowed(state, Some(user), "vcs-group-create", "access").await?)
}

#[derive(sqlx::FromRow)]
struct Pending {
    repo_id: String,
    to_kind: String,
    to_id: String,
    requested_by: String,
    created: i64,
}

/// Who decides a transfer: the receiving person, or the server owner for a group.
fn decides(transfer: &Pending, user: &auth::User) -> bool {
    match transfer.to_kind.as_str() {
        "user" => transfer.to_id == user.id,
        _ => user.owner,
    }
}

async fn target_name(db: &sqlx::SqlitePool, kind: &str, id: &str) -> Result<Option<String>> {
    let query = match kind {
        "user" => "SELECT username FROM users WHERE id=? AND status='approved'",
        "group" => "SELECT name FROM groups WHERE id=?",
        _ => return Ok(None),
    };
    Ok(sqlx::query_scalar(query)
        .bind(id)
        .fetch_optional(db)
        .await?)
}

async fn describe(
    state: &State,
    transfer: &Pending,
    user: &auth::User,
    repo: Option<&Repo>,
) -> Result<Value> {
    let db = &state.0.db;
    let repo = match repo {
        Some(repo) => repo.clone(),
        None => find(state, &transfer.repo_id).await?,
    };
    let from = owner(db, &repo).await?;
    let requested_by: String = sqlx::query_scalar("SELECT username FROM users WHERE id=?")
        .bind(&transfer.requested_by)
        .fetch_one(db)
        .await?;
    let to = target_name(db, &transfer.to_kind, &transfer.to_id)
        .await?
        .unwrap_or_default();
    Ok(json!({
        "repo_id": repo.id, "repo_name": repo.name, "from": from,
        "to": {"kind": transfer.to_kind, "id": transfer.to_id, "name": to},
        "requested_by": requested_by, "created": transfer.created,
        "can_decide": decides(transfer, user),
        "can_cancel": transfer.requested_by == user.id || can_give_away(&repo, user),
    }))
}

/// The transfer waiting on a repository, as this person sees it.
pub(super) async fn pending(
    state: &State,
    repo: &Repo,
    user: &auth::User,
) -> Result<Option<Value>> {
    let transfer: Option<Pending> = sqlx::query_as("SELECT * FROM skald_transfers WHERE repo_id=?")
        .bind(&repo.id)
        .fetch_optional(&state.0.db)
        .await?;
    match transfer {
        Some(transfer) => Ok(Some(describe(state, &transfer, user, Some(repo)).await?)),
        None => Ok(None),
    }
}

/// Transfers this person can accept or decline, or asked for themselves.
pub(super) async fn involving(state: &State, user: &auth::User) -> Result<Vec<Value>> {
    let rows: Vec<Pending> = sqlx::query_as(
        "SELECT * FROM skald_transfers WHERE requested_by=?1 OR (to_kind='user' AND to_id=?1) OR (to_kind='group' AND ?2) ORDER BY created",
    )
    .bind(&user.id)
    .bind(user.owner)
    .fetch_all(&state.0.db)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(describe(state, row, user, None).await?);
    }
    Ok(out)
}

/// How many transfers wait on this person's decision, for the navigation badge.
pub(crate) async fn waiting_count(db: &sqlx::SqlitePool, user: &auth::User) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM skald_transfers WHERE (to_kind='user' AND to_id=?1) OR (to_kind='group' AND ?2)",
    )
    .bind(&user.id)
    .bind(user.owner)
    .fetch_one(db)
    .await?)
}

/// Moves the repository to its new owner. A group keeps the creator in `owner_id`.
async fn apply(
    tx: &mut sqlx::SqliteConnection,
    repo: &Repo,
    kind: &str,
    target: &str,
) -> Result<()> {
    let result = if kind == "user" {
        sqlx::query("UPDATE skald_repos SET owner_id=?,group_id=NULL WHERE id=?")
            .bind(target)
            .bind(&repo.id)
            .execute(&mut *tx)
            .await
    } else {
        sqlx::query("UPDATE skald_repos SET group_id=? WHERE id=?")
            .bind(target)
            .bind(&repo.id)
            .execute(&mut *tx)
            .await
    };
    if result.as_ref().is_err_and(|error| {
        error
            .as_database_error()
            .is_some_and(|error| error.is_unique_violation())
    }) {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "The new owner already has a repository with this name. Rename one of them first.",
        ));
    }
    result?;
    sqlx::query("DELETE FROM skald_transfers WHERE repo_id=?")
        .bind(&repo.id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    /// `start`, `accept`, `decline` or `cancel`.
    action: String,
    /// For `start`: `user` or `group`.
    kind: Option<String>,
    /// For `start`: the person's or group's id.
    target: Option<String>,
}

pub(super) async fn transfer(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<Change>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.vcs_files.write().await;
    let repo = find(&state, &id).await?;
    let current: Option<Pending> = sqlx::query_as("SELECT * FROM skald_transfers WHERE repo_id=?")
        .bind(&id)
        .fetch_optional(&state.0.db)
        .await?;
    let mut tx = state.0.db.begin().await?;
    let outcome = match input.action.as_str() {
        "start" => {
            // Only someone who can see and manage the repository may give it away.
            require(&state, &id, &user.id, Access::Manage).await?;
            if !can_give_away(&repo, &user) {
                return Err(Error::forbidden());
            }
            if current.is_some() {
                return Err(Error::new(
                    StatusCode::CONFLICT,
                    "A transfer is already waiting. Cancel it first.",
                ));
            }
            let kind = input.kind.as_deref().unwrap_or_default();
            let target = input.target.as_deref().unwrap_or_default();
            let Some(name) = target_name(&state.0.db, kind, target).await? else {
                return Err(Error::bad("Choose a person or group to transfer to."));
            };
            let already = match kind {
                "user" => repo.group_id.is_none() && repo.owner_id == target,
                _ => repo.group_id.as_deref() == Some(target),
            };
            if already {
                return Err(Error::bad(format!("{name} already owns this repository.")));
            }
            // Nobody else needs to agree when the giver is also the one who decides.
            let immediate = match kind {
                "user" => target == user.id,
                _ => user.owner,
            };
            if immediate {
                apply(&mut tx, &repo, kind, target).await?;
                json!({"ok":true,"transferred":true,"to":name})
            } else {
                sqlx::query("INSERT INTO skald_transfers(repo_id,to_kind,to_id,requested_by,created) VALUES(?,?,?,?,?)")
                    .bind(&id).bind(kind).bind(target).bind(&user.id).bind(auth::now())
                    .execute(&mut *tx).await?;
                json!({"ok":true,"transferred":false,"to":name})
            }
        }
        "accept" | "decline" | "cancel" => {
            let Some(transfer) = current else {
                return Err(Error::not_found());
            };
            let allowed = if input.action == "cancel" {
                transfer.requested_by == user.id || can_give_away(&repo, &user)
            } else {
                decides(&transfer, &user)
            };
            if !allowed {
                return Err(Error::forbidden());
            }
            if input.action == "accept" {
                apply(&mut tx, &repo, &transfer.to_kind, &transfer.to_id).await?;
            } else {
                sqlx::query("DELETE FROM skald_transfers WHERE repo_id=?")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
            }
            json!({"ok":true,"transferred":input.action == "accept"})
        }
        _ => return Err(Error::bad("Unknown transfer action.")),
    };
    tx.commit().await?;
    state
        .audit(&user.id, &format!("vcs-transfer-{}", input.action), &id)
        .await?;
    Ok(Json(outcome))
}
