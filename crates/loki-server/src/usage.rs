//! What takes up space in LOKI's data directory, for the owner's Overview.
//!
//! Measuring walks every file, so the result is kept for a while and measured
//! again only when it is old or the owner asks, and one measurement runs at a time.
use crate::{Result, State};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

/// How long a measurement is shown before the next visit measures again.
const FRESH: Duration = Duration::from_secs(15 * 60);

static LAST: Mutex<Option<(Instant, Value)>> = Mutex::new(None);
static MEASURING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The parts of the data directory, by the names they have on disk. Anything not
/// listed counts as "Other".
const PARTS: &[(&str, &str, &[&str])] = &[
    ("sync", "Save sync", &["sync"]),
    ("repositories", "Repositories", &["skald-repos"]),
    ("apps", "Installed apps", &["apps", "registry"]),
    ("app-data", "App data", &["app-data"]),
    ("personal", "Personal files", &["personal-files"]),
    ("pictures", "Pictures and icons", &["avatars"]),
    ("database", "Database", &["loki.db", "loki.db-wal", "loki.db-shm"]),
    ("backups", "Backups", &["backups"]),
    ("rollbacks", "App rollback copies", &["rollback-data"]),
    ("logs", "Logs", &["logs"]),
    ("temporary", "Temporary files", &["staging"]),
];

/// The latest measurement, measuring again when it is stale or `refresh` is set.
pub async fn report(state: &State, refresh: bool) -> Result<Value> {
    if !refresh && let Some(value) = fresh() {
        return Ok(value);
    }
    let _one = MEASURING.lock().await;
    // Someone else may have just measured while this request waited.
    if !refresh && let Some(value) = fresh() {
        return Ok(value);
    }
    let data = state.0.data.clone();
    let value = tokio::task::spawn_blocking(move || measure(&data)).await??;
    *LAST.lock().unwrap() = Some((Instant::now(), value.clone()));
    Ok(value)
}

fn fresh() -> Option<Value> {
    LAST.lock()
        .unwrap()
        .as_ref()
        .filter(|(at, _)| at.elapsed() < FRESH)
        .map(|(_, value)| value.clone())
}

fn measure(data: &Path) -> anyhow::Result<Value> {
    let mut sizes = vec![0u64; PARTS.len() + 1];
    for entry in std::fs::read_dir(data)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let part = PARTS
            .iter()
            .position(|(_, _, names)| names.contains(&name.as_str()))
            .unwrap_or(PARTS.len());
        sizes[part] += size(&entry.path());
    }
    let mut items: Vec<Value> = PARTS
        .iter()
        .map(|(key, label, _)| (*key, *label))
        .chain([("other", "Other")])
        .zip(sizes.iter())
        .filter(|(_, bytes)| **bytes > 0)
        .map(|((key, label), bytes)| json!({"key": key, "label": label, "bytes": bytes}))
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item["bytes"].as_u64().unwrap_or(0)));
    Ok(json!({
        "measured": crate::auth::now(),
        "total": sizes.iter().sum::<u64>(),
        "items": items,
        "disk": {
            "free": fs2::available_space(data).ok(),
            "total": fs2::total_space(data).ok(),
        },
    }))
}

/// The bytes in a file or a whole folder. Links are not followed, so nothing is
/// counted twice and nothing outside the data directory is counted.
fn size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0;
    let mut stack = vec![path.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(entry.path()),
                Ok(kind) if kind.is_file() => total += entry.metadata().map_or(0, |meta| meta.len()),
                _ => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::measure;

    #[test]
    fn parts_are_measured_and_unknown_files_count_as_other() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        std::fs::create_dir_all(data.join("sync/a/b")).unwrap();
        std::fs::write(data.join("sync/a/b/save.dat"), vec![0; 3000]).unwrap();
        std::fs::write(data.join("loki.db"), vec![0; 500]).unwrap();
        std::fs::write(data.join("loki.db-wal"), vec![0; 100]).unwrap();
        std::fs::write(data.join("identity.pem"), vec![0; 40]).unwrap();
        let report = measure(data).unwrap();
        assert_eq!(report["total"], 3640);
        let items = report["items"].as_array().unwrap();
        let find = |key: &str| items.iter().find(|item| item["key"] == key).map(|item| item["bytes"].clone());
        assert_eq!(find("sync"), Some(3000.into()));
        assert_eq!(find("database"), Some(600.into()));
        assert_eq!(find("other"), Some(40.into()));
        assert_eq!(find("backups"), None);
        assert_eq!(items[0]["key"], "sync");
    }
}
