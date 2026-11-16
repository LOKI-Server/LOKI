use super::{
    Access, DEFAULT_REF, REPO_BYTES, Repo, TOTAL_BYTES, branch, fetch, in_repo, initialize, oid,
    output, repo_dir, require, reserves, resolve, run, size, valid_id, with_input,
};
use crate::{Error, Result, State, auth};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Serialize, sqlx::FromRow)]
pub(super) struct Item {
    #[serde(skip)]
    cursor: i64,
    id: String,
    repo_id: String,
    author_id: String,
    username: String,
    kind: String,
    title: String,
    body: String,
    status: String,
    created: i64,
    // A snapshot can be viewed by the target repo without exposing the fork's id.
    #[serde(skip)]
    source_repo: Option<String>,
    #[serde(skip)]
    source_branch: Option<String>,
    head: Option<String>,
    base: Option<String>,
    approved_head: Option<String>,
    approved_base: Option<String>,
    reviewer_id: Option<String>,
    merged_oid: Option<String>,
}

const ITEMS: &str =
    "SELECT i.rowid AS cursor,i.*,u.username FROM git_items i JOIN users u ON u.id=i.author_id";

#[derive(Deserialize)]
pub(super) struct New {
    kind: String,
    title: String,
    body: String,
    source: Option<String>,
    branch: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Change {
    pub action: String,
    pub body: Option<String>,
    pub head: Option<String>,
    pub base: Option<String>,
}

pub(super) async fn list(state: &State, repo: &Repo, before: i64) -> Result<Value> {
    let mut items: Vec<Item> = sqlx::query_as(&format!(
        "{ITEMS} WHERE i.repo_id=?1 AND i.rowid<?2 ORDER BY i.rowid DESC LIMIT 51"
    ))
    .bind(&repo.id)
    .bind(before)
    .fetch_all(&state.0.db)
    .await?;
    let next = (items.len() > 50).then(|| items[49].cursor);
    items.truncate(50);
    Ok(json!({"items":items,"next":next}))
}

async fn item(state: &State, repo: &Repo, id: &str) -> Result<Item> {
    if !valid_id(id) {
        return Err(Error::not_found());
    }
    let mut item: Item = sqlx::query_as(&format!("{ITEMS} WHERE i.repo_id=? AND i.id=?"))
        .bind(&repo.id)
        .bind(id)
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(Error::not_found)?;
    if let Some(merged) = &item.merged_oid
        && item.status == "open"
    {
        let git = super::checked_binary(state).await?;
        let directory = repo_dir(&state.0.data, &repo.id);
        if resolve(&git, &directory, &format!("refs/loki/merged/{id}"))
            .await
            .as_ref()
            .is_ok_and(|oid| oid == merged)
        {
            sqlx::query("UPDATE git_items SET status='merged' WHERE id=? AND merged_oid=?")
                .bind(id)
                .bind(merged)
                .execute(&state.0.db)
                .await?;
            item.status = "merged".into();
        }
    }
    Ok(item)
}

fn text(value: &str, max: usize, required: bool) -> Result<&str> {
    let value = value.trim();
    if value.len() > max || (required && value.is_empty()) || value.contains('\0') {
        return Err(Error::bad(format!("Enter text up to {max} bytes.")));
    }
    Ok(value)
}

pub(super) async fn create(state: &State, repo: &Repo, user: &str, input: New) -> Result<String> {
    let title = text(&input.title, 160, true)?;
    let body = text(&input.body, 16_384, false)?;
    let id = auth::id();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM git_items WHERE repo_id=?")
        .bind(&repo.id)
        .fetch_one(&state.0.db)
        .await?;
    if count >= 10_000 {
        return Err(Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "This repository reached its issue and pull request limit.",
        ));
    }
    match input.kind.as_str() {
        "issue" => {
            sqlx::query("INSERT INTO git_items(id,repo_id,author_id,kind,title,body,created) VALUES(?,?,?,'issue',?,?,?)")
                .bind(&id).bind(&repo.id).bind(user).bind(title).bind(body).bind(auth::now()).execute(&state.0.db).await?;
        }
        "pull" => {
            let source_id = input
                .source
                .as_deref()
                .filter(|id| !id.is_empty())
                .unwrap_or(&repo.id);
            let source = require(state, source_id, user, Access::Read).await?.0;
            if source.id != repo.id && source.upstream.as_deref() != Some(&repo.id) {
                return Err(Error::bad(
                    "Choose a branch from this repository or a direct fork.",
                ));
            }
            let refname = branch(input.branch.as_deref().unwrap_or_default())?;
            if source.id == repo.id && refname == DEFAULT_REF {
                return Err(Error::bad("Choose a contributor branch."));
            }
            let (snapshot, head, base) = capture(state, repo, &source, &refname).await?;
            let result = async {
                let mut tx = state.0.db.begin().await?;
                sqlx::query("INSERT INTO git_items(id,repo_id,author_id,kind,title,body,created,source_repo,source_branch,head,base) VALUES(?,?,?,'pull',?,?,?,?,?,?,?)")
                    .bind(&id).bind(&repo.id).bind(user).bind(title).bind(body).bind(auth::now()).bind(&source.id).bind(&refname).bind(&head).bind(&base).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO git_pull_snapshots(item_id,snapshot) VALUES(?,?)").bind(&id).bind(&snapshot).execute(&mut *tx).await?;
                tx.commit().await
            }.await;
            if let Err(error) = result {
                discard(&snapshot_path(state, repo, &snapshot)?);
                return Err(error.into());
            }
        }
        _ => return Err(Error::bad("Choose an issue or pull request.")),
    }
    Ok(id)
}

fn snapshot_path(state: &State, repo: &Repo, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        return Err(Error::not_found());
    }
    Ok(repo_dir(&state.0.data, &repo.id)
        .join("loki-pulls")
        .join(id))
}

async fn snapshot(state: &State, repo: &Repo, item: &str) -> Result<PathBuf> {
    let id: String = sqlx::query_scalar("SELECT snapshot FROM git_pull_snapshots WHERE item_id=?")
        .bind(item)
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(Error::not_found)?;
    snapshot_path(state, repo, &id)
}

fn discard(path: &Path) {
    if let Err(error) = std::fs::remove_dir_all(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error,"failed to clean a Git pull request snapshot");
    }
}

async fn capture(
    state: &State,
    target: &Repo,
    source: &Repo,
    reference: &str,
) -> Result<(String, String, String)> {
    let git = super::checked_binary(state).await?;
    let target_dir = repo_dir(&state.0.data, &target.id);
    let source_dir = repo_dir(&state.0.data, &source.id);
    let base = resolve(&git, &target_dir, DEFAULT_REF).await?;
    let head = resolve(&git, &source_dir, reference).await?;
    if head == base {
        return Err(Error::bad("The selected branch has no changes to submit."));
    }
    let id = auth::id();
    let dir = snapshot_path(state, target, &id)?;
    let result = async {
        let source_size = size(&source_dir.join("objects"), REPO_BYTES)?;
        let target_size = size(&target_dir.join("objects"), REPO_BYTES)?;
        reserves(
            &state.0.data,
            Some(&target_dir),
            source_size + target_size + super::MIB,
        )?;
        initialize(&git, &dir).await?;
        fetch(&git, &dir, &target_dir, DEFAULT_REF, "refs/heads/base").await?;
        fetch(&git, &dir, &source_dir, reference, "refs/heads/submission").await?;
        size(&target_dir, REPO_BYTES)?;
        size(&state.0.data.join("git-repos"), TOTAL_BYTES)?;
        // Require common history before sharing an unrelated repository in a pull request.
        run(&git, &dir, &["merge-base", &base, &head], 1024).await?;
        Ok::<_, Error>(())
    }
    .await;
    if let Err(error) = result {
        discard(&dir);
        return Err(error);
    }
    Ok((id, head, base))
}

pub(super) async fn detail(state: &State, repo: &Repo, id: &str) -> Result<Value> {
    let item = item(state, repo, id).await?;
    let comments: Vec<(String,String,String,i64)> = sqlx::query_as("SELECT c.id,u.username,c.body,c.created FROM git_comments c JOIN users u ON u.id=c.author_id WHERE c.item_id=? ORDER BY c.rowid LIMIT 1000")
        .bind(id).fetch_all(&state.0.db).await?;
    let comments: Vec<_> = comments.into_iter().map(|(id,username,body,created)|json!({"id":id,"username":username,"body":body,"created":created})).collect();
    let diff = if item.kind == "pull" {
        let git = super::checked_binary(state).await?;
        let dir = snapshot(state, repo, id).await?;
        let mut command = in_repo(&git, &dir);
        command.args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "refs/heads/base...refs/heads/submission",
            "--",
        ]);
        match output(command, 512 * 1024).await {
            Ok(diff) => Some(String::from_utf8_lossy(&diff).into_owned()),
            Err(error) if error.status == StatusCode::PAYLOAD_TOO_LARGE => {
                Some("This diff exceeds the web preview limit.".into())
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    Ok(json!({"item":item,"comments":comments,"diff":diff}))
}

pub(super) async fn change(
    state: &State,
    repo: &Repo,
    id: &str,
    user: &str,
    role: Access,
    input: Change,
) -> Result<()> {
    let item = item(state, repo, id).await?;
    let own = item.author_id == user;
    let manager = role == Access::Manage;
    match input.action.as_str() {
        "comment" => {
            let body = text(input.body.as_deref().unwrap_or_default(), 8192, true)?;
            let count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM git_comments WHERE item_id=?")
                    .bind(id)
                    .fetch_one(&state.0.db)
                    .await?;
            if count >= 1000 {
                return Err(Error::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "This discussion reached its comment limit.",
                ));
            }
            sqlx::query(
                "INSERT INTO git_comments(id,item_id,author_id,body,created) VALUES(?,?,?,?,?)",
            )
            .bind(auth::id())
            .bind(id)
            .bind(user)
            .bind(body)
            .bind(auth::now())
            .execute(&state.0.db)
            .await?;
        }
        "close" | "reopen" if own || manager => {
            let status = if input.action == "close" {
                "closed"
            } else {
                "open"
            };
            sqlx::query("UPDATE git_items SET status=?,approved_head=NULL,approved_base=NULL,reviewer_id=NULL WHERE id=? AND status!='merged'")
                .bind(status).bind(id).execute(&state.0.db).await?;
        }
        "approve" if manager && item.kind == "pull" && item.status == "open" => {
            if input.head.is_none() || item.head != input.head || item.base != input.base {
                return Err(stale());
            }
            let git = super::checked_binary(state).await?;
            let current = resolve(&git, &repo_dir(&state.0.data, &repo.id), DEFAULT_REF).await?;
            if item.base.as_deref() != Some(&current) {
                return Err(stale());
            }
            sqlx::query("UPDATE git_items SET reviewer_id=?,approved_head=head,approved_base=base WHERE id=?")
                .bind(user).bind(id).execute(&state.0.db).await?;
        }
        "refresh" if own && item.kind == "pull" && item.status == "open" => {
            let source = require(
                state,
                item.source_repo.as_deref().unwrap_or_default(),
                user,
                Access::Read,
            )
            .await?
            .0;
            let reference = item.source_branch.as_deref().ok_or_else(Error::not_found)?;
            let previous = snapshot(state, repo, id).await?;
            let (snapshot, head, base) = capture(state, repo, &source, reference).await?;
            let result = async {
                let mut tx = state.0.db.begin().await?;
                sqlx::query("UPDATE git_items SET head=?,base=?,approved_head=NULL,approved_base=NULL,reviewer_id=NULL,merged_oid=NULL WHERE id=?")
                    .bind(head).bind(base).bind(id).execute(&mut *tx).await?;
                sqlx::query("UPDATE git_pull_snapshots SET snapshot=? WHERE item_id=?").bind(&snapshot).bind(id).execute(&mut *tx).await?;
                tx.commit().await
            }.await;
            if let Err(error) = result {
                discard(&snapshot_path(state, repo, &snapshot)?);
                return Err(error.into());
            }
            discard(&previous);
        }
        "merge" if manager && item.kind == "pull" && item.status == "open" => {
            if item.head != input.head || item.base != input.base {
                return Err(stale());
            }
            merge(state, repo, item).await?
        }
        _ => return Err(Error::forbidden()),
    }
    Ok(())
}

fn stale() -> Error {
    Error::new(
        StatusCode::CONFLICT,
        "The target branch or review changed. Refresh the submission and review it again.",
    )
}

async fn merge(state: &State, repo: &Repo, item: Item) -> Result<()> {
    let path = repo_dir(&state.0.data, &repo.id);
    let git = super::checked_binary(state).await?;
    let base = resolve(&git, &path, DEFAULT_REF).await?;
    let head = item
        .head
        .as_deref()
        .filter(|oid_str| oid(oid_str))
        .ok_or_else(stale)?;
    if item.approved_head.as_deref() != Some(head)
        || item.approved_base.as_deref() != Some(&base)
        || item.base.as_deref() != Some(&base)
    {
        return Err(stale());
    }
    let reviewer = item.reviewer_id.as_deref().ok_or_else(stale)?;
    require(state, &repo.id, reviewer, Access::Manage)
        .await
        .map_err(|_| stale())?;
    let snapshot = snapshot(state, repo, &item.id).await?;
    let tree = run(
        &git,
        &snapshot,
        &["merge-tree", "--write-tree", &base, head],
        16 * 1024,
    )
    .await
    .map_err(|error| {
        if error.status == StatusCode::CONFLICT {
            Error::new(
                StatusCode::CONFLICT,
                "This pull request has merge conflicts.",
            )
        } else {
            error
        }
    })?;
    let tree = String::from_utf8_lossy(&tree).trim().to_owned();
    if !oid(&tree) {
        return Err(stale());
    }
    let mut commit = in_repo(&git, &snapshot);
    commit
        .env("GIT_AUTHOR_NAME", "LOKI")
        .env("GIT_AUTHOR_EMAIL", "loki@localhost")
        .env("GIT_COMMITTER_NAME", "LOKI")
        .env("GIT_COMMITTER_EMAIL", "loki@localhost")
        .args([
            "commit-tree",
            &tree,
            "-p",
            &base,
            "-p",
            head,
            "-m",
            &format!("Merge pull request {}", item.id),
        ]);
    let merged = String::from_utf8_lossy(&output(commit, 1024).await?)
        .trim()
        .to_owned();
    if !oid(&merged) {
        return Err(stale());
    }
    run(
        &git,
        &snapshot,
        &["update-ref", "refs/heads/merge", &merged],
        1024,
    )
    .await?;
    let extra = size(&snapshot.join("objects"), REPO_BYTES)?;
    reserves(&state.0.data, Some(&path), extra)?;
    let staging = format!("refs/loki/staging/{}", item.id);
    run(&git, &path, &["update-ref", "-d", &staging], 1024).await?;
    fetch(&git, &path, &snapshot, "refs/heads/merge", &staging).await?;
    sqlx::query("UPDATE git_items SET merged_oid=? WHERE id=?")
        .bind(&merged)
        .bind(&item.id)
        .execute(&state.0.db)
        .await?;
    with_input(&git,&path,&["update-ref","--stdin"],format!("start\nupdate {DEFAULT_REF} {merged} {base}\ncreate refs/loki/merged/{} {merged}\ndelete {staging} {merged}\nprepare\ncommit\n",item.id)).await?;
    sqlx::query("UPDATE git_items SET status='merged' WHERE id=?")
        .bind(&item.id)
        .execute(&state.0.db)
        .await?;
    Ok(())
}
