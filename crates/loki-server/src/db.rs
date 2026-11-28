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
    /// Removes a column if the table has it.
    DropColumn(&'static str, &'static str),
    Sql(&'static str),
}
use Migration::{Column, DropColumn, Sql};
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
    // Native apps signed people in with an OIDC client named after the app. WASI
    // apps use `component-APP_ID`, and LOKI 0.3 runs nothing else.
    Sql("DELETE FROM clients WHERE id IN (SELECT id FROM apps)"),
    // App and gated-route sign-ins remember the LOKI session they came from, so
    // signing out of LOKI signs out of them too.
    Column("codes", "login_session", "TEXT"),
    Column("gateway_sessions", "login_session", "TEXT"),
    Column("gateway_flows", "next", "TEXT NOT NULL DEFAULT '/'"),
    Column("gateway_flows", "popup", "INTEGER NOT NULL DEFAULT 0"),
    // Where an app came from: `upload` or `registry`. Only registry apps are
    // offered registry updates.
    Column("apps", "source", "TEXT NOT NULL DEFAULT 'upload'"),
    // Again: a development build listed the step above before the gateway_flows
    // columns, so databases it opened counted it as done without adding it.
    Column("apps", "source", "TEXT NOT NULL DEFAULT 'upload'"),
    // The owner's storage quota for an app, in MiB; NULL keeps the app's own.
    Column("apps", "storage_mib", "INTEGER"),
    // Keep existing credentials and IDs while moving native sign-in to the account.
    Sql("INSERT OR IGNORE INTO devices SELECT * FROM sync_devices"),
    Sql("INSERT OR IGNORE INTO devices SELECT * FROM file_devices"),
    Sql("DELETE FROM sync_devices"),
    Sql("DELETE FROM file_devices"),
    Column(
        "git_repos",
        "default_branch",
        "TEXT NOT NULL DEFAULT 'main'",
    ),
    Column("git_repos", "protect_default", "INTEGER NOT NULL DEFAULT 1"),
    Column(
        "git_items",
        "base_ref",
        "TEXT NOT NULL DEFAULT 'refs/heads/main'",
    ),
    Column(
        "shares",
        "share_type",
        "TEXT NOT NULL DEFAULT 'files' CHECK(share_type IN ('files','apps'))",
    ),
    Column(
        "skald_repos",
        "protect_default",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    Sql(
        "INSERT OR IGNORE INTO grants(resource,subject_kind,subject,role)
         SELECT 'vcs-create',subject_kind,subject,role FROM grants WHERE resource='git-create'",
    ),
    Column("skald_repos", "upstream", "TEXT REFERENCES skald_repos(id) ON DELETE SET NULL"),
    Column("skald_repos", "bytes", "INTEGER NOT NULL DEFAULT 0"),
    Column("skald_repos", "object_count", "INTEGER NOT NULL DEFAULT 0"),
    Sql("UPDATE skald_repos SET bytes=(SELECT coalesce(sum(size),0) FROM skald_objects WHERE repo_id=skald_repos.id),
        object_count=(SELECT count(*) FROM skald_objects WHERE repo_id=skald_repos.id)"),
    Sql("CREATE TRIGGER IF NOT EXISTS skald_object_insert AFTER INSERT ON skald_objects BEGIN
        UPDATE skald_repos SET bytes=bytes+new.size,object_count=object_count+1 WHERE id=new.repo_id; END"),
    Sql("CREATE TRIGGER IF NOT EXISTS skald_object_delete AFTER DELETE ON skald_objects BEGIN
        UPDATE skald_repos SET bytes=bytes-old.size,object_count=object_count-1 WHERE id=old.repo_id; END"),
    // Groups can own repositories and carry an icon. A group's repositories need
    // unique names within the group; a person's are unique through UNIQUE(owner_id,name).
    Column("groups", "icon", "INTEGER NOT NULL DEFAULT 0"),
    Column("skald_repos", "group_id", "TEXT REFERENCES groups(id)"),
    Sql("CREATE UNIQUE INDEX IF NOT EXISTS skald_repos_group_name ON skald_repos(group_id,name) WHERE group_id IS NOT NULL"),
    // Connected services and file shares can have an icon of their own.
    Column("resources", "icon", "INTEGER NOT NULL DEFAULT 0"),
    // Rejected accounts are removed a few days after rejection; older ones start counting now.
    Column("users", "rejected_at", "INTEGER"),
    Sql("UPDATE users SET rejected_at=CAST(strftime('%s','now') AS INTEGER) WHERE status='rejected' AND rejected_at IS NULL"),
    Column(
        "shares",
        "supports_updates",
        "INTEGER NOT NULL DEFAULT 0 CHECK(supports_updates IN (0,1))",
    ),
    Sql("UPDATE shares SET supports_updates=1,share_type='files' WHERE share_type='apps'"),
    // Folder rollback needs to know when each earlier version was current. `stashed`
    // marks conflicting uploads that never were; `since` is when a version became
    // current (0: unknown, treated as always).
    Column("sync_history", "since", "INTEGER NOT NULL DEFAULT 0"),
    Column("sync_history", "stashed", "INTEGER NOT NULL DEFAULT 0"),
    // A stash was recorded against a version the device had seen, so it repeats a
    // version number already in history, or uses 0 for a path never seen.
    Sql("UPDATE sync_history SET stashed=1 WHERE reason='conflict' AND (version=0 OR EXISTS(SELECT 1 FROM sync_history o WHERE o.folder=sync_history.folder AND o.path=sync_history.path AND o.version=sync_history.version AND o.id<sync_history.id))"),
    // A version became current when the one before it was displaced.
    Sql("UPDATE sync_history SET since=coalesce((SELECT max(p.created) FROM sync_history p WHERE p.folder=sync_history.folder AND p.path=sync_history.path AND p.version=sync_history.version-1 AND p.stashed=0),0) WHERE stashed=0"),
    // File shares no longer offer update tracking.
    DropColumn("shares", "supports_updates"),
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
                if !exists
                    && (sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?)",
                    )
                    .bind(table)
                    .fetch_one(&mut *tx)
                    .await?)
                {
                    sqlx::query(&format!(
                        "ALTER TABLE {table} ADD COLUMN {column} {definition}"
                    ))
                    .execute(&mut *tx)
                    .await?;
                }
            }
            DropColumn(table, column) => {
                let exists = sqlx::query(&format!("PRAGMA table_info({table})"))
                    .fetch_all(&mut *tx)
                    .await?
                    .iter()
                    .any(|r| r.get::<String, _>("name") == *column);
                if exists {
                    sqlx::query(&format!("ALTER TABLE {table} DROP COLUMN {column}"))
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
    async fn existing_native_devices_keep_tokens_and_ids_when_account_sign_in_is_unified() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("loki.db");
        let pool = open(&path).await.unwrap();
        sqlx::query("INSERT INTO users(id,username,display_name,password,status,created) VALUES('u','user','User','','approved',1)")
            .execute(&pool).await.unwrap();
        for (table, id) in [
            ("sync_devices", "sync-device"),
            ("file_devices", "file-device"),
        ] {
            sqlx::query(&format!("INSERT INTO {table}(id,user_id,name,token,auth_version,created,last_seen) VALUES(?,'u',?,?,0,1,1)"))
                .bind(id).bind(id).bind(crate::auth::digest(id)).execute(&pool).await.unwrap();
        }
        sqlx::query("PRAGMA user_version=20")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let pool = open(&path).await.unwrap();
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id,token FROM devices ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![
                ("file-device".into(), crate::auth::digest("file-device")),
                ("sync-device".into(), crate::auth::digest("sync-device"))
            ]
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sync_devices")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM file_devices")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        pool.close().await;
        let pool = open(&path).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM devices")
                .fetch_one(&pool)
                .await
                .unwrap(),
            2
        );
        pool.close().await;
    }

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
    async fn existing_sync_history_learns_when_versions_began_and_which_were_stashed() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("loki.db");
        let pool = open(&path).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO users(id,username,display_name,password,status,created) VALUES('u','user','User','','approved',1);
             INSERT INTO sync_folders(id,user_id,name,created) VALUES('f','u','Saves',1);
             INSERT INTO sync_history(id,folder,path,version,hash,size,mtime,device,created,reason) VALUES
               (1,'f','a.srm',1,'h1',1,1,'d',100,'replaced'),
               (2,'f','a.srm',2,'h2',1,1,'d',200,'conflict'),
               (3,'f','a.srm',2,'h3',1,1,'e',300,'conflict'),
               (4,'f','b.srm',0,'h4',1,1,'e',400,'conflict'),
               (5,'f','b.srm',3,'h5',1,1,'e',500,'deleted');",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(&format!("PRAGMA user_version={}", MIGRATIONS.len() - 3))
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let pool = open(&path).await.unwrap();
        let rows: Vec<(i64, i64, bool)> =
            sqlx::query_as("SELECT id,since,stashed FROM sync_history ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![
                (1, 0, false),
                // Version 2 began when version 1 was replaced.
                (2, 100, false),
                // A second entry for version 2, and a version 0, were stashed uploads.
                (3, 0, true),
                (4, 0, true),
                // Version 2 of b.srm is not in history (a deletion), so its start is unknown.
                (5, 0, false),
            ]
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn former_app_repositories_become_plain_file_shares() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("loki.db");
        let pool = open(&path).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO resources(id,kind,name) VALUES('games','share','Games');
             INSERT INTO shares(id,root,excluded_extensions,share_type) VALUES('games','game-root','[\"bak\"]','apps');",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Back to before the 0.11 share steps: the update column comes and goes again.
        sqlx::query(&format!("PRAGMA user_version={}", MIGRATIONS.len() - 7))
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let pool = open(&path).await.unwrap();
        let share: (String, String, String) = sqlx::query_as(
            "SELECT root,excluded_extensions,share_type FROM shares WHERE id='games'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            share,
            ("game-root".into(), "[\"bak\"]".into(), "files".into())
        );
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('shares')")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(!columns.iter().any(|c| c == "supports_updates"));
    }

    /// LOKI 0.3.0 databases stop at 17 steps; a development build left some at
    /// 18 without the `source` column. Both gain it.
    #[tokio::test]
    async fn apps_gain_their_source_from_released_and_development_databases() {
        for done in [17, 18] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("loki.db");
            let pool = SqlitePoolOptions::new()
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(&path)
                        .create_if_missing(true),
                )
                .await
                .unwrap();
            sqlx::raw_sql(&format!("CREATE TABLE resources(id TEXT PRIMARY KEY,kind TEXT NOT NULL,name TEXT NOT NULL,visibility TEXT NOT NULL DEFAULT 'restricted'); CREATE TABLE apps(id TEXT PRIMARY KEY REFERENCES resources(id) ON DELETE CASCADE, manifest TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 0, hostname TEXT NOT NULL DEFAULT '', config TEXT NOT NULL DEFAULT '{{}}', port INTEGER NOT NULL, secret TEXT NOT NULL, previous TEXT, error TEXT NOT NULL DEFAULT '', previous_config TEXT, lan_bind TEXT NOT NULL DEFAULT ''); INSERT INTO resources(id,kind,name) VALUES('voter','app','Voter'); INSERT INTO apps(id,manifest,port,secret) VALUES('voter','{{}}',7070,''); PRAGMA user_version={done};"))
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
            let pool = open(&path).await.unwrap();
            let source: String = sqlx::query_scalar("SELECT source FROM apps WHERE id='voter'")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(source, "upload", "from {done} steps");
            pool.close().await;
        }
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
