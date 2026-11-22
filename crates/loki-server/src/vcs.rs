mod collaboration;

mod credentials;
mod ownership;
mod protocol;
mod quota;
mod web;

use crate::{Error, Result, State};
use axum::http::StatusCode;
use serde::Serialize;
use skald_vcs_core::server::{Conflict, Hosted};
use std::path::{Path, PathBuf};

pub(crate) use ownership::waiting_count as transfers_waiting;
pub(crate) use quota::{overrides as quota_overrides, set_override as set_quota};
pub(crate) use web::{api_router, router};

pub(crate) const PROTOCOL_PATH: &str = "/skald/v1";

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
struct Repo {
    id: String,
    owner_id: String,
    name: String,
    created: i64,
    protect_default: bool,
    upstream: Option<String>,
    /// Set when a group owns the repository; `owner_id` then only records who created it.
    group_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum Access {
    Read,
    Write,
    Manage,
}

pub(crate) fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
}

fn directory(data: &Path, id: &str) -> PathBuf {
    data.join("skald-repos").join(id)
}

async fn find(state: &State, id: &str) -> Result<Repo> {
    if !valid_id(id) {
        return Err(Error::not_found());
    }
    sqlx::query_as(
        "SELECT id,owner_id,name,created,protect_default,upstream,group_id FROM skald_repos WHERE id=?",
    )
    .bind(id)
    .fetch_optional(&state.0.db)
    .await?
    .ok_or_else(Error::not_found)
}

/// The server owner manages every repository. A person who owns a repository manages
/// it; members of a group that owns one can write to it. Grants add to either.
async fn access(db: &sqlx::SqlitePool, repo: &Repo, user: &str) -> Result<Option<Access>> {
    let level: i64 = sqlx::query_scalar("SELECT coalesce((SELECT CASE WHEN owner OR (?4 IS NULL AND id=?2) THEN 3 ELSE max(
            CASE WHEN ?4 IS NOT NULL AND EXISTS(SELECT 1 FROM memberships WHERE user_id=?1 AND group_id=?4) THEN 2 ELSE 0 END,
            coalesce((SELECT max(CASE role WHEN 'manage' THEN 3 WHEN 'write' THEN 2 WHEN 'read' THEN 1 END) FROM grants WHERE resource=?3 AND ((subject_kind='user' AND subject=?1) OR (subject_kind='group' AND subject IN(SELECT group_id FROM memberships WHERE user_id=?1)))),0)
        ) END FROM users WHERE id=?1 AND status='approved'),0)")
        .bind(user).bind(&repo.owner_id).bind(&repo.id).bind(&repo.group_id).fetch_one(db).await?;
    Ok(match level {
        3 => Some(Access::Manage),
        2 => Some(Access::Write),
        1 => Some(Access::Read),
        _ => None,
    })
}

async fn require(state: &State, id: &str, user: &str, minimum: Access) -> Result<(Repo, Access)> {
    let repo = find(state, id).await?;
    let role = access(&state.0.db, &repo, user)
        .await?
        .ok_or_else(Error::not_found)?;
    if role < minimum {
        return Err(Error::forbidden());
    }
    Ok((repo, role))
}

/// Whether the repository is waiting for someone to accept or decline a transfer.
async fn transfer_pending(db: &sqlx::SqlitePool, id: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM skald_transfers WHERE repo_id=?)")
            .bind(id)
            .fetch_one(db)
            .await?,
    )
}

/// Refuses code changes while a transfer waits for a decision.
async fn ensure_unpaused(db: &sqlx::SqlitePool, id: &str) -> Result<()> {
    if transfer_pending(db, id).await? {
        return Err(Error::new(
            StatusCode::LOCKED,
            "This repository is waiting for a transfer to be accepted or declined. Changes are paused until then.",
        ));
    }
    Ok(())
}

/// Who may give a repository away or delete it: the server owner, or the person who owns it.
fn can_give_away(repo: &Repo, user: &crate::auth::User) -> bool {
    user.owner || (repo.group_id.is_none() && repo.owner_id == user.id)
}

fn host(state: &State, id: &str) -> Result<Hosted> {
    Hosted::open(&directory(&state.0.data, id)).map_err(Error::from)
}

fn invalid(error: anyhow::Error) -> Error {
    if error.is::<Conflict>() {
        Error::new(
            StatusCode::CONFLICT,
            "Reference or lock changed. Refresh and retry.",
        )
    } else if error.is::<std::io::Error>() {
        error.into()
    } else {
        Error::bad(error.to_string())
    }
}

pub(crate) fn backup_entry(path: &str) -> anyhow::Result<()> {
    let (id, rest) = path
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("Invalid Skald backup entry"))?;
    anyhow::ensure!(valid_id(id), "Invalid Skald backup repository");
    if matches!(rest, "refs.json" | "writer.lock") {
        return Ok(());
    }
    let object = rest
        .strip_prefix("objects/")
        .and_then(|p| p.split_once('/'));
    anyhow::ensure!(
        object.is_some_and(|(shard, tail)| shard.len() == 3
            && format!("{shard}{tail}")
                .parse::<skald_vcs_core::Id>()
                .is_ok()),
        "Invalid Skald backup object"
    );
    Ok(())
}

pub(crate) async fn verify_restore(data: &Path, db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM skald_repos")
        .fetch_all(db)
        .await?;
    let records: Vec<(String, String)> = sqlx::query_as(
        "SELECT repo_id,head FROM skald_items WHERE head IS NOT NULL
         UNION SELECT repo_id,base FROM skald_items WHERE base IS NOT NULL",
    )
    .fetch_all(db)
    .await?;
    let mut roots = std::collections::HashMap::<String, Vec<skald_vcs_core::Id>>::new();
    for (repo, id) in records {
        roots.entry(repo).or_default().push(id.parse()?);
    }
    let data = data.to_owned();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        for id in ids {
            anyhow::ensure!(valid_id(&id), "Invalid Skald repository record");
            let repo = directory(&data, &id);
            // Empty repositories have no object directory entry in file-only backups.
            std::fs::create_dir_all(repo.join("objects"))?;
            Hosted::open(&repo)?.verify_with(roots.remove(&id).unwrap_or_default())?;
        }
        Ok(())
    })
    .await?
}

pub(crate) fn backup_copy(source: &Path, destination: &Path, ids: &[String]) -> anyhow::Result<()> {
    for id in ids {
        anyhow::ensure!(valid_id(id), "Invalid Skald repository record");
        let source = directory(source, id);
        let dest = directory(destination, id);
        let host = Hosted::open(&source)?;
        std::fs::create_dir_all(&dest)?;
        std::fs::copy(source.join("refs.json"), dest.join("refs.json"))?;
        host.store().backup_to(&dest.join("objects"))?;
    }
    Ok(())
}
