//! How much room repositories get: the server's defaults, and limits the
//! server owner sets for one person's own repositories.

use crate::{Error, Result, Settings, State};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqliteConnection;

const MIB: i64 = 1_048_576;
/// The largest limit, in MiB, the dashboard accepts: 1 TiB per repository, 100 TiB in total.
const MAX_REPO_MIB: i64 = 1_048_576;
const MAX_TOTAL_MIB: i64 = 100 * 1_048_576;

/// The storage a repository may use, in bytes.
pub(super) struct Limits {
    /// The most this one repository may hold.
    pub repo: i64,
    /// The most all repositories on the server may hold together.
    pub server: i64,
    /// For a person's own repository with a limit of their own: that limit, and
    /// what all their own repositories use now.
    pub personal: Option<(i64, i64)>,
}

impl Limits {
    /// How many more bytes fit, given what the repository and the server use now.
    pub fn room(&self, repo_used: i64, server_used: i64) -> i64 {
        let mut room = (self.repo - repo_used).min(self.server - server_used);
        if let Some((limit, used)) = self.personal {
            room = room.min(limit - used);
        }
        room.max(0)
    }
}

/// The limits for a repository owned by `owner`, or by `group` when a group owns it.
/// A group's repositories use the server defaults.
pub(super) async fn limits(
    conn: &mut SqliteConnection,
    settings: &Settings,
    owner: &str,
    group: Option<&str>,
) -> Result<Limits> {
    let mut limits = Limits {
        repo: settings.vcs_repo_mib as i64 * MIB,
        server: settings.vcs_total_mib as i64 * MIB,
        personal: None,
    };
    if group.is_some() {
        return Ok(limits);
    }
    let custom: Option<(Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT repo_bytes,total_bytes FROM vcs_quotas WHERE user_id=?")
            .bind(owner)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((repo, total)) = custom else {
        return Ok(limits);
    };
    if let Some(repo) = repo {
        limits.repo = repo;
    }
    if let Some(total) = total {
        let used: i64 = sqlx::query_scalar(
            "SELECT coalesce(sum(bytes),0) FROM skald_repos WHERE owner_id=? AND group_id IS NULL",
        )
        .bind(owner)
        .fetch_one(&mut *conn)
        .await?;
        limits.personal = Some((total, used));
    }
    Ok(limits)
}

/// Every person's own limits, by user id, in MiB. Blank means the default.
pub(crate) async fn overrides(db: &sqlx::SqlitePool) -> Result<Value> {
    let rows: Vec<(String, Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT user_id,repo_bytes,total_bytes FROM vcs_quotas")
            .fetch_all(db)
            .await?;
    Ok(Value::Object(
        rows.into_iter()
            .map(|(user, repo, total)| {
                (user, json!({"repo_mib":repo.map(|b| b / MIB),"total_mib":total.map(|b| b / MIB)}))
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub(crate) struct Override {
    /// MiB per repository, or none for the server default.
    repo_mib: Option<i64>,
    /// MiB for all the person's own repositories together, or none for no limit of their own.
    total_mib: Option<i64>,
}

/// Sets or clears one person's limits. Clearing both returns them to the defaults.
pub(crate) async fn set_override(state: &State, user: &str, input: Override) -> Result<Value> {
    let valid = |value: Option<i64>, max: i64| value.is_none_or(|mib| (1..=max).contains(&mib));
    if !valid(input.repo_mib, MAX_REPO_MIB) || !valid(input.total_mib, MAX_TOTAL_MIB) {
        return Err(Error::bad(
            "Enter sizes of at least 1 MB, or leave them blank for the default.",
        ));
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?)")
        .bind(user)
        .fetch_one(&state.0.db)
        .await?;
    if !exists {
        return Err(Error::not_found());
    }
    if input.repo_mib.is_none() && input.total_mib.is_none() {
        sqlx::query("DELETE FROM vcs_quotas WHERE user_id=?")
            .bind(user)
            .execute(&state.0.db)
            .await?;
    } else {
        sqlx::query("INSERT INTO vcs_quotas(user_id,repo_bytes,total_bytes) VALUES(?,?,?) ON CONFLICT(user_id) DO UPDATE SET repo_bytes=excluded.repo_bytes,total_bytes=excluded.total_bytes")
            .bind(user)
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
    fn room_is_the_tightest_of_the_repository_server_and_personal_limits() {
        let mut limits = Limits { repo: 100, server: 1000, personal: None };
        assert_eq!(limits.room(40, 500), 60);
        assert_eq!(limits.room(40, 990), 10);
        limits.personal = Some((300, 280));
        assert_eq!(limits.room(40, 500), 20);
        // Already over a limit that was lowered: no room, never negative.
        limits.personal = Some((100, 150));
        assert_eq!(limits.room(40, 500), 0);
    }
}
