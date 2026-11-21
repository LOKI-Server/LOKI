use super::{Access, host, invalid, require, valid_id, web::actor};
use crate::{Error, Result, State, auth};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State as ExtractState},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use skald_vcs_core::{Id, protocol::Update, reference};
use std::collections::BTreeSet;

pub(super) fn routes(router: Router<State>, base: &str) -> Router<State> {
    router
        .route(&format!("{base}/{{id}}/items"), get(list).post(create))
        .route(
            &format!("{base}/{{id}}/items/{{item}}"),
            get(detail).post(change),
        )
}

#[derive(Serialize, sqlx::FromRow)]
struct Item {
    id: String,
    author_id: String,
    username: String,
    kind: String,
    title: String,
    body: String,
    status: String,
    created: i64,
    source_branch: Option<String>,
    base_ref: Option<String>,
    head: Option<String>,
    base: Option<String>,
    approved_head: Option<String>,
    reviewer_id: Option<String>,
}

async fn find(state: &State, id: &str, item: &str) -> Result<Item> {
    if !valid_id(item) {
        return Err(Error::not_found());
    }
    sqlx::query_as("SELECT i.*,u.username FROM skald_items i JOIN users u ON u.id=i.author_id WHERE i.repo_id=? AND i.id=?")
        .bind(id).bind(item).fetch_optional(&state.0.db).await?.ok_or_else(Error::not_found)
}

#[derive(Deserialize)]
struct Page {
    before: Option<i64>,
}

async fn list(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    Query(page): Query<Page>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    require(&state, &id, &user.id, Access::Read).await?;
    let mut rows: Vec<(i64, String, String, String, String)> = sqlx::query_as(
        "SELECT seq,id,kind,title,status FROM skald_items WHERE repo_id=? AND seq<? ORDER BY seq DESC LIMIT 51"
    ).bind(id).bind(page.before.unwrap_or(i64::MAX)).fetch_all(&state.0.db).await?;
    let next = (rows.len() > 50).then(|| {
        rows.pop();
        rows[49].0
    });
    Ok(Json(
        json!({"items":rows.into_iter().map(|(_, id, kind, title, status)|
        json!({"id":id,"kind":kind,"title":title,"status":status})).collect::<Vec<_>>(),"next":next}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct New {
    kind: String,
    title: String,
    body: String,
    branch: Option<String>,
}

fn submission(state: &State, repo: &str, branch: &str) -> Result<(String, Id, Id)> {
    reference(branch).map_err(invalid)?;
    let host = host(state, repo)?;
    let refs = host.refs()?;
    if branch == refs.default_branch {
        return Err(Error::bad(
            "Choose a source branch different from the default branch.",
        ));
    }
    let base = *refs
        .branches
        .get(&refs.default_branch)
        .ok_or_else(|| Error::bad("Push the first commit to the default branch."))?;
    let head = *refs.branches.get(branch).ok_or_else(Error::not_found)?;
    if base == head {
        return Err(Error::bad("The branches have the same revision."));
    }
    Ok((refs.default_branch, base, head))
}

async fn create(
    ExtractState(state): ExtractState<State>,
    Path(id): Path<String>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<New>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.vcs_files.write().await;
    require(&state, &id, &user.id, Access::Read).await?;
    let title = input.title.trim();
    if title.is_empty()
        || title.len() > 640
        || title.chars().count() > 160
        || title.chars().any(char::is_control)
        || input.body.len() > 12_000
    {
        return Err(Error::bad(
            "Enter a title and description within their size limits.",
        ));
    }
    auth::rate(&state, format!("vcs-item:{}", user.id), 60, 3600).await?;
    let (branch, base_ref, base, head) = match input.kind.as_str() {
        "issue" if input.branch.is_none() => (None, None, None, None),
        "pull" => {
            let branch = input
                .branch
                .as_deref()
                .ok_or_else(|| Error::bad("Choose a source branch."))?;
            let (base_ref, base, head) = submission(&state, &id, branch)?;
            (
                Some(branch),
                Some(base_ref),
                Some(base.to_string()),
                Some(head.to_string()),
            )
        }
        _ => return Err(Error::bad("Choose an issue or a pull request.")),
    };
    let item = auth::id();
    sqlx::query("INSERT INTO skald_items(id,repo_id,author_id,kind,title,body,created,source_branch,base_ref,base,head) VALUES(?,?,?,?,?,?,?,?,?,?,?)")
        .bind(&item).bind(&id).bind(&user.id).bind(input.kind).bind(title).bind(input.body)
        .bind(auth::now()).bind(branch).bind(base_ref).bind(base).bind(head).execute(&state.0.db).await?;
    state.audit(&user.id, "vcs-item-created", &item).await?;
    Ok(Json(json!({"id":item})))
}

#[derive(Serialize, sqlx::FromRow)]
struct Comment {
    username: String,
    body: String,
    created: i64,
}

async fn detail(
    ExtractState(state): ExtractState<State>,
    Path((id, key)): Path<(String, String)>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let guard = state.0.vcs_files.clone().read_owned().await;
    let (_, role) = require(&state, &id, &user.id, Access::Read).await?;
    let item = find(&state, &id, &key).await?;
    let comments: Vec<Comment> = sqlx::query_as(
        "SELECT u.username,c.body,c.created FROM skald_comments c JOIN users u ON u.id=c.author_id WHERE c.item_id=? ORDER BY c.seq LIMIT 200"
    ).bind(&key).fetch_all(&state.0.db).await?;
    let diff = if let (Some(base), Some(head)) = (&item.base, &item.head) {
        let host = host(&state, &id)?;
        let base = base.parse()?;
        let head = head.parse()?;
        Some(
            tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                let _guard = guard;
                let a = host.store().snapshot(Some(base))?;
                let b = host.store().snapshot(Some(head))?;
                let mut lines = Vec::new();
                let mut patch_bytes = 0;
                for path in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
                    let change = match (a.get(path), b.get(path)) {
                        (None, Some(_)) => "Added",
                        (Some(_), None) => "Removed",
                        (Some(a), Some(b)) if a != b => "Modified",
                        _ => continue,
                    };
                    if lines.len() == 500 {
                        lines.push(String::from("More changed paths follow."));
                        break;
                    }
                    lines.push(format!("{change} {path}"));
                    if lines.len() <= 25 && patch_bytes < 128 * 1024 {
                        use skald_vcs_core::text::{file_text, unified_patch};
                        let left = file_text(host.store(), a.get(path).copied())?;
                        let right = file_text(host.store(), b.get(path).copied())?;
                        if let Some((left, right)) = left.zip(right) {
                            let patch = unified_patch(path, &left, &right);
                            if !patch.is_empty() && patch_bytes + patch.len() <= 128 * 1024 {
                                patch_bytes += patch.len();
                                if let Some(line) = lines.last_mut() {
                                    line.push('\n');
                                    line.push_str(&patch);
                                }
                            }
                        }
                    }
                }
                Ok(lines.join("\n"))
            })
            .await??,
        )
    } else {
        None
    };
    Ok(Json(
        json!({"item":item,"comments":comments,"role":role,"diff":diff}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    action: String,
    body: Option<String>,
    head: Option<String>,
    base: Option<String>,
}

fn changed() -> Error {
    Error::new(
        StatusCode::CONFLICT,
        "The submission or default branch changed. Refresh and review the new revisions.",
    )
}

async fn change(
    ExtractState(state): ExtractState<State>,
    Path((id, key)): Path<(String, String)>,
    headers: HeaderMap,
    native: Option<Extension<auth::User>>,
    Json(input): Json<Change>,
) -> Result<Json<Value>> {
    let user = actor(&state, &headers, native).await?;
    let _guard = state.0.vcs_files.write().await;
    let (_, role) = require(&state, &id, &user.id, Access::Read).await?;
    let item = find(&state, &id, &key).await?;
    let mine = item.author_id == user.id;
    match input.action.as_str() {
        "comment" => {
            let body = input.body.as_deref().unwrap_or_default().trim();
            if body.is_empty() || body.len() > 6000 {
                return Err(Error::bad("Enter a comment up to 6,000 bytes."));
            }
            auth::rate(&state, format!("vcs-comment:{}", user.id), 120, 3600).await?;
            let inserted = sqlx::query(
                "INSERT INTO skald_comments(item_id,author_id,body,created) SELECT ?1,?2,?3,?4
                 WHERE (SELECT count(*) FROM skald_comments WHERE item_id=?1)<200",
            )
            .bind(&key)
            .bind(&user.id)
            .bind(body)
            .bind(auth::now())
            .execute(&state.0.db)
            .await?;
            if inserted.rows_affected() == 0 {
                return Err(Error::bad("Discussion reached its comment limit."));
            }
        }
        "close" | "reopen" if (mine || role == Access::Manage) && item.status != "merged" => {
            let status = if input.action == "close" {
                "closed"
            } else {
                "open"
            };
            sqlx::query(
                "UPDATE skald_items SET status=?,approved_head=NULL,reviewer_id=NULL WHERE id=?",
            )
            .bind(status)
            .bind(&key)
            .execute(&state.0.db)
            .await?;
        }
        "refresh" if mine && item.kind == "pull" && item.status == "open" => {
            let branch = item.source_branch.as_deref().ok_or_else(Error::not_found)?;
            let (base_ref, base, head) = submission(&state, &id, branch)?;
            sqlx::query("UPDATE skald_items SET base_ref=?,base=?,head=?,approved_head=NULL,reviewer_id=NULL WHERE id=?")
                .bind(base_ref).bind(base.to_string()).bind(head.to_string()).bind(&key).execute(&state.0.db).await?;
        }
        "approve" | "merge"
            if role == Access::Manage && item.kind == "pull" && item.status == "open" =>
        {
            if input.head != item.head || input.base != item.base {
                return Err(changed());
            }
            let host = host(&state, &id)?;
            let refs = host.refs()?;
            let base = item.base.as_deref().ok_or_else(changed)?.parse::<Id>()?;
            let head = item.head.as_deref().ok_or_else(changed)?.parse::<Id>()?;
            if item.base_ref.as_deref() != Some(&refs.default_branch) {
                return Err(changed());
            }
            let current = refs.branches.get(&refs.default_branch).copied();
            if input.action == "approve" {
                if current != Some(base) {
                    return Err(changed());
                }
                sqlx::query("UPDATE skald_items SET approved_head=?,reviewer_id=? WHERE id=?")
                    .bind(head.to_string())
                    .bind(&user.id)
                    .bind(&key)
                    .execute(&state.0.db)
                    .await?;
            } else {
                if item.approved_head != item.head {
                    return Err(Error::forbidden());
                }
                let reviewer = item.reviewer_id.as_deref().ok_or_else(changed)?;
                require(&state, &id, reviewer, Access::Manage)
                    .await
                    .map_err(|_| changed())?;
                if current != Some(head) {
                    if current != Some(base) {
                        return Err(changed());
                    }
                    host.update(
                        false,
                        &refs.default_branch,
                        Update {
                            expected: Some(base),
                            target: Some(head),
                        },
                    )
                    .map_err(invalid)?;
                }
                sqlx::query("UPDATE skald_items SET status='merged' WHERE id=?")
                    .bind(&key)
                    .execute(&state.0.db)
                    .await?;
            }
        }
        _ => return Err(Error::forbidden()),
    }
    state
        .audit(&user.id, &format!("vcs-{}", input.action), &key)
        .await?;
    Ok(Json(json!({"ok":true})))
}

pub(super) async fn submission_roots(state: &State, repo: &str) -> Result<Vec<Id>> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT head FROM skald_items WHERE repo_id=? AND head IS NOT NULL
         UNION SELECT base FROM skald_items WHERE repo_id=? AND base IS NOT NULL",
    )
    .bind(repo)
    .fetch_all(&state.0.db)
    .await?;
    Ok(ids
        .into_iter()
        .map(|id| id.parse())
        .collect::<anyhow::Result<_>>()?)
}
