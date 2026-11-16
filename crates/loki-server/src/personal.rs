//! Host-owned personal files. SQLite names immutable bodies and is the source of truth.
mod web;

use crate::{Error, Result, State, apps, auth};
use axum::http::StatusCode;
use futures_util::TryStreamExt;
use serde::Serialize;
use std::{
    io::{Read, Seek, Write},
    path::Path,
};
use wasmtime::component::{ComponentType, Lower};
pub(crate) use web::router;

const MIB: u64 = 1024 * 1024;

#[derive(Serialize, ComponentType, Lower)]
#[component(record)]
pub(crate) struct Usage {
    bytes: u64,
    entries: u32,
    #[component(name = "limit-bytes")]
    limit_bytes: u64,
    #[component(name = "max-entries")]
    max_entries: u32,
    #[component(name = "max-file-bytes")]
    max_file_bytes: u64,
}

#[derive(Debug, Serialize, ComponentType, Lower)]
#[component(record)]
pub(crate) struct FileInfo {
    path: String,
    size: u64,
}

#[derive(Serialize, ComponentType, Lower)]
#[component(record)]
pub(crate) struct Page {
    files: Vec<FileInfo>,
    next: Option<String>,
}

pub(crate) async fn recover(data: &Path, db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let root = data.join("personal-files");
    if let Ok(meta) = std::fs::symlink_metadata(&root) {
        anyhow::ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "Unsafe personal files directory"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            anyhow::ensure!(
                meta.file_attributes() & 0x400 == 0,
                "Unsafe personal files directory"
            );
        }
    }
    std::fs::create_dir_all(&root)?;
    crate::platform::private_directory(&root)?;
    let live: std::collections::HashSet<String> =
        sqlx::query_scalar("SELECT blob FROM personal_files")
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| valid_blob(name) && !live.contains(name))
        {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn valid_blob(name: &str) -> bool {
    uuid::Uuid::parse_str(name).is_ok_and(|id| id.to_string() == name)
}

/// The personal files folder, refusing a link or reparse point in its place.
fn directory_path(data: &Path) -> Result<std::path::PathBuf> {
    let path = data.join("personal-files");
    let metadata = path.symlink_metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(Error::forbidden());
        }
    }
    if !metadata.is_dir() {
        return Err(Error::forbidden());
    }
    Ok(path)
}

fn directory(data: &Path) -> Result<cap_std::fs::Dir> {
    let root = cap_std::fs::Dir::open_ambient_dir(data, cap_std::ambient_authority())?;
    let metadata = root.symlink_metadata("personal-files")?;
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(Error::forbidden());
        }
    }
    if !metadata.is_dir() {
        return Err(Error::forbidden());
    }
    Ok(root.open_dir("personal-files")?)
}

fn open(data: &Path, blob: &str) -> Result<std::fs::File> {
    if !valid_blob(blob) {
        return Err(Error::not_found());
    }
    open_body(&directory_path(data)?.join(blob)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Error::not_found()
        } else {
            error.into()
        }
    })
}

/// Opens a stored body so a concurrent replacement or garbage collection can
/// still delete it (Windows otherwise refuses to delete an open file).
pub(crate) fn open_body(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let meta = path.symlink_metadata()?;
    if !meta.is_file() {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        if meta.file_attributes() & 0x400 != 0 {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        options.share_mode(0x7);
    }
    options.open(path)
}

fn checked_path(path: &str) -> Result<()> {
    if path.len() > 240 || path.split('/').any(|part| part.len() > 120) {
        return Err(Error::bad("The personal file path is too long."));
    }
    crate::sync::valid_path(path)
}

pub(crate) async fn allowed(state: &State, app: &str, user: &str, version: i64) -> Result<()> {
    let account: auth::User =
        sqlx::query_as("SELECT * FROM users WHERE id=? AND auth_version=? AND status='approved'")
            .bind(user)
            .bind(version)
            .fetch_optional(&state.0.db)
            .await?
            .ok_or_else(Error::forbidden)?;
    let enabled: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM personal_apps WHERE app_id=?)")
            .bind(app)
            .fetch_one(&state.0.db)
            .await?;
    if !enabled || !auth::allowed(state, Some(&account), app, "access").await? {
        return Err(Error::forbidden());
    }
    Ok(())
}

pub(crate) async fn usage(state: &State, user: &str) -> Result<Usage> {
    let (bytes, entries): (i64, i64) = sqlx::query_as(
        "SELECT coalesce(sum(size),0), count(*) FROM personal_files WHERE user_id=?",
    )
    .bind(user)
    .fetch_one(&state.0.db)
    .await?;
    Ok(Usage {
        bytes: bytes as u64,
        entries: u32::try_from(entries).unwrap_or(u32::MAX),
        // Preserve the WIT record used by installed apps. Maximum values
        // represent the absence of a configured storage cap.
        limit_bytes: u64::MAX,
        max_entries: u32::MAX,
        max_file_bytes: u64::MAX,
    })
}

pub(crate) async fn list(
    state: &State,
    user: &str,
    app: &str,
    prefix: &str,
    after: Option<&str>,
) -> Result<Page> {
    if prefix.len() > 240 || after.is_some_and(|path| path.len() > 240) || !apps::valid_id(app) {
        return Err(Error::bad("Invalid personal file listing."));
    }
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT path,size FROM personal_files WHERE user_id=?1 AND app_id=?2 AND path>?3 AND substr(path,1,length(?4))=?4 ORDER BY path LIMIT 101")
        .bind(user).bind(app).bind(after.unwrap_or("")).bind(prefix).fetch_all(&state.0.db).await?;
    let next = (rows.len() > 100).then(|| rows[99].0.clone());
    let files = rows
        .into_iter()
        .take(100)
        .map(|(path, size)| FileInfo {
            path,
            size: size as u64,
        })
        .collect();
    Ok(Page { files, next })
}

pub(crate) async fn get(
    state: &State,
    user: &str,
    app: &str,
    path: &str,
) -> Result<(std::fs::File, String)> {
    checked_path(path)?;
    let _guard = state.0.personal_files.read().await;
    let blob: String = sqlx::query_scalar(
        "SELECT blob FROM personal_files WHERE user_id=? AND app_id=? AND path=?",
    )
    .bind(user)
    .bind(app)
    .bind(path)
    .fetch_optional(&state.0.db)
    .await?
    .ok_or_else(Error::not_found)?;
    Ok((open(&state.0.data, &blob)?, blob))
}

pub(crate) async fn read(
    state: &State,
    user: &str,
    app: &str,
    path: &str,
    offset: u64,
    length: u32,
) -> Result<Vec<u8>> {
    let (mut file, _) = get(state, user, app, path).await?;
    tokio::task::spawn_blocking(move || {
        file.seek(std::io::SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(u64::from(length).min(MIB))
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    })
    .await?
}

enum Source {
    Bytes(Vec<u8>),
    Upload(std::fs::File),
}

pub(crate) async fn write(
    state: &State,
    user: &str,
    app: &str,
    version: i64,
    path: String,
    bytes: Vec<u8>,
) -> Result<FileInfo> {
    if bytes.len() as u64 > MIB {
        return Err(Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Generated writes are limited to 1 MiB. Import an upload for a larger file.",
        ));
    }
    write_from(state, user, app, version, path, Source::Bytes(bytes)).await
}

pub(crate) async fn import(
    state: &State,
    user: &str,
    app: &str,
    version: i64,
    path: String,
    file: std::fs::File,
) -> Result<FileInfo> {
    write_from(state, user, app, version, path, Source::Upload(file)).await
}

async fn write_from(
    state: &State,
    user: &str,
    app: &str,
    version: i64,
    path: String,
    source: Source,
) -> Result<FileInfo> {
    checked_path(&path)?;
    let size = match &source {
        Source::Bytes(bytes) => bytes.len() as u64,
        Source::Upload(file) => file.metadata()?.len(),
    };
    // Own the entire write in a task so a guest timeout cannot interrupt the commit.
    let (state, user, app) = (state.clone(), user.to_owned(), app.to_owned());
    tokio::spawn(async move {
        let _guard = state.0.personal_files.write().await;
        allowed(&state, &app, &user, version).await?;
        let previous: Option<String> = sqlx::query_scalar("SELECT blob FROM personal_files WHERE user_id=? AND app_id=? AND path=?")
            .bind(&user).bind(&app).bind(&path).fetch_optional(&state.0.db).await?;
        // A logical file cannot also be a directory. Reject portable name collisions.
        if previous.is_none() {
            let mut paths = sqlx::query_scalar::<_, String>("SELECT path FROM personal_files WHERE user_id=? AND app_id=?")
                .bind(&user).bind(&app).fetch(&state.0.db);
            let requested = path.to_lowercase();
            while let Some(existing) = paths.try_next().await? {
                let existing = existing.to_lowercase();
                if existing == requested
                    || existing.strip_prefix(&requested).is_some_and(|suffix| suffix.starts_with('/'))
                    || requested.strip_prefix(&existing).is_some_and(|suffix| suffix.starts_with('/')) {
                    return Err(Error::new(StatusCode::CONFLICT, "That file path conflicts with an existing file."));
                }
            }
        }
        let blob = uuid::Uuid::new_v4().to_string();
        let data = state.0.data.clone();
        let name = blob.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            crate::storage::ensure_disk_space(&data, size)?;
            let dir = directory(&data)?;
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut output = dir.open_with(&name, &options)?;
            let result = (|| -> Result<()> {
                match source {
                    Source::Bytes(bytes) => output.write_all(&bytes)?,
                    Source::Upload(input) => {
                        if std::io::copy(&mut input.take(size + 1), &mut output)? != size {
                            return Err(Error::bad("The upload changed before it was saved."));
                        }
                    }
                }
                output.sync_all()?;
                #[cfg(unix)]
                dir.try_clone()?.into_std_file().sync_all()?;
                Ok(())
            })();
            drop(output);
            if result.is_err() {
                let _ = dir.remove_file(&name);
            }
            result
        }).await??;
        if let Err(error) = async {
            allowed(&state, &app, &user, version).await?;
            sqlx::query("INSERT INTO personal_files(user_id,app_id,path,blob,size) VALUES(?,?,?,?,?) ON CONFLICT(user_id,app_id,path) DO UPDATE SET blob=excluded.blob,size=excluded.size")
                .bind(&user).bind(&app).bind(&path).bind(&blob).bind(size as i64).execute(&state.0.db).await?;
            Ok::<_, Error>(())
        }.await {
            remove_body(&state.0.data, &blob);
            return Err(error);
        }
        if let Some(old) = previous {
            remove_body(&state.0.data, &old);
        }
        Ok(FileInfo { path, size })
    }).await?
}

fn remove_body(data: &Path, blob: &str) {
    if valid_blob(blob)
        && let Err(error) = directory(data).and_then(|dir| Ok(dir.remove_file(blob)?))
    {
        tracing::warn!(%blob, message=%error.message, "could not remove an unreferenced personal file");
    }
}

pub(crate) async fn delete(
    state: &State,
    user: &str,
    app: &str,
    path: &str,
    version: i64,
) -> Result<bool> {
    checked_path(path)?;
    let _guard = state.0.personal_files.write().await;
    allowed(state, app, user, version).await?;
    let removed: Option<String> = sqlx::query_scalar(
        "DELETE FROM personal_files WHERE user_id=? AND app_id=? AND path=? RETURNING blob",
    )
    .bind(user)
    .bind(app)
    .bind(path)
    .fetch_optional(&state.0.db)
    .await?;
    if let Some(blob) = &removed {
        remove_body(&state.0.data, blob);
    }
    Ok(removed.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apps_ignore_legacy_quotas_and_keep_their_own_files() {
        let data = tempfile::tempdir().unwrap();
        let state = State::open(data.path().to_path_buf(), "127.0.0.1:7070".parse().unwrap())
            .await
            .unwrap();
        sqlx::raw_sql("INSERT INTO users(id,username,display_name,password,status,created,owner) VALUES('u','user','User','','approved',1,1); CREATE TABLE personal_quotas(user_id TEXT PRIMARY KEY, mib INTEGER NOT NULL); INSERT INTO personal_quotas VALUES('u',1); INSERT INTO resources(id,kind,name) VALUES('one','app','One'),('two','app','Two'); INSERT INTO apps(id,manifest,secret,port) VALUES('one','{}','',7070),('two','{}','',7070); INSERT INTO personal_apps VALUES('one'),('two');")
            .execute(&state.0.db).await.unwrap();
        let (one, two) = tokio::join!(
            write(
                &state,
                "u",
                "one",
                0,
                "document.txt".into(),
                vec![1; 700_000]
            ),
            write(
                &state,
                "u",
                "two",
                0,
                "document.txt".into(),
                vec![2; 700_000]
            ),
        );
        one.unwrap();
        two.unwrap();
        let used = usage(&state, "u").await.unwrap();
        assert_eq!((used.bytes, used.entries), (1_400_000, 2));
        assert_eq!(
            (used.limit_bytes, used.max_entries, used.max_file_bytes),
            (u64::MAX, u32::MAX, u64::MAX)
        );
        assert!(
            write(&state, "u", "one", 0, "../document.txt".into(), vec![3])
                .await
                .is_err()
        );
        assert_eq!(
            write(&state, "u", "one", 0, "Document.txt/child".into(), vec![3])
                .await
                .unwrap_err()
                .status,
            StatusCode::CONFLICT
        );
        for (app, byte) in [("one", 1), ("two", 2)] {
            assert_eq!(
                read(&state, "u", app, "document.txt", 1, 8).await.unwrap(),
                vec![byte; 8]
            );
        }
        sqlx::query("UPDATE users SET auth_version=1 WHERE id='u'")
            .execute(&state.0.db)
            .await
            .unwrap();
        assert_eq!(
            write(&state, "u", "one", 0, "document.txt".into(), vec![3])
                .await
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
    }
}
