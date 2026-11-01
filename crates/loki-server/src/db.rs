use crate::Settings;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::path::Path;

pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    sqlx::raw_sql(include_str!("schema.sql"))
        .execute(&pool)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}
/// One change to databases made by older versions. `schema.sql` already has the
/// result, so new databases skip straight past these.
enum Migration {
    /// Adds a column unless the table has it.
    Column(&'static str, &'static str, &'static str),
    Sql(&'static str),
}
use Migration::{Column, Sql};
/// Applied in order, once each: `PRAGMA user_version` counts how many have run.
/// Databases from before the count start at 0 and may already have some columns,
/// so `Column` checks first. Only append; never reorder or remove a step.
const MIGRATIONS: &[Migration] = &[
    // Credential epochs invalidate stale in-flight authentication after revocation.
    Column("users", "auth_version", "INTEGER NOT NULL DEFAULT 0"),
    Column("sessions", "auth_version", "INTEGER NOT NULL DEFAULT 0"),
    Column("codes", "auth_version", "INTEGER NOT NULL DEFAULT 0"),
    Column("tokens", "auth_version", "INTEGER NOT NULL DEFAULT 0"),
    // Families started by device sign-in refresh without a client secret.
    Column("token_families", "public", "INTEGER NOT NULL DEFAULT 0"),
    // Share roots chosen before paths were simplified kept Windows' `\\?\D:\` form.
    Sql(r"UPDATE shares SET root=substr(root,5) WHERE root LIKE '\\?\%' AND substr(root,6,2)=':\'"),
    Column("apps", "previous_config", "TEXT"),
    Column(
        "shares",
        "excluded_extensions",
        "TEXT NOT NULL DEFAULT '[]'",
    ),
    Column("apps", "lan_bind", "TEXT NOT NULL DEFAULT ''"),
    Column("users", "bio", "TEXT NOT NULL DEFAULT ''"),
    // Tokens issued before families were tracked.
    Sql(
        "INSERT OR IGNORE INTO token_families(id,expires) SELECT family,max(expires) FROM tokens GROUP BY family",
    ),
    // An early Save sync draft paired devices in its own table.
    Sql("DROP TABLE IF EXISTS sync_pairings"),
];
async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    let done: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    let done = usize::try_from(done)?;
    if done >= MIGRATIONS.len() {
        return Ok(());
    }
    for step in &MIGRATIONS[done..] {
        match step {
            Column(table, column, definition) => {
                let exists = sqlx::query(&format!("PRAGMA table_info({table})"))
                    .fetch_all(&mut *tx)
                    .await?
                    .iter()
                    .any(|r| r.get::<String, _>("name") == *column);
                if !exists {
                    sqlx::query(&format!(
                        "ALTER TABLE {table} ADD COLUMN {column} {definition}"
                    ))
                    .execute(&mut *tx)
                    .await?;
                }
            }
            Sql(sql) => {
                sqlx::query(sql).execute(&mut *tx).await?;
            }
        }
    }
    sqlx::query(&format!("PRAGMA user_version={}", MIGRATIONS.len()))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn settings(pool: &SqlitePool) -> anyhow::Result<Settings> {
    let json: Option<String> = sqlx::query_scalar("SELECT json FROM settings WHERE id=1")
        .fetch_optional(pool)
        .await?;
    Ok(match json {
        Some(j) => serde_json::from_str(&j)?,
        None => Settings::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn existing_shares_gain_empty_exclusions_and_keep_them_after_reopening() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("loki.db");
        let pool = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE resources(id TEXT PRIMARY KEY,kind TEXT NOT NULL,name TEXT NOT NULL,visibility TEXT NOT NULL DEFAULT 'restricted'); CREATE TABLE shares(id TEXT PRIMARY KEY REFERENCES resources(id),root TEXT NOT NULL); INSERT INTO resources(id,kind,name) VALUES('old','share','Old share'); INSERT INTO shares(id,root) VALUES('old','kept-folder');")
            .execute(&pool).await.unwrap();
        pool.close().await;
        let pool = open(&path).await.unwrap();
        let share: (String, String) =
            sqlx::query_as("SELECT root,excluded_extensions FROM shares WHERE id='old'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(share, ("kept-folder".into(), "[]".into()));
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        sqlx::query("UPDATE shares SET excluded_extensions='[\"tmp\"]' WHERE id='old'")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let pool = open(&path).await.unwrap();
        let excluded: String =
            sqlx::query_scalar("SELECT excluded_extensions FROM shares WHERE id='old'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(excluded, "[\"tmp\"]");
        pool.close().await;
    }

    #[tokio::test]
    async fn verbatim_share_roots_become_plain_paths() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("loki.db");
        let pool = open(&path).await.unwrap();
        for (id, root) in [
            ("a", r"\\?\D:\Games"),
            ("b", r"\\?\UNC\nas\media"),
            ("c", r"E:\Music"),
        ] {
            sqlx::query("INSERT INTO resources(id,kind,name) VALUES(?,'share',?)")
                .bind(id)
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO shares(id,root) VALUES(?,?)")
                .bind(id)
                .bind(root)
                .execute(&pool)
                .await
                .unwrap();
        }
        // As if written by a version from before migrations were counted.
        sqlx::query("PRAGMA user_version=0")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let pool = open(&path).await.unwrap();
        let roots: Vec<String> = sqlx::query_scalar("SELECT root FROM shares ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        // Network shares are left alone; only drive paths are shortened.
        assert_eq!(roots, [r"D:\Games", r"\\?\UNC\nas\media", r"E:\Music"]);
    }
}
