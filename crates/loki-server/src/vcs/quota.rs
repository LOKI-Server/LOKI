//! How much room repositories get: the server's defaults, and limits the
//! server owner sets for the repositories one person or one group owns.

use crate::{Error, Result, Settings, State};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqliteConnection;

const MIB: i64 = 1_048_576;
/// The largest limit, in MiB, the dashboard accepts: 1 TiB per repository, 100 TiB in total.
const MAX_REPO_MIB: i64 = 1_048_576;
const MAX_TOTAL_MIB: i64 = 100 * 1_048_576;

/// Who a limit is for: a person, for repositories they own themselves, or a group,
/// for the repositories it owns.
#[derive(Clone, Copy)]
pub(crate) enum Subject {
    User,
    Group,
}

impl Subject {
    /// The table holding this kind's limits, and its key column. Both are fixed text.
    fn table(self) -> (&'static str, &'static str) {
        match self {
            Self::User => ("vcs_quotas", "user_id"),
            Self::Group => ("vcs_group_quotas", "group_id"),
        }
    }
}

/// The storage a repository may use, in bytes.
pub(super) struct Limits {
    /// The most this one repository may hold.
    pub repo: i64,
    /// The most all repositories on the server may hold together.
    pub server: i64,
    /// When the repository's owner has a total of their own: that total, and what
    /// all the repositories they own use now.
    pub owner: Option<(i64, i64)>,
}

impl Limits {
    /// How many more bytes fit, given what the repository and the server use now.
    pub fn room(&self, repo_used: i64, server_used: i64) -> i64 {
        let mut room = (self.repo - repo_used).min(self.server - server_used);
        if let Some((limit, used)) = self.owner {
            room = room.min(limit - used);
        }
        room.max(0)
    }
}

/// The limits for a repository owned by `owner`, or by `group` when a group owns it.
pub(super) async fn limits(
    conn: &mut SqliteConnection,
    settings: &Settings,
    owner: &str,
    group: Option<&str>,
) -> Result<Limits> {
    let mut limits = Limits {
        repo: settings.vcs_repo_mib as i64 * MIB,
        server: settings.vcs_total_mib as i64 * MIB,
        owner: None,
    };
    let (subject, id) = match group {
        Some(group) => (Subject::Group, group),
        None => (Subject::User, owner),
    };
    let (table, key) = subject.table();
    let custom: Option<(Option<i64>, Option<i64>)> =
        sqlx::query_as(&format!("SELECT repo_bytes,total_bytes FROM {table} WHERE {key}=?"))
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((repo, total)) = custom else {
        return Ok(limits);
    };
    if let Some(repo) = repo {
        limits.repo = repo;
    }
    if let Some(total) = total {
        let used: i64 = sqlx::query_scalar(match subject {
            Subject::User => "SELECT coalesce(sum(bytes),0) FROM skald_repos WHERE owner_id=? AND group_id IS NULL",
            Subject::Group => "SELECT coalesce(sum(bytes),0) FROM skald_repos WHERE group_id=?",
        })
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
        limits.owner = Some((total, used));
    }
    Ok(limits)
}

/// Every person's or group's own limits, by id, in MiB. Blank means the default.
pub(crate) async fn overrides(db: &sqlx::SqlitePool, subject: Subject) -> Result<Value> {
    let (table, key) = subject.table();
    let rows: Vec<(String, Option<i64>, Option<i64>)> =
        sqlx::query_as(&format!("SELECT {key},repo_bytes,total_bytes FROM {table}"))
            .fetch_all(db)
            .await?;
    Ok(Value::Object(
        rows.into_iter()
            .map(|(id, repo, total)| {
                (id, json!({"repo_mib":repo.map(|b| b / MIB),"total_mib":total.map(|b| b / MIB)}))
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub(crate) struct Override {
    /// MiB per repository, or none for the server default.
    repo_mib: Option<i64>,
    /// MiB for all the repositories the person or group owns, or none for no total of their own.
    total_mib: Option<i64>,
}

/// Sets or clears the limits of one person or group. Clearing both returns them to
/// the defaults.
pub(crate) async fn set_override(
    state: &State,
    subject: Subject,
    id: &str,
    input: Override,
) -> Result<Value> {
    let valid = |value: Option<i64>, max: i64| value.is_none_or(|mib| (1..=max).contains(&mib));
    if !valid(input.repo_mib, MAX_REPO_MIB) || !valid(input.total_mib, MAX_TOTAL_MIB) {
        return Err(Error::bad(
            "Enter sizes of at least 1 MB, or leave them blank for the default.",
        ));
    }
    let exists: bool = sqlx::query_scalar(match subject {
        Subject::User => "SELECT EXISTS(SELECT 1 FROM users WHERE id=?)",
        Subject::Group => "SELECT EXISTS(SELECT 1 FROM groups WHERE id=?)",
    })
    .bind(id)
    .fetch_one(&state.0.db)
    .await?;
    if !exists {
        return Err(Error::not_found());
    }
    let (table, key) = subject.table();
    if input.repo_mib.is_none() && input.total_mib.is_none() {
        sqlx::query(&format!("DELETE FROM {table} WHERE {key}=?"))
            .bind(id)
            .execute(&state.0.db)
            .await?;
    } else {
        sqlx::query(&format!("INSERT INTO {table}({key},repo_bytes,total_bytes) VALUES(?,?,?) ON CONFLICT({key}) DO UPDATE SET repo_bytes=excluded.repo_bytes,total_bytes=excluded.total_bytes"))
            .bind(id)
            .bind(input.repo_mib.map(|mib| mib * MIB))
            .bind(input.total_mib.map(|mib| mib * MIB))
            .execute(&state.0.db)
            .await?;
    }
    Ok(json!({"ok":true}))
}

#[cfg(test)]
mod tests {
    use super::Limits;

    #[test]
    fn room_is_the_tightest_of_the_repository_server_and_owner_limits() {
        let mut limits = Limits { repo: 100, server: 1000, owner: None };
        assert_eq!(limits.room(40, 500), 60);
        assert_eq!(limits.room(40, 990), 10);
        limits.owner = Some((300, 280));
        assert_eq!(limits.room(40, 500), 20);
        // Already over a limit that was lowered: no room, never negative.
        limits.owner = Some((100, 150));
        assert_eq!(limits.room(40, 500), 0);
    }
}
