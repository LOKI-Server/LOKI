//! App-scoped durable values and generated files. No guest filesystem paths.
use crate::{Error, Result, apps};
use axum::http::StatusCode;
use cap_std::fs::{Dir, File, OpenOptions};
use serde_json::Value;
use std::{
    io::{Read, Write},
    path::Path,
    sync::Arc,
};
use wasmtime::component::{ComponentType, Lower};

pub const VALUE_BYTES: u64 = 128 * 1024;
pub const FILE_BYTES: u64 = 1024 * 1024;
#[cfg(test)]
const TOTAL_BYTES: u64 = loki_sdk::manifest::STORAGE_MIB * 1024 * 1024;
#[cfg(test)]
const ENTRIES: usize = loki_sdk::manifest::STORAGE_ENTRIES;

/// Matches `file-info` in the WIT.
#[derive(Clone, Debug, ComponentType, Lower)]
#[component(record)]
pub(crate) struct FileInfo {
    pub path: String,
    pub size: u64,
}
/// Matches `file-page` in the WIT.
#[derive(Debug, ComponentType, Lower)]
#[component(record)]
pub(crate) struct FilePage {
    pub files: Vec<FileInfo>,
    pub next: Option<String>,
}
/// Matches `usage` in the WIT.
#[derive(Debug, ComponentType, Lower)]
#[component(record)]
pub(crate) struct Usage {
    pub bytes: u64,
    pub entries: u32,
    #[component(name = "limit-bytes")]
    pub limit_bytes: u64,
    #[component(name = "max-entries")]
    pub max_entries: u32,
    #[component(name = "max-file-bytes")]
    pub max_file_bytes: u64,
    #[component(name = "max-value-bytes")]
    pub max_value_bytes: u64,
}
pub(crate) struct Storage {
    values: Dir,
    files: Dir,
    lock: Arc<tokio::sync::Mutex<()>>,
    quota: u64,
    entries: usize,
    /// Bytes and entries in use, counted once and then kept up to date.
    totals: std::sync::Mutex<Option<(u64, usize)>>,
}
fn io(error: std::io::Error) -> Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => Error::not_found(),
        std::io::ErrorKind::PermissionDenied => Error::forbidden(),
        std::io::ErrorKind::AlreadyExists => {
            Error::new(StatusCode::CONFLICT, "Storage name already exists.")
        }
        _ => error.into(),
    }
}
fn limit(message: &str) -> Error {
    Error::new(StatusCode::PAYLOAD_TOO_LARGE, message)
}
fn regular(metadata: &cap_std::fs::Metadata) -> Result<()> {
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(Error::forbidden());
        }
    }
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::forbidden());
    }
    Ok(())
}
fn name(path: &str, key: bool) -> Result<String> {
    if path.len() > 120 || !apps::safe_relative(path) || (key && path.contains('/')) {
        return Err(Error::bad(
            "Use a safe relative storage name of 1–120 UTF-8 bytes; JSON keys cannot contain slashes.",
        ));
    }
    // Reversible encoding gives case-sensitive, Unicode names on every host OS.
    // Logical slashes are namespaced keys, never guest-selected OS directories.
    Ok(hex::encode(path.as_bytes()))
}
impl Storage {
    #[cfg(test)]
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        Self::open_with(path, TOTAL_BYTES, ENTRIES)
    }
    /// Opens the app's storage with its quota in bytes and entries.
    pub fn open_with(path: &Path, quota: u64, entries: usize) -> Result<Arc<Self>> {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                return Err(Error::forbidden());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    return Err(Error::forbidden());
                }
            }
        }
        std::fs::create_dir_all(path)?;
        let root = Dir::open_ambient_dir(path, cap_std::ambient_authority())?;
        let directory = |kind: &str| -> Result<Dir> {
            root.create_dir_all(kind).map_err(io)?;
            let meta = root.symlink_metadata(kind).map_err(io)?;
            if meta.file_type().is_symlink() {
                return Err(Error::forbidden());
            }
            #[cfg(windows)]
            {
                use cap_std::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    return Err(Error::forbidden());
                }
            }
            let dir = root.open_dir(kind).map_err(io)?;
            for (index, entry) in dir.entries().map_err(io)?.enumerate() {
                if index >= entries {
                    return Err(limit("App storage entry limit exceeded."));
                }
                let entry = entry.map_err(io)?;
                if entry.file_name().to_string_lossy().starts_with(".pending-") {
                    regular(&dir.symlink_metadata(entry.file_name()).map_err(io)?)?;
                    dir.remove_file(entry.file_name()).map_err(io)?;
                }
            }
            Ok(dir)
        };
        Ok(Arc::new(Self {
            values: directory("values")?,
            files: directory("files")?,
            lock: Arc::new(tokio::sync::Mutex::new(())),
            quota,
            entries,
            totals: std::sync::Mutex::new(None),
        }))
    }
    /// Bytes and entries in use. Counted from disk the first time.
    fn totals(&self) -> Result<(u64, usize)> {
        if let Some(totals) = *self.totals.lock().unwrap() {
            return Ok(totals);
        }
        let totals = self.scan()?;
        *self.totals.lock().unwrap() = Some(totals);
        Ok(totals)
    }
    /// Records bytes added and removed and the change in entries, or forgets the
    /// totals when the change is unknown.
    fn changed(&self, change: Option<(u64, u64, isize)>) {
        let mut totals = self.totals.lock().unwrap();
        *totals = match (*totals, change) {
            (Some((bytes, count)), Some((added, removed, entries))) => Some((
                bytes.saturating_add(added).saturating_sub(removed),
                count.saturating_add_signed(entries),
            )),
            _ => None,
        };
    }
    fn scan(&self) -> Result<(u64, usize)> {
        let mut bytes = 0u64;
        let mut count = 0;
        for dir in [&self.values, &self.files] {
            for entry in dir.entries().map_err(io)? {
                let entry = entry.map_err(io)?;
                let meta = dir.symlink_metadata(entry.file_name()).map_err(io)?;
                regular(&meta)?;
                count += 1;
                if count > self.entries {
                    return Err(limit("App storage entry limit exceeded."));
                }
                bytes = bytes
                    .checked_add(meta.len())
                    .ok_or_else(|| limit("App storage quota exceeded."))?;
            }
        }
        Ok((bytes, count))
    }
    fn existing(dir: &Dir, name: &str) -> Result<Option<u64>> {
        match dir.symlink_metadata(name) {
            Ok(meta) => {
                regular(&meta)?;
                Ok(Some(meta.len()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io(error)),
        }
    }
    fn write(&self, dir: &Dir, name: &str, bytes: &[u8], max: u64) -> Result<()> {
        if bytes.len() as u64 > max {
            return Err(limit("Storage value or file exceeds its size limit."));
        }
        let (total, count) = self.totals()?;
        let old = Self::existing(dir, name)?;
        if total.saturating_sub(old.unwrap_or(0)) + bytes.len() as u64 > self.quota
            || (old.is_none() && count >= self.entries)
        {
            return Err(limit("App storage quota or entry limit exceeded."));
        }
        let temporary = format!(".pending-{}", uuid::Uuid::new_v4());
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = dir.open_with(&temporary, &options).map_err(io)?;
            file.write_all(bytes).map_err(io)?;
            file.sync_all().map_err(io)?;
            drop(file);
            dir.rename(&temporary, dir, name).map_err(io)?;
            #[cfg(unix)]
            dir.try_clone()
                .map_err(io)?
                .into_std_file()
                .sync_all()
                .map_err(io)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = dir.remove_file(&temporary);
            self.changed(None);
        } else {
            self.changed(Some((
                bytes.len() as u64,
                old.unwrap_or(0),
                isize::from(old.is_none()),
            )));
        }
        result
    }
    fn read(dir: &Dir, name: &str, max: u64) -> Result<Option<Vec<u8>>> {
        if Self::existing(dir, name)?.is_none() {
            return Ok(None);
        }
        let file = dir.open(name).map_err(io)?;
        regular(&file.metadata().map_err(io)?)?;
        let mut bytes = Vec::new();
        file.take(max + 1).read_to_end(&mut bytes).map_err(io)?;
        if bytes.len() as u64 > max {
            return Err(limit("Storage value or file exceeds its read limit."));
        }
        Ok(Some(bytes))
    }
    /// Runs storage work off the async runtime under the app's storage lock. The
    /// blocking job owns the guard even if its requesting guest times out.
    pub async fn job<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&Self) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let guard = self.lock.clone().lock_owned().await;
        let storage = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            work(&storage)
        })
        .await?
    }
    /// Replaces the value under `key` with JSON text.
    pub fn set_value(&self, key: &str, json: &str) -> Result<()> {
        let key = name(key, true)?;
        let value: Value = serde_json::from_str(json)
            .map_err(|_| Error::bad("Storage values must be JSON text."))?;
        self.write(
            &self.values,
            &key,
            &serde_json::to_vec(&value)?,
            VALUE_BYTES,
        )
    }
    /// The JSON text under `key`, if any.
    pub fn value(&self, key: &str) -> Result<Option<String>> {
        Self::read(&self.values, &name(key, true)?, VALUE_BYTES)?
            .map(|bytes| Ok(serde_json::from_slice::<Value>(&bytes)?.to_string()))
            .transpose()
    }
    pub fn delete_value(&self, key: &str) -> Result<bool> {
        self.remove(&self.values, &name(key, true)?)
    }
    pub fn write_file(&self, path: &str, bytes: &[u8]) -> Result<FileInfo> {
        let key = name(path, false)?;
        if bytes.len() as u64 > FILE_BYTES {
            return Err(limit("Generated file exceeds 1 MiB."));
        }
        self.write(&self.files, &key, bytes, FILE_BYTES)?;
        Ok(FileInfo {
            path: path.into(),
            size: bytes.len() as u64,
        })
    }
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        Self::read(&self.files, &name(path, false)?, FILE_BYTES)
            .map_err(|e| {
                if e.status == StatusCode::PAYLOAD_TOO_LARGE {
                    limit("This file is over 1 MiB; read it in ranges with files.read.")
                } else {
                    e
                }
            })?
            .ok_or_else(Error::not_found)
    }
    /// Adds at most 1 MiB to the end of a file, creating it if needed. The file may
    /// grow to the quota.
    pub fn append_file(&self, path: &str, bytes: &[u8]) -> Result<FileInfo> {
        let key = name(path, false)?;
        if bytes.len() as u64 > FILE_BYTES {
            return Err(limit("Append at most 1 MiB at a time."));
        }
        let (total, count) = self.totals()?;
        let old = Self::existing(&self.files, &key)?;
        if total + bytes.len() as u64 > self.quota || (old.is_none() && count >= self.entries) {
            return Err(limit("App storage quota or entry limit exceeded."));
        }
        let result = (|| -> Result<u64> {
            let mut options = OpenOptions::new();
            options.append(true).create(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = self.files.open_with(&key, &options).map_err(io)?;
            regular(&file.metadata().map_err(io)?)?;
            file.write_all(bytes).map_err(io)?;
            file.sync_data().map_err(io)?;
            Ok(file.metadata().map_err(io)?.len())
        })();
        match result {
            Ok(size) => {
                self.changed(Some((bytes.len() as u64, 0, isize::from(old.is_none()))));
                Ok(FileInfo {
                    path: path.into(),
                    size,
                })
            }
            Err(e) => {
                self.changed(None);
                Err(e)
            }
        }
    }
    /// Moves a file in one step, replacing `to` only when asked.
    pub fn rename_file(&self, from: &str, to: &str, replace: bool) -> Result<()> {
        let (source, target) = (name(from, false)?, name(to, false)?);
        Self::existing(&self.files, &source)?.ok_or_else(Error::not_found)?;
        if source == target {
            return Ok(());
        }
        let replaced = Self::existing(&self.files, &target)?;
        if replaced.is_some() && !replace {
            return Err(Error::new(StatusCode::CONFLICT, "That file already exists."));
        }
        self.totals()?;
        match self.files.rename(&source, &self.files, &target).map_err(io) {
            Ok(()) => {
                self.changed(Some((
                    0,
                    replaced.unwrap_or(0),
                    -isize::from(replaced.is_some()),
                )));
                Ok(())
            }
            Err(e) => {
                self.changed(None);
                Err(e)
            }
        }
    }
    pub fn file_info(&self, path: &str) -> Result<FileInfo> {
        let size =
            Self::existing(&self.files, &name(path, false)?)?.ok_or_else(Error::not_found)?;
        Ok(FileInfo {
            path: path.into(),
            size,
        })
    }
    pub fn delete_file(&self, path: &str) -> Result<bool> {
        self.remove(&self.files, &name(path, false)?)
    }
    fn remove(&self, dir: &Dir, name: &str) -> Result<bool> {
        let Some(size) = Self::existing(dir, name)? else {
            return Ok(false);
        };
        if let Err(e) = dir.remove_file(name) {
            self.changed(None);
            return Err(io(e));
        }
        self.changed(Some((0, size, -1)));
        Ok(true)
    }
    /// Files under `prefix`, sorted by path, 100 at a time after the `after` cursor.
    pub fn list_files(&self, prefix: &str, after: Option<&str>) -> Result<FilePage> {
        let after = after.unwrap_or("");
        if prefix.len() > 120 || after.len() > 120 {
            return Err(Error::bad("Invalid storage cursor."));
        }
        self.scan()?;
        let mut files = Vec::new();
        for entry in self.files.entries().map_err(io)? {
            let entry = entry.map_err(io)?;
            let path = String::from_utf8(
                hex::decode(entry.file_name().to_string_lossy().as_bytes())
                    .map_err(|_| Error::bad("Invalid stored filename."))?,
            )
            .map_err(|_| Error::bad("Invalid stored filename."))?;
            if path.starts_with(prefix) && path.as_str() > after {
                let size = entry.metadata().map_err(io)?.len();
                files.push(FileInfo { path, size });
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let more = files.len() > 100;
        files.truncate(100);
        let next = if more {
            files.last().map(|f| f.path.clone())
        } else {
            None
        };
        Ok(FilePage { files, next })
    }
    pub fn usage(&self) -> Result<Usage> {
        let (bytes, entries) = self.totals()?;
        Ok(Usage {
            bytes,
            entries: entries as u32,
            limit_bytes: self.quota,
            max_entries: self.entries as u32,
            max_file_bytes: FILE_BYTES,
            max_value_bytes: VALUE_BYTES,
        })
    }
    pub async fn open_file(self: &Arc<Self>, path: &str) -> Result<File> {
        let guard = self.lock.clone().lock_owned().await;
        let storage = self.clone();
        let name = name(path, false)?;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            Self::existing(&storage.files, &name)?.ok_or_else(Error::not_found)?;
            let file = storage.files.open(name).map_err(io)?;
            regular(&file.metadata().map_err(io)?)?;
            Ok(file)
        })
        .await?
    }
    pub async fn drain(&self) {
        let _guard = self.lock.lock().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn storage() -> (tempfile::TempDir, Arc<Storage>) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("storage")).unwrap();
        (dir, storage)
    }
    #[test]
    fn values_files_and_case_sensitive_unicode_names_survive_reopening() {
        let (dir, storage) = storage();
        storage.set_value("prefs", r#"{"theme":"green"}"#).unwrap();
        for path in ["reports/枝.txt", "reports/BRANCH.txt", "reports/branch.txt"] {
            storage.write_file(path, path.as_bytes()).unwrap();
        }
        drop(storage);
        let storage = Storage::open(&dir.path().join("storage")).unwrap();
        assert_eq!(
            storage.value("prefs").unwrap().as_deref(),
            Some(r#"{"theme":"green"}"#)
        );
        assert_eq!(storage.value("absent").unwrap(), None);
        assert_eq!(storage.list_files("reports/", None).unwrap().files.len(), 3);
        for path in ["reports/枝.txt", "reports/BRANCH.txt", "reports/branch.txt"] {
            assert_eq!(storage.read_file(path).unwrap(), path.as_bytes());
        }
    }
    #[test]
    fn invalid_names_and_oversized_values_never_replace_existing_data() {
        let (_dir, storage) = storage();
        storage.set_value("prefs", r#""old""#).unwrap();
        let large = serde_json::to_string(&"x".repeat(VALUE_BYTES as usize)).unwrap();
        assert_eq!(
            storage.set_value("prefs", &large).unwrap_err().status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            storage.set_value("prefs", "not json").unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(storage.value("prefs").unwrap().as_deref(), Some(r#""old""#));
        for path in [
            "../other-app/secret",
            "/absolute",
            "C:/secret",
            "a\\b",
            "con.txt",
            "%2f../escape",
            "a/../b",
            "",
        ] {
            assert!(storage.write_file(path, b"x").is_err(), "{path}");
        }
        assert!(
            storage
                .write_file("large", &vec![0; FILE_BYTES as usize + 1])
                .is_err()
        );
        assert_eq!(storage.usage().unwrap().entries, 1);
    }
    #[test]
    fn quota_and_entry_limits_allow_replacement_and_deletion() {
        let (_dir, mut storage) = storage();
        let writable = Arc::get_mut(&mut storage).unwrap();
        writable.quota = 10;
        writable.entries = 2;
        let write = |path: &str, bytes: &[u8]| storage.write_file(path, bytes);
        write("a", b"12345").unwrap();
        write("b", b"12345").unwrap();
        assert_eq!(
            write("c", b"1").unwrap_err().status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            write("a", b"123456").unwrap_err().status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        write("a", b"1").unwrap();
        assert!(storage.delete_file("b").unwrap());
        assert!(!storage.delete_file("b").unwrap());
        write("c", b"123456789").unwrap();
        assert_eq!(storage.usage().unwrap().bytes, 10);
    }
    #[test]
    fn listings_have_stable_bounded_cursors() {
        let (_dir, storage) = storage();
        for i in 0..105 {
            storage
                .write_file(&format!("notes/{i:03}.txt"), b"")
                .unwrap();
        }
        let first = storage.list_files("notes/", None).unwrap();
        assert_eq!(first.files.len(), 100);
        let second = storage.list_files("notes/", first.next.as_deref()).unwrap();
        assert_eq!(second.files.len(), 5);
        assert_eq!(second.next, None);
        assert_eq!(second.files[0].path, "notes/100.txt");
    }
    #[tokio::test]
    async fn apps_are_isolated_and_concurrent_writes_respect_quota() {
        let (dir, mut storage) = storage();
        Arc::get_mut(&mut storage).unwrap().quota = 5;
        let other = Storage::open(&dir.path().join("another-app/storage")).unwrap();
        let (a, b) = tokio::join!(
            storage.job(|s| s.write_file("a", b"12345")),
            storage.job(|s| s.write_file("b", b"12345"))
        );
        assert_ne!(a.is_ok(), b.is_ok());
        assert_eq!(other.job(|s| s.value("prefs")).await.unwrap(), None);
        other.job(|s| s.set_value("prefs", "42")).await.unwrap();
        assert_eq!(storage.job(|s| s.value("prefs")).await.unwrap(), None);
    }
    #[tokio::test]
    async fn snapshots_wait_for_background_io_even_if_the_request_is_cancelled() {
        let (_dir, storage) = storage();
        let guard = storage.lock.clone().lock_owned().await;
        let (release, wait) = std::sync::mpsc::channel();
        let job = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            wait.recv().unwrap();
        });
        // Dropping a JoinHandle cancels the wait, not an in-progress filesystem job.
        drop(job);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), storage.drain())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        storage.drain().await;
    }
    #[test]
    fn links_cannot_expose_files_outside_the_app() {
        let (dir, storage) = storage();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "private").unwrap();
        let link = dir
            .path()
            .join("storage/files")
            .join(name("escape", false).unwrap());
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("secret"), &link).unwrap();
        #[cfg(windows)]
        {
            assert!(std::process::Command::new("powershell").args(["-NoProfile","-Command","New-Item -ItemType Junction -Path $env:LOKI_STORAGE_LINK -Target $env:LOKI_STORAGE_OUTSIDE -ErrorAction Stop | Out-Null"]).env("LOKI_STORAGE_LINK",&link).env("LOKI_STORAGE_OUTSIDE",outside.path()).status().unwrap().success());
        }
        assert!(storage.read_file("escape").is_err());
        assert!(storage.list_files("", None).is_err());
        assert!(storage.write_file("escape", b"x").is_err());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret")).unwrap(),
            "private"
        );
        #[cfg(windows)]
        std::fs::remove_dir(link).unwrap();
    }
}
