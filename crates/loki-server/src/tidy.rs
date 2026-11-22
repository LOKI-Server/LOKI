//! Housekeeping when LOKI starts, before anything else can be using these folders:
//! leftover temporary files, old Caddy config snapshots, and app data copies no app
//! can roll back to any more. Each step only logs when it fails, so a stubborn file
//! never stops LOKI from starting.
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

/// Caddy config snapshots kept in `caddy/history`, newest first.
pub const CADDY_SNAPSHOTS: usize = 10;

pub(crate) async fn at_startup(data: &Path, db: &SqlitePool) {
    clear_staging(data);
    if let Err(error) = move_caddy_snapshots(data) {
        tracing::warn!(%error, "could not move old Caddy snapshots out of the backups folder");
    }
    prune_caddy_history(data);
    if let Err(error) = tidy_rollbacks(data, db).await {
        tracing::warn!(%error, "could not tidy app rollback copies");
    }
    if let Err(error) = remove_old_packages(data, db).await {
        tracing::warn!(%error, "could not remove old packages of uninstalled apps");
    }
}

/// An app's previous package stays beside it for rolling back; once the app is
/// uninstalled, nothing can use it. (Uninstalling keeps the app's data on purpose.)
async fn remove_old_packages(data: &Path, db: &SqlitePool) -> anyhow::Result<()> {
    let installed: Vec<String> = sqlx::query_scalar("SELECT id FROM apps").fetch_all(db).await?;
    for entry in std::fs::read_dir(data.join("apps"))?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_suffix(".previous")
            && !installed.iter().any(|app| app == id)
            && entry.file_type()?.is_dir()
            && let Err(error) = std::fs::remove_dir_all(entry.path())
        {
            tracing::warn!(path = %entry.path().display(), %error, "could not remove an old app package");
        }
    }
    Ok(())
}

/// `staging` only holds work in progress, and nothing is in progress yet.
fn clear_staging(data: &Path) {
    let Ok(entries) = std::fs::read_dir(data.join("staging")) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let removed = match entry.file_type() {
            Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        };
        match removed {
            Ok(()) => tracing::info!(path = %path.display(), "removed a leftover temporary file"),
            Err(error) => tracing::warn!(path = %path.display(), %error, "could not remove a leftover temporary file"),
        }
    }
}

/// Earlier versions kept every Caddy snapshot in `backups`, beside the backup zips.
fn move_caddy_snapshots(data: &Path) -> std::io::Result<()> {
    let history = data.join("caddy").join("history");
    for entry in std::fs::read_dir(data.join("backups"))?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("caddy-") || !name.ends_with(".json") || !entry.file_type()?.is_file() {
            continue;
        }
        let target = if name == "caddy-before-adoption.json" {
            data.join("caddy").join("before-adoption.json")
        } else {
            history.join(&name)
        };
        std::fs::create_dir_all(target.parent().unwrap_or(&history))?;
        if target.exists() {
            std::fs::remove_file(entry.path())?;
        } else {
            std::fs::rename(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Keeps the newest [`CADDY_SNAPSHOTS`] snapshots. Their names start with the time.
pub fn prune_caddy_history(data: &Path) {
    let Ok(entries) = std::fs::read_dir(data.join("caddy").join("history")) else {
        return;
    };
    let mut snapshots: Vec<(i64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let time = name.strip_prefix("caddy-")?.split('-').next()?.parse().ok()?;
            Some((time, entry.path()))
        })
        .collect();
    snapshots.sort_by(|a, b| b.cmp(a));
    for (_, path) in snapshots.into_iter().skip(CADDY_SNAPSHOTS) {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), %error, "could not remove an old Caddy snapshot");
        }
    }
}

/// Each app keeps one data copy to roll back to, in `rollback-data`. Copies still
/// in `backups` (from earlier versions) move there; copies no app points at go.
async fn tidy_rollbacks(data: &Path, db: &SqlitePool) -> anyhow::Result<()> {
    let rollbacks = data.join("rollback-data");
    std::fs::create_dir_all(&rollbacks)?;
    let apps: Vec<(String, String)> =
        sqlx::query_as("SELECT id,previous FROM apps WHERE previous IS NOT NULL")
            .fetch_all(db)
            .await?;
    let mut kept = Vec::new();
    for (id, previous) in apps {
        let path = PathBuf::from(&previous);
        let Some(name) = path.file_name().map(|name| name.to_owned()) else {
            continue;
        };
        // Match by folder name, so a data directory that moved still finds its copies.
        let in_backups = data.join("backups").join(&name);
        if in_backups.is_dir() {
            let target = rollbacks.join(&name);
            std::fs::rename(&in_backups, &target)?;
            sqlx::query("UPDATE apps SET previous=? WHERE id=?")
                .bind(target.to_string_lossy().as_ref())
                .bind(&id)
                .execute(db)
                .await?;
        }
        kept.push(name);
    }
    for folder in [data.join("backups"), rollbacks] {
        for entry in std::fs::read_dir(&folder)?.flatten() {
            let name = entry.file_name();
            let is_copy = folder.ends_with("rollback-data") || name.to_string_lossy().starts_with("app-");
            if is_copy
                && entry.file_type()?.is_dir()
                && !kept.contains(&name)
                && let Err(error) = std::fs::remove_dir_all(entry.path())
            {
                tracing::warn!(path = %entry.path().display(), %error, "could not remove an unused app data copy");
            }
        }
    }
    Ok(())
}

/// Deletes an app data copy LOKI made, never anything outside the data directory.
pub(crate) async fn remove_copy(data: &Path, copy: &str) {
    let path = Path::new(copy);
    let ours = ["rollback-data", "backups"].iter().any(|folder| path.starts_with(data.join(folder)) && path != data.join(folder));
    if ours && let Err(error) = tokio::fs::remove_dir_all(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), %error, "could not remove an app data copy");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_clears_staging_and_tidies_the_backups_folder() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        for folder in ["staging/old-backup/sync", "backups", "caddy", "rollback-data/orphan", "apps/notes.previous", "apps/gone.previous", "app-data/gone"] {
            std::fs::create_dir_all(data.join(folder)).unwrap();
        }
        std::fs::write(data.join("staging/old-backup/sync/blob"), b"x").unwrap();
        std::fs::write(data.join("staging/upload.tmp"), b"x").unwrap();
        std::fs::write(data.join("backups/loki-1-a.zip"), b"zip").unwrap();
        std::fs::write(data.join("backups/caddy-before-adoption.json"), b"{}").unwrap();
        for time in 100..115 {
            std::fs::write(data.join(format!("backups/caddy-{time}-x.json")), b"{}").unwrap();
        }
        let kept = data.join("backups/app-notes-1");
        std::fs::create_dir_all(kept.join("files")).unwrap();
        std::fs::create_dir_all(data.join("backups/app-notes-0")).unwrap();

        let db = crate::db::open(&data.join("loki.db")).await.unwrap();
        sqlx::query("INSERT INTO resources(id,kind,name) VALUES('notes','app','Notes')").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO apps(id,manifest,port,secret,previous) VALUES('notes','{}',1,'',?)")
            .bind(kept.to_string_lossy().as_ref())
            .execute(&db)
            .await
            .unwrap();

        at_startup(data, &db).await;

        assert_eq!(std::fs::read_dir(data.join("staging")).unwrap().count(), 0);
        let backups: Vec<_> = std::fs::read_dir(data.join("backups")).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        assert_eq!(backups, ["loki-1-a.zip"]);
        assert!(data.join("caddy/before-adoption.json").is_file());
        let mut history: Vec<_> = std::fs::read_dir(data.join("caddy/history")).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        history.sort();
        assert_eq!(history.len(), CADDY_SNAPSHOTS);
        assert_eq!(history[0], "caddy-105-x.json");
        let moved = data.join("rollback-data/app-notes-1");
        assert!(moved.join("files").is_dir());
        assert!(!data.join("rollback-data/orphan").exists());
        // An installed app keeps its previous package; an uninstalled one's goes, but its data stays.
        assert!(data.join("apps/notes.previous").is_dir());
        assert!(!data.join("apps/gone.previous").exists());
        assert!(data.join("app-data/gone").is_dir());
        let previous: String = sqlx::query_scalar("SELECT previous FROM apps WHERE id='notes'").fetch_one(&db).await.unwrap();
        assert_eq!(PathBuf::from(previous), moved);
    }
}
