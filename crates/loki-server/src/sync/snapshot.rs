//! A folder as it was at a moment: previewing it, rolling the folder back to it, and
//! downloading it as a ZIP. Every version records when it became current (`since`)
//! and when it was displaced (`created`), so the state at time T is, for each path,
//! the current version if it started by T, or else the history entry that spanned T.
use super::{Caller, api, blobs, browser, caller};
use crate::{Error, Result, State, auth, files::tree};
use axum::{
    Json,
    extract::{Path as RoutePath, Query, State as ExtractState},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

/// How many paths each preview list shows.
const LISTED: usize = 100;

/// One live file at the snapshot time.
#[derive(Clone)]
struct Entry {
    hash: String,
    size: i64,
    mtime: i64,
}

struct Snapshot {
    /// Live files at the time, by path.
    files: BTreeMap<String, Entry>,
    /// Paths whose state at the time is unknown because older versions were pruned.
    unavailable: Vec<String>,
    /// Current live files, by path, to compare against.
    current: BTreeMap<String, String>,
}

impl Snapshot {
    /// Files to bring back (missing or different now) and files added since.
    fn changes(&self) -> (Vec<(&String, &Entry)>, Vec<&String>) {
        let restore = self
            .files
            .iter()
            .filter(|(path, e)| self.current.get(*path) != Some(&e.hash))
            .collect();
        let remove = self
            .current
            .keys()
            .filter(|path| !self.files.contains_key(*path) && !self.unavailable.contains(path))
            .collect();
        (restore, remove)
    }
}

/// path, hash, size, mtime, updated
type FileRow = (String, Option<String>, i64, i64, i64);
/// path, hash, size, mtime, since, created
type HistoryRow = (String, Option<String>, i64, i64, i64, i64);

async fn snapshot(
    db: &mut sqlx::SqliteConnection,
    folder: &api::Folder,
    time: i64,
) -> Result<Snapshot> {
    let rows: Vec<FileRow> =
        sqlx::query_as("SELECT path,hash,size,mtime,updated FROM sync_files WHERE folder=?")
            .bind(&folder.id)
            .fetch_all(&mut *db)
            .await?;
    let mut history = HashMap::<String, Vec<HistoryRow>>::new();
    let entries: Vec<HistoryRow> = sqlx::query_as(
        "SELECT path,hash,size,mtime,since,created FROM sync_history WHERE folder=? AND stashed=0 ORDER BY id",
    )
    .bind(&folder.id)
    .fetch_all(&mut *db)
    .await?;
    for entry in entries {
        history.entry(entry.0.clone()).or_default().push(entry);
    }
    let mut snapshot = Snapshot {
        files: BTreeMap::new(),
        unavailable: Vec::new(),
        current: BTreeMap::new(),
    };
    for (path, hash, size, mtime, updated) in rows {
        if let Some(hash) = &hash {
            snapshot.current.insert(path.clone(), hash.clone());
        }
        if updated <= time {
            if let Some(hash) = hash {
                snapshot.files.insert(path, Entry { hash, size, mtime });
            }
            continue;
        }
        let earlier = history.get(&path).map(Vec::as_slice).unwrap_or_default();
        if let Some((_, hash, size, mtime, ..)) = earlier
            .iter()
            .rev()
            .find(|(_, _, _, _, since, created)| *since <= time && *created > time)
        {
            if let Some(hash) = hash.clone() {
                snapshot.files.insert(
                    path,
                    Entry {
                        hash,
                        size: *size,
                        mtime: *mtime,
                    },
                );
            }
        } else if earlier.len() as i64 >= folder.keep_versions
            && earlier.iter().all(|(_, _, _, _, since, _)| *since > time)
        {
            // Pruning dropped whatever this path held then; it may have been a file.
            snapshot.unavailable.push(path);
        }
        // Otherwise it did not exist, or had been deleted, at that time.
    }
    Ok(snapshot)
}

async fn snapshot_now(state: &State, folder: &api::Folder, time: i64) -> Result<Snapshot> {
    let mut db = state.0.db.acquire().await?;
    snapshot(&mut db, folder, time).await
}

#[derive(Deserialize)]
pub struct TimeQuery {
    /// Unix seconds. Omitted: now.
    #[serde(default)]
    time: Option<i64>,
}

fn preview(snapshot: &Snapshot, time: i64) -> Value {
    let (restore, remove) = snapshot.changes();
    let unchanged = snapshot.files.len() - restore.len();
    json!({
        "time": time,
        "files": snapshot.files.len(),
        "bytes": snapshot.files.values().map(|e| e.size).sum::<i64>(),
        "unchanged": unchanged,
        "restore": restore.len(),
        "remove": remove.len(),
        "unavailable": snapshot.unavailable.len(),
        "restore_paths": restore.iter().take(LISTED).map(|(p, _)| p).collect::<Vec<_>>(),
        "remove_paths": remove.iter().take(LISTED).collect::<Vec<_>>(),
        "unavailable_paths": snapshot.unavailable.iter().take(LISTED).collect::<Vec<_>>(),
    })
}

async fn preview_for(state: &State, user: &str, id: &str, time: Option<i64>) -> Result<Value> {
    let folder = api::folder(state, user, id).await?;
    let time = time.unwrap_or_else(auth::now);
    let snapshot = snapshot_now(state, &folder, time).await?;
    Ok(preview(&snapshot, time))
}

/// `GET /sync/v1/folders/{id}/snapshot?time=` for devices.
pub async fn snapshot_api(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(query): Query<TimeQuery>,
) -> Result<Json<Value>> {
    let Caller { user, .. } = caller(&state, &headers).await?;
    Ok(Json(preview_for(&state, &user.id, &id, query.time).await?))
}

/// `GET /api/sync/folders/{id}/snapshot?time=` for the dashboard.
pub async fn browser_snapshot(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(query): Query<TimeQuery>,
) -> Result<Json<Value>> {
    let user = browser(&state, &headers).await?;
    Ok(Json(preview_for(&state, &user.id, &id, query.time).await?))
}

#[derive(Deserialize)]
pub struct Rollback {
    time: i64,
    /// Also delete files that were added after `time`.
    #[serde(default)]
    remove_newer: bool,
}

/// `POST /sync/v1/folders/{id}/rollback` for devices.
pub async fn rollback_api(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Json(input): Json<Rollback>,
) -> Result<Json<Value>> {
    let Caller { user, device, .. } = caller(&state, &headers).await?;
    Ok(Json(rollback(&state, &user.id, &id, input, &device).await?))
}

/// Dashboard action `sync-rollback`.
pub async fn rollback_action(state: &State, user: &str, id: &str, data: Value) -> Result<Value> {
    let input: Rollback = crate::web::args(data)?;
    rollback(state, user, id, input, "web").await
}

/// Makes the folder match what it held at `time`, as new versions every device
/// pulls. What it replaces goes to history, so a rollback can itself be undone.
async fn rollback(
    state: &State,
    user: &str,
    id: &str,
    input: Rollback,
    device: &str,
) -> Result<Value> {
    if input.time <= 0 || input.time > auth::now() {
        return Err(Error::bad("Choose a time in the past."));
    }
    let _blobs = state.0.sync_blobs.read().await;
    let folder = api::folder(state, user, id).await?;
    let mut tx = state.0.db.begin_with("BEGIN IMMEDIATE").await?;
    let snapshot = snapshot(&mut tx, &folder, input.time).await?;
    let (restore, remove) = snapshot.changes();
    let now = auth::now();
    let (mut restored, mut removed) = (0, 0);
    let mut skipped = Vec::new();
    // Removing first frees names that restored files may need.
    if input.remove_newer {
        for path in remove {
            let Some(old) = api::current(&mut tx, id, path).await? else {
                continue;
            };
            let Some(hash) = old.hash.as_deref() else {
                continue;
            };
            api::record_history(
                &mut tx,
                id,
                path,
                old.version,
                Some(hash),
                old.size,
                old.mtime,
                &old.device,
                "deleted",
                Some(old.updated),
            )
            .await?;
            api::write_version(&mut tx, id, path, old.version + 1, None, 0, now, device).await?;
            api::prune(&mut tx, id, Some(path), folder.keep_versions).await?;
            removed += 1;
        }
    }
    for (path, entry) in restore {
        if !tokio::fs::try_exists(blobs::blob_path(&state.0.data, &entry.hash))
            .await
            .unwrap_or(false)
        {
            skipped.push(json!({"path":path,"error":"Its content is missing from the server."}));
            continue;
        }
        let existing = api::current(&mut tx, id, path).await?;
        let version = existing.as_ref().map_or(0, |c| c.version);
        match existing.as_ref().filter(|c| c.hash.is_some()) {
            Some(old) => {
                api::record_history(
                    &mut tx,
                    id,
                    path,
                    old.version,
                    old.hash.as_deref(),
                    old.size,
                    old.mtime,
                    &old.device,
                    "replaced",
                    Some(old.updated),
                )
                .await?;
            }
            None => {
                if let Some((_, other)) = api::clash(&mut tx, id, path).await? {
                    skipped.push(
                        json!({"path":path,"error":format!("It cannot coexist with {other}.")}),
                    );
                    continue;
                }
            }
        }
        api::write_version(
            &mut tx,
            id,
            path,
            version + 1,
            Some(&entry.hash),
            entry.size,
            entry.mtime,
            device,
        )
        .await?;
        api::prune(&mut tx, id, Some(path), folder.keep_versions).await?;
        restored += 1;
    }
    tx.commit().await?;
    if restored + removed > 0 {
        state.wake_sync();
    }
    Ok(
        json!({"ok":true,"restored":restored,"removed":removed,"skipped":skipped,"unavailable":snapshot.unavailable.len()}),
    )
}

/// Recent moments when files were replaced or deleted, grouped by the device that
/// changed them, so the dashboard can offer "roll back to just before this".
pub async fn browser_activity(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let user = browser(&state, &headers).await?;
    api::folder(&state, &user.id, &id).await?;
    // The device that displaced a version wrote the next one, current or in history.
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "WITH recent AS (SELECT h.created,h.reason,coalesce((SELECT f.device FROM sync_files f WHERE f.folder=h.folder AND f.path=h.path AND f.version=h.version+1),(SELECT n.device FROM sync_history n WHERE n.folder=h.folder AND n.path=h.path AND n.version=h.version+1 AND n.stashed=0 ORDER BY n.id LIMIT 1),'') device FROM sync_history h WHERE h.folder=? AND h.stashed=0 ORDER BY h.id DESC LIMIT 20000) SELECT r.created,r.reason,coalesce(d.name,CASE r.device WHEN 'web' THEN 'Dashboard' ELSE 'Removed device' END) FROM recent r LEFT JOIN devices d ON d.id=r.device",
    )
    .bind(&id)
    .fetch_all(&state.0.db)
    .await?;
    struct Group {
        start: i64,
        end: i64,
        device: String,
        counts: BTreeMap<String, i64>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for (created, reason, device) in rows {
        // Newest first: a change joins the group above it when the same device made
        // it within two minutes of that group's earliest change.
        match groups.last_mut() {
            Some(g) if g.device == device && g.start - created <= 120 => {
                g.start = g.start.min(created);
                *g.counts.entry(reason).or_default() += 1;
            }
            _ => {
                if groups.len() == 30 {
                    break;
                }
                groups.push(Group {
                    start: created,
                    end: created,
                    device,
                    counts: BTreeMap::from([(reason, 1)]),
                });
            }
        }
    }
    Ok(Json(json!(
        groups
            .into_iter()
            .map(|g| json!({"start":g.start,"end":g.end,"device":g.device,"replaced":g.counts.get("replaced").copied().unwrap_or(0),"deleted":g.counts.get("deleted").copied().unwrap_or(0),"conflict":g.counts.get("conflict").copied().unwrap_or(0)}))
            .collect::<Vec<_>>()
    )))
}

/// Names that work as a ZIP's top folder on every system.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || r#"/\:*?"<>|"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').to_string();
    if cleaned.is_empty() {
        "Sync folder".into()
    } else {
        cleaned
    }
}

/// `GET /api/sync/folders/{id}/archive?time=` downloads the folder, now or as it
/// was at `time`, as a ZIP.
pub async fn browser_archive(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(query): Query<TimeQuery>,
) -> Result<Response> {
    let user = browser(&state, &headers).await?;
    let folder = api::folder(&state, &user.id, &id).await?;
    let permit = tree::zip_permit()?;
    let snapshot = snapshot_now(&state, &folder, query.time.unwrap_or(i64::MAX)).await?;
    let top = safe_name(&folder.name);
    let name = match query.time {
        Some(time) => {
            let date = time::OffsetDateTime::from_unix_timestamp(time)
                .map_err(|_| Error::new(StatusCode::BAD_REQUEST, "Choose a valid time."))?;
            format!(
                "{top} {:04}-{:02}-{:02} {:02}.{:02}",
                date.year(),
                u8::from(date.month()),
                date.day(),
                date.hour(),
                date.minute()
            )
        }
        None => top.clone(),
    };
    let data = state.0.data.clone();
    let files: Vec<(String, String)> = snapshot
        .files
        .into_iter()
        .map(|(path, e)| (path, e.hash))
        .collect();
    tree::zip_download(permit, &name, move |out| {
        out.directory(&top)?;
        for (path, hash) in files {
            if out.closed() {
                return Ok(());
            }
            let mut file = std::fs::File::open(blobs::blob_path(&data, &hash)).map_err(|_| {
                Error::bad(format!("The content of {path} is missing from the server."))
            })?;
            out.file(&format!("{top}/{path}"), |zip| {
                std::io::copy(&mut file, zip)?;
                Ok(())
            })?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::safe_name;

    #[test]
    fn zip_names_are_portable() {
        assert_eq!(safe_name("GBA saves"), "GBA saves");
        assert_eq!(safe_name("a/b:c"), "a_b_c");
        assert_eq!(safe_name(" . "), "Sync folder");
        assert_eq!(safe_name("trailing."), "trailing");
    }
}
