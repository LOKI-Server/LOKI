//! Folders, the change feed, commits, and version history.
use super::{Caller, blobs, browser, caller, max_file, valid_hash, valid_name, valid_path};
use crate::{Error, Result, State, apps, auth};
use axum::{
    Json,
    extract::{Path as RoutePath, Query, State as ExtractState},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{FromRow, Sqlite, Transaction};
use std::sync::atomic::Ordering;

const MAX_FOLDERS: i64 = 200;
const MAX_CHANGES: usize = 1000;

pub async fn discovery(ExtractState(state): ExtractState<State>) -> Json<Value> {
    Json(json!({
        "protocol": 1,
        "server": env!("CARGO_PKG_VERSION"),
        "name": state.0.settings.read().await.name,
        "hash": "sha256",
        "max_file_bytes": max_file(&state).await,
        "pairing": true,
    }))
}

#[derive(FromRow)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub keep_versions: i64,
    pub seq: i64,
    pub created: i64,
}
impl Folder {
    fn json(&self) -> Value {
        json!({"id":self.id,"name":self.name,"kind":self.kind,"keep_versions":self.keep_versions,"seq":self.seq,"created":self.created})
    }
}
/// A folder belonging to `user`. Other users' folders are indistinguishable from none.
pub async fn folder(state: &State, user: &str, id: &str) -> Result<Folder> {
    sqlx::query_as(
        "SELECT id,name,kind,keep_versions,seq,created FROM sync_folders WHERE id=? AND user_id=?",
    )
    .bind(id)
    .bind(user)
    .fetch_optional(&state.0.db)
    .await?
    .ok_or_else(|| {
        Error::coded(
            StatusCode::NOT_FOUND,
            "unknown_folder",
            "This folder does not exist.",
        )
    })
}

pub async fn folders(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let Caller { user, .. } = caller(&state, &headers).await?;
    let rows: Vec<Folder> = sqlx::query_as("SELECT id,name,kind,keep_versions,seq,created FROM sync_folders WHERE user_id=? ORDER BY name COLLATE NOCASE")
        .bind(&user.id)
        .fetch_all(&state.0.db)
        .await?;
    Ok(Json(json!(
        rows.iter().map(Folder::json).collect::<Vec<_>>()
    )))
}
#[derive(Deserialize)]
pub struct FolderInput {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    keep_versions: Option<i64>,
}
pub async fn create_folder(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Json(input): Json<FolderInput>,
) -> Result<(StatusCode, Json<Value>)> {
    let Caller { user, .. } = caller(&state, &headers).await?;
    let folder = new_folder(
        &state,
        &user.id,
        input.name.as_deref().unwrap_or(""),
        input.kind.as_deref().unwrap_or("generic"),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(folder)))
}
pub async fn update_folder(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Json(input): Json<FolderInput>,
) -> Result<Json<Value>> {
    let Caller { user, .. } = caller(&state, &headers).await?;
    Ok(Json(
        change_folder(
            &state,
            &user.id,
            &id,
            input.name.as_deref(),
            input.keep_versions,
        )
        .await?,
    ))
}
pub async fn new_folder(state: &State, user: &str, name: &str, kind: &str) -> Result<Value> {
    let name = valid_name(name, 100)?;
    if !apps::valid_id(kind) {
        return Err(Error::bad(
            "Folder kinds use lowercase letters, digits, and hyphens, such as retroarch-saves.",
        ));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sync_folders WHERE user_id=?")
        .bind(user)
        .fetch_one(&state.0.db)
        .await?;
    if count >= MAX_FOLDERS {
        return Err(Error::bad(format!(
            "You can have at most {MAX_FOLDERS} sync folders."
        )));
    }
    let id = auth::id();
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO sync_folders(id,user_id,name,kind,created) VALUES(?,?,?,?,?)",
    )
    .bind(&id)
    .bind(user)
    .bind(name)
    .bind(kind)
    .bind(auth::now())
    .execute(&state.0.db)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(Error::coded(
            StatusCode::CONFLICT,
            "exists",
            "You already have a folder with this name.",
        ));
    }
    // Other devices watching this person's folders learn about the new one.
    state.wake_sync();
    Ok(folder(state, user, &id).await?.json())
}
pub async fn change_folder(
    state: &State,
    user: &str,
    id: &str,
    name: Option<&str>,
    keep_versions: Option<i64>,
) -> Result<Value> {
    folder(state, user, id).await?;
    if let Some(name) = name {
        let renamed =
            sqlx::query("UPDATE OR IGNORE sync_folders SET name=? WHERE id=? AND user_id=?")
                .bind(valid_name(name, 100)?)
                .bind(id)
                .bind(user)
                .execute(&state.0.db)
                .await?;
        if renamed.rows_affected() != 1 {
            return Err(Error::coded(
                StatusCode::CONFLICT,
                "exists",
                "You already have a folder with this name.",
            ));
        }
    }
    if let Some(keep) = keep_versions {
        if !(1..=1000).contains(&keep) {
            return Err(Error::bad(
                "Keep between 1 and 1,000 versions of each file.",
            ));
        }
        let mut tx = state.0.db.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE sync_folders SET keep_versions=? WHERE id=?")
            .bind(keep)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        prune(&mut tx, id, None, keep).await?;
        tx.commit().await?;
    }
    Ok(folder(state, user, id).await?.json())
}

#[derive(FromRow)]
struct Current {
    version: i64,
    hash: Option<String>,
    size: i64,
    mtime: i64,
    device: String,
    seq: i64,
}
impl Current {
    fn json(&self) -> Value {
        json!({"version":self.version,"hash":self.hash,"size":self.size,"mtime":self.mtime,"device":self.device,"seq":self.seq})
    }
}
async fn current(
    tx: &mut Transaction<'_, Sqlite>,
    folder: &str,
    path: &str,
) -> Result<Option<Current>> {
    Ok(sqlx::query_as(
        "SELECT version,hash,size,mtime,device,seq FROM sync_files WHERE folder=? AND path=?",
    )
    .bind(folder)
    .bind(path)
    .fetch_optional(&mut **tx)
    .await?)
}
#[allow(clippy::too_many_arguments)]
async fn record_history(
    tx: &mut Transaction<'_, Sqlite>,
    folder: &str,
    path: &str,
    version: i64,
    hash: Option<&str>,
    size: i64,
    mtime: i64,
    device: &str,
    reason: &str,
) -> Result<i64> {
    Ok(sqlx::query("INSERT INTO sync_history(folder,path,version,hash,size,mtime,device,created,reason,resolved) VALUES(?,?,?,?,?,?,?,?,?,?)")
        .bind(folder)
        .bind(path)
        .bind(version)
        .bind(hash)
        .bind(size)
        .bind(mtime)
        .bind(device)
        .bind(auth::now())
        .bind(reason)
        .bind(reason != "conflict")
        .execute(&mut **tx)
        .await?
        .last_insert_rowid())
}
/// Writes the next version of a path and returns its folder sequence number.
#[allow(clippy::too_many_arguments)]
async fn write_version(
    tx: &mut Transaction<'_, Sqlite>,
    folder: &str,
    path: &str,
    version: i64,
    hash: Option<&str>,
    size: i64,
    mtime: i64,
    device: &str,
) -> Result<i64> {
    let seq: i64 = sqlx::query_scalar("UPDATE sync_folders SET seq=seq+1 WHERE id=? RETURNING seq")
        .bind(folder)
        .fetch_one(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO sync_files(folder,path,version,hash,size,mtime,device,seq,updated) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(folder,path) DO UPDATE SET version=excluded.version,hash=excluded.hash,size=excluded.size,mtime=excluded.mtime,device=excluded.device,seq=excluded.seq,updated=excluded.updated")
        .bind(folder)
        .bind(path)
        .bind(version)
        .bind(hash)
        .bind(size)
        .bind(mtime)
        .bind(device)
        .bind(seq)
        .bind(auth::now())
        .execute(&mut **tx)
        .await?;
    Ok(seq)
}
/// Keeps the newest `keep` resolved versions of each path. Unresolved conflicts stay.
async fn prune(
    tx: &mut Transaction<'_, Sqlite>,
    folder: &str,
    path: Option<&str>,
    keep: i64,
) -> Result<()> {
    sqlx::query("DELETE FROM sync_history WHERE id IN (SELECT id FROM (SELECT id,row_number() OVER (PARTITION BY path ORDER BY id DESC) n FROM sync_history WHERE folder=?1 AND resolved=1 AND (?2 IS NULL OR path=?2)) WHERE n>?3)")
        .bind(folder)
        .bind(path)
        .bind(keep)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
fn like_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
/// Paths that cannot coexist on a case-insensitive or ordinary filesystem: the same
/// name in another case, or a file where another live path needs a folder.
async fn clash(
    tx: &mut Transaction<'_, Sqlite>,
    folder: &str,
    path: &str,
) -> Result<Option<(&'static str, String)>> {
    let other: Option<String> = sqlx::query_scalar("SELECT path FROM sync_files WHERE folder=? AND hash IS NOT NULL AND path!=? AND path LIKE ? ESCAPE '\\' LIMIT 1")
        .bind(folder)
        .bind(path)
        .bind(like_escape(path))
        .fetch_optional(&mut **tx)
        .await?;
    if let Some(other) = other {
        return Ok(Some(("case_conflict", other)));
    }
    let mut ancestor = String::new();
    for part in path.split('/').take(path.split('/').count() - 1) {
        if !ancestor.is_empty() {
            ancestor.push('/');
        }
        ancestor.push_str(part);
        let file: Option<String> = sqlx::query_scalar("SELECT path FROM sync_files WHERE folder=? AND hash IS NOT NULL AND path LIKE ? ESCAPE '\\' LIMIT 1")
            .bind(folder)
            .bind(like_escape(&ancestor))
            .fetch_optional(&mut **tx)
            .await?;
        if let Some(file) = file {
            return Ok(Some(("path_conflict", file)));
        }
    }
    let inside: Option<String> = sqlx::query_scalar("SELECT path FROM sync_files WHERE folder=? AND hash IS NOT NULL AND path LIKE ? ESCAPE '\\' LIMIT 1")
        .bind(folder)
        .bind(format!("{}/%", like_escape(path)))
        .fetch_optional(&mut **tx)
        .await?;
    Ok(inside.map(|p| ("path_conflict", p)))
}

#[derive(Deserialize)]
pub struct ChangesQuery {
    #[serde(default)]
    since: i64,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    wait: i64,
}
/// path, version, hash, size, mtime, device, seq
type FeedRow = (String, i64, Option<String>, i64, i64, String, i64);
struct Feed {
    seq: i64,
    more: bool,
    reset: bool,
    changes: Vec<Value>,
}
async fn read_feed(state: &State, user: &str, id: &str, since: i64, limit: i64) -> Result<Feed> {
    let mut tx = state.0.db.begin().await?;
    let seq: i64 = sqlx::query_scalar("SELECT seq FROM sync_folders WHERE id=? AND user_id=?")
        .bind(id)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            Error::coded(
                StatusCode::NOT_FOUND,
                "unknown_folder",
                "This folder does not exist.",
            )
        })?;
    // A cursor from the future means the server was restored; start over.
    let reset = since > seq || since < 0;
    let since = if reset { 0 } else { since };
    let rows: Vec<FeedRow> = sqlx::query_as("SELECT path,version,hash,size,mtime,device,seq FROM sync_files WHERE folder=? AND seq>? ORDER BY seq LIMIT ?")
        .bind(id)
        .bind(since)
        .bind(limit + 1)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let more = rows.len() as i64 > limit;
    let changes: Vec<Value> = rows
        .into_iter()
        .take(limit as usize)
        .map(|(path, version, hash, size, mtime, device, seq)| json!({"path":path,"version":version,"hash":hash,"size":size,"mtime":mtime,"device":device,"seq":seq}))
        .collect();
    let seq = if more {
        changes
            .last()
            .and_then(|c| c["seq"].as_i64())
            .unwrap_or(seq)
    } else {
        seq
    };
    Ok(Feed {
        seq,
        more,
        reset,
        changes,
    })
}
pub async fn changes(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(query): Query<ChangesQuery>,
) -> Result<Json<Value>> {
    let Caller { user, .. } = caller(&state, &headers).await?;
    let limit = query.limit.unwrap_or(1000).clamp(1, 5000);
    let feed = long_poll(&state, query.wait, || async {
        let feed = read_feed(&state, &user.id, &id, query.since, limit).await?;
        let ready = !feed.changes.is_empty();
        Ok((feed, ready))
    })
    .await?;
    Ok(Json(
        json!({"seq":feed.seq,"more":feed.more,"reset":feed.reset,"changes":feed.changes}),
    ))
}

/// Reads with `read` until it reports something new or `wait` seconds pass. Each
/// commit (and maintenance) wakes waiters to read again. A bounded number of
/// requests may wait; the rest answer at once.
async fn long_poll<T, F, Fut>(state: &State, wait: i64, mut read: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(T, bool)>>,
{
    // Subscribe before reading so a commit in between still wakes this request.
    let mut wake = state.0.sync_wake.subscribe();
    let (mut value, ready) = read().await?;
    let wait = wait.clamp(0, 60);
    if ready || wait == 0 {
        return Ok(value);
    }
    let Ok(_waiting) = state.0.sync_waiters.try_acquire() else {
        return Ok(value);
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait as u64);
    while !state.0.maintenance.load(Ordering::SeqCst)
        && let Ok(Ok(())) = tokio::time::timeout_at(deadline, wake.changed()).await
    {
        if state.0.maintenance.load(Ordering::SeqCst) {
            break;
        }
        let (next, ready) = read().await?;
        value = next;
        if ready {
            break;
        }
    }
    Ok(value)
}

#[derive(Deserialize)]
pub struct WatchQuery {
    #[serde(default)]
    state: String,
    #[serde(default)]
    wait: i64,
}
/// A short token for the sequence numbers of all of a person's folders: it
/// changes whenever any folder changes, appears, or is deleted.
async fn folder_state(state: &State, user: &str) -> Result<(String, Vec<Value>)> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT id,seq FROM sync_folders WHERE user_id=? ORDER BY id")
            .bind(user)
            .fetch_all(&state.0.db)
            .await?;
    let summary = rows
        .iter()
        .map(|(id, seq)| format!("{id}:{seq}"))
        .collect::<Vec<_>>()
        .join(",");
    let folders = rows
        .into_iter()
        .map(|(id, seq)| json!({"id":id,"seq":seq}))
        .collect();
    Ok((auth::digest(&summary)[..16].to_string(), folders))
}
/// One request per device that returns when any of the person's folders changes,
/// so clients learn about changes at once without a connection per folder.
pub async fn watch(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Query(query): Query<WatchQuery>,
) -> Result<Json<Value>> {
    let Caller { user, .. } = caller(&state, &headers).await?;
    let (token, folders) = long_poll(&state, query.wait, || async {
        let (token, folders) = folder_state(&state, &user.id).await?;
        let changed = token != query.state;
        Ok(((token, folders), changed))
    })
    .await?;
    Ok(Json(
        json!({"state":token,"changed":token != query.state,"folders":folders}),
    ))
}

#[derive(Deserialize)]
pub struct Change {
    path: String,
    #[serde(default)]
    base_version: i64,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    size: Option<i64>,
    #[serde(default)]
    mtime: i64,
}
#[derive(Deserialize)]
pub struct Commit {
    changes: Vec<Change>,
    #[serde(default)]
    on_conflict: Option<String>,
}
pub async fn commit(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Json(input): Json<Commit>,
) -> Result<Json<Value>> {
    let Caller { user, device } = caller(&state, &headers).await?;
    let on_conflict = input.on_conflict.as_deref().unwrap_or("reject");
    if !matches!(on_conflict, "reject" | "overwrite" | "stash") {
        return Err(Error::bad(
            "on_conflict must be reject, overwrite, or stash.",
        ));
    }
    if input.changes.is_empty() || input.changes.len() > MAX_CHANGES {
        return Err(Error::bad(format!(
            "Commit 1–{MAX_CHANGES} changes at a time."
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for change in &input.changes {
        valid_path(&change.path)?;
        if change.hash.as_deref().is_some_and(|h| !valid_hash(h)) {
            return Err(Error::coded(
                StatusCode::BAD_REQUEST,
                "invalid_hash",
                "Hashes are lowercase SHA-256 hex.",
            ));
        }
        if change.base_version < 0 || !seen.insert(change.path.as_str()) {
            return Err(Error::bad(format!(
                "Invalid or repeated change for {}.",
                change.path
            )));
        }
    }
    let _blobs = state.0.sync_blobs.read().await;
    let folder = folder(&state, &user.id, &id).await?;
    let mut tx = state.0.db.begin_with("BEGIN IMMEDIATE").await?;
    let mut results = Vec::with_capacity(input.changes.len());
    let mut changed = false;
    for change in input.changes {
        let hash = change.hash.as_deref();
        let size =
            match hash {
                Some(hash) => {
                    let size: i64 = sqlx::query_scalar(
                        "SELECT size FROM sync_uploads WHERE user_id=? AND hash=?",
                    )
                    .bind(&user.id)
                    .bind(hash)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(|| {
                        Error::coded(
                            StatusCode::BAD_REQUEST,
                            "unknown_blob",
                            format!("Upload the blob for {} before committing it.", change.path),
                        )
                    })?;
                    if change.size.is_some_and(|n| n != size) {
                        return Err(Error::coded(
                            StatusCode::BAD_REQUEST,
                            "size_mismatch",
                            format!("The size of {} does not match its blob.", change.path),
                        ));
                    }
                    size
                }
                None => 0,
            };
        let existing = current(&mut tx, &id, &change.path).await?;
        let version = existing.as_ref().map_or(0, |c| c.version);
        let live = existing.as_ref().and_then(|c| c.hash.as_deref());
        let current_json = existing.as_ref().map(Current::json);
        if live == hash {
            // Already true: a retry, or two devices producing identical bytes.
            results.push(json!({"path":change.path,"status":"ok","version":version,"seq":existing.as_ref().map_or(0, |c| c.seq)}));
            continue;
        }
        let conflict = change.base_version != version;
        if conflict && on_conflict != "overwrite" {
            if on_conflict == "stash" && hash.is_some() {
                let history = record_history(
                    &mut tx,
                    &id,
                    &change.path,
                    change.base_version,
                    hash,
                    size,
                    change.mtime,
                    &device,
                    "conflict",
                )
                .await?;
                changed = true;
                results.push(json!({"path":change.path,"status":"stashed","history_id":history,"current":current_json}));
            } else {
                results
                    .push(json!({"path":change.path,"status":"conflict","current":current_json}));
            }
            continue;
        }
        if hash.is_some()
            && live.is_none()
            && let Some((code, other)) = clash(&mut tx, &id, &change.path).await?
        {
            results.push(json!({"path":change.path,"status":"rejected","code":code,"error":format!("{} cannot coexist with {other}.", change.path)}));
            continue;
        }
        if let Some(old) = existing.as_ref().filter(|c| c.hash.is_some()) {
            let reason = if conflict {
                "conflict"
            } else if hash.is_none() {
                "deleted"
            } else {
                "replaced"
            };
            record_history(
                &mut tx,
                &id,
                &change.path,
                old.version,
                old.hash.as_deref(),
                old.size,
                old.mtime,
                &old.device,
                reason,
            )
            .await?;
        }
        let seq = write_version(
            &mut tx,
            &id,
            &change.path,
            version + 1,
            hash,
            size,
            change.mtime,
            &device,
        )
        .await?;
        prune(&mut tx, &id, Some(&change.path), folder.keep_versions).await?;
        changed = true;
        results.push(json!({"path":change.path,"status":"ok","version":version + 1,"seq":seq,"conflict":conflict}));
    }
    tx.commit().await?;
    if changed {
        state.wake_sync();
    }
    Ok(Json(json!({"results":results})))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    conflicts: bool,
}
async fn list_history(state: &State, user: &str, id: &str, query: &HistoryQuery) -> Result<Value> {
    folder(state, user, id).await?;
    let rows = sqlx::query_as::<_, (i64, String, i64, Option<String>, i64, i64, String, String, i64, String, bool)>("SELECT h.id,h.path,h.version,h.hash,h.size,h.mtime,h.device,coalesce(d.name,CASE h.device WHEN 'web' THEN 'Dashboard' ELSE 'Removed device' END),h.created,h.reason,h.resolved FROM sync_history h LEFT JOIN devices d ON d.id=h.device WHERE h.folder=?1 AND (?2 IS NULL OR h.path=?2) AND (?3=0 OR h.resolved=0) ORDER BY h.id DESC LIMIT 500")
        .bind(id)
        .bind(query.path.as_deref())
        .bind(query.conflicts)
        .fetch_all(&state.0.db)
        .await?;
    Ok(json!(rows.into_iter().map(|(id, path, version, hash, size, mtime, device, device_name, created, reason, resolved)| json!({"id":id,"path":path,"version":version,"hash":hash,"size":size,"mtime":mtime,"device":device,"device_name":device_name,"created":created,"reason":reason,"resolved":resolved})).collect::<Vec<_>>()))
}
pub async fn history(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Value>> {
    let Caller { user, .. } = caller(&state, &headers).await?;
    Ok(Json(list_history(&state, &user.id, &id, &query).await?))
}
pub async fn browser_history(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Value>> {
    let user = browser(&state, &headers).await?;
    Ok(Json(list_history(&state, &user.id, &id, &query).await?))
}

#[derive(Deserialize)]
pub struct Restore {
    history_id: i64,
}
pub async fn restore(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Json(input): Json<Restore>,
) -> Result<Json<Value>> {
    let Caller { user, device } = caller(&state, &headers).await?;
    Ok(Json(
        restore_version(&state, &user.id, &id, input.history_id, &device).await?,
    ))
}
/// Makes a historical version current again as a new version every device pulls.
pub async fn restore_version(
    state: &State,
    user: &str,
    id: &str,
    history: i64,
    device: &str,
) -> Result<Value> {
    let _blobs = state.0.sync_blobs.read().await;
    let folder = folder(state, user, id).await?;
    let mut tx = state.0.db.begin_with("BEGIN IMMEDIATE").await?;
    let (path, hash, size, mtime): (String, Option<String>, i64, i64) =
        sqlx::query_as("SELECT path,hash,size,mtime FROM sync_history WHERE id=? AND folder=?")
            .bind(history)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(Error::not_found)?;
    let hash = hash.ok_or_else(|| Error::bad("This entry has no content to restore."))?;
    let existing = current(&mut tx, id, &path).await?;
    let version = existing.as_ref().map_or(0, |c| c.version);
    let mut seq = existing.as_ref().map_or(0, |c| c.seq);
    let mut result_version = version;
    if existing.as_ref().and_then(|c| c.hash.as_deref()) != Some(hash.as_str()) {
        match existing.as_ref().filter(|c| c.hash.is_some()) {
            Some(old) => {
                record_history(
                    &mut tx,
                    id,
                    &path,
                    old.version,
                    old.hash.as_deref(),
                    old.size,
                    old.mtime,
                    &old.device,
                    "replaced",
                )
                .await?;
            }
            None => {
                if let Some((_, other)) = clash(&mut tx, id, &path).await? {
                    return Err(Error::coded(
                        StatusCode::CONFLICT,
                        "path_conflict",
                        format!("{path} cannot coexist with {other}."),
                    ));
                }
            }
        }
        seq = write_version(
            &mut tx,
            id,
            &path,
            version + 1,
            Some(&hash),
            size,
            mtime,
            device,
        )
        .await?;
        result_version = version + 1;
    }
    sqlx::query("UPDATE sync_history SET resolved=1 WHERE id=?")
        .bind(history)
        .execute(&mut *tx)
        .await?;
    prune(&mut tx, id, Some(&path), folder.keep_versions).await?;
    tx.commit().await?;
    state.wake_sync();
    Ok(json!({"ok":true,"path":path,"version":result_version,"seq":seq}))
}
pub async fn dismiss_conflict(state: &State, user: &str, id: &str, history: i64) -> Result<Value> {
    let folder = folder(state, user, id).await?;
    let mut tx = state.0.db.begin_with("BEGIN IMMEDIATE").await?;
    let path: String = sqlx::query_scalar(
        "UPDATE sync_history SET resolved=1 WHERE id=? AND folder=? AND resolved=0 RETURNING path",
    )
    .bind(history)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(Error::not_found)?;
    prune(&mut tx, id, Some(&path), folder.keep_versions).await?;
    tx.commit().await?;
    Ok(json!({"ok":true}))
}

#[derive(Deserialize)]
pub struct Browse {
    #[serde(default)]
    path: String,
    #[serde(default)]
    page: usize,
}
/// One directory level of a folder's live files, for the dashboard.
pub async fn browse(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(query): Query<Browse>,
) -> Result<Json<Value>> {
    let user = browser(&state, &headers).await?;
    let folder = folder(&state, &user.id, &id).await?;
    let dir = query.path.trim_matches('/');
    if !dir.is_empty() {
        valid_path(dir)?;
    }
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let rows: Vec<(String, i64, i64, i64, String)> = sqlx::query_as("SELECT f.path,f.size,f.mtime,f.version,coalesce(d.name,CASE f.device WHEN 'web' THEN 'Dashboard' ELSE 'Removed device' END) FROM sync_files f LEFT JOIN devices d ON d.id=f.device WHERE f.folder=?1 AND f.hash IS NOT NULL AND substr(f.path,1,length(?2))=?2")
        .bind(&id)
        .bind(&prefix)
        .fetch_all(&state.0.db)
        .await?;
    let mut directories = std::collections::BTreeMap::<String, (i64, i64)>::new();
    let mut files = Vec::new();
    for (path, size, mtime, version, device) in rows {
        let rest = &path[prefix.len()..];
        match rest.split_once('/') {
            Some((name, _)) => {
                let entry = directories.entry(name.to_string()).or_default();
                entry.0 += size;
                entry.1 = entry.1.max(mtime);
            }
            None => files.push(json!({"name":rest,"directory":false,"size":size,"modified":mtime,"version":version,"device":device})),
        }
    }
    files.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    let mut entries: Vec<Value> = directories
        .into_iter()
        .map(|(name, (size, mtime))| json!({"name":name,"directory":true,"size":size,"modified":mtime}))
        .collect();
    entries.extend(files);
    let total = entries.len();
    let entries: Vec<Value> = entries
        .into_iter()
        .skip(query.page * 100)
        .take(100)
        .collect();
    Ok(Json(
        json!({"folder":folder.json(),"entries":entries,"total":total}),
    ))
}
#[derive(Deserialize)]
pub struct FileQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    history: Option<i64>,
}
pub async fn browser_file(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(query): Query<FileQuery>,
) -> Result<Response> {
    let user = browser(&state, &headers).await?;
    folder(&state, &user.id, &id).await?;
    let found: Option<(String, Option<String>)> = match query.history {
        Some(history) => {
            sqlx::query_as("SELECT path,hash FROM sync_history WHERE id=? AND folder=?")
                .bind(history)
                .bind(&id)
                .fetch_optional(&state.0.db)
                .await?
        }
        None => {
            sqlx::query_as("SELECT path,hash FROM sync_files WHERE folder=? AND path=?")
                .bind(&id)
                .bind(&query.path)
                .fetch_optional(&state.0.db)
                .await?
        }
    };
    let (path, hash) = found.ok_or_else(Error::not_found)?;
    let hash = hash.ok_or_else(Error::not_found)?;
    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    blobs::stream(&state, &hash, &headers, Some(&name)).await
}
