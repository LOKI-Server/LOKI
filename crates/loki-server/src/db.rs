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
    // Credential epochs invalidate stale in-flight authentication after revocation.
    for table in ["users", "sessions", "codes", "tokens"] {
        let columns = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&pool)
            .await?;
        if !columns
            .iter()
            .any(|r| r.get::<String, _>("name") == "auth_version")
        {
            sqlx::query(&format!(
                "ALTER TABLE {table} ADD COLUMN auth_version INTEGER NOT NULL DEFAULT 0"
            ))
            .execute(&pool)
            .await?;
        }
    }
    if !sqlx::query("PRAGMA table_info(apps)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "previous_config")
    {
        sqlx::query("ALTER TABLE apps ADD COLUMN previous_config TEXT")
            .execute(&pool)
            .await?;
    }
    if !sqlx::query("PRAGMA table_info(shares)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "excluded_extensions")
    {
        sqlx::query("ALTER TABLE shares ADD COLUMN excluded_extensions TEXT NOT NULL DEFAULT '[]'")
            .execute(&pool)
            .await?;
    }
    if !sqlx::query("PRAGMA table_info(users)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "bio")
    {
        sqlx::query("ALTER TABLE users ADD COLUMN bio TEXT NOT NULL DEFAULT ''")
            .execute(&pool)
            .await?;
    }
    sqlx::query("INSERT OR IGNORE INTO token_families(id,expires) SELECT family,max(expires) FROM tokens GROUP BY family").execute(&pool).await?;
    Ok(pool)
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
}
