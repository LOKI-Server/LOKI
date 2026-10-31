//! App-scoped durable values and generated files. No guest filesystem paths.
use crate::{Error, Result, apps};
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::STANDARD};
use cap_std::fs::{Dir, File, OpenOptions};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
    sync::Arc,
};

pub const VALUE_BYTES: u64 = 128 * 1024;
pub const FILE_BYTES: u64 = 1024 * 1024;
const TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const ENTRIES: usize = 4096;

pub(crate) struct Storage {
    values: Dir,
    files: Dir,
    lock: Arc<tokio::sync::Mutex<()>>,
    quota: u64,
    entries: usize,
}
fn io(error: std::io::Error) -> Error {
    match error.kind() {
        std::io::ErrorKind::NotFound => Error::not_found(),
        std::io::ErrorKind::PermissionDenied => Error::forbidden(),
        std::io::ErrorKind::AlreadyExists => {
            Error(StatusCode::CONFLICT, "Storage name already exists.".into())
        }
        _ => error.into(),
    }
}
fn limit(message: &str) -> Error {
    Error(StatusCode::PAYLOAD_TOO_LARGE, message.into())
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
    pub fn open(path: &Path) -> Result<Arc<Self>> {
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
                if index >= ENTRIES {
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
            quota: TOTAL_BYTES,
            entries: ENTRIES,
        }))
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
        let (total, count) = self.scan()?;
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
    pub async fn call(self: &Arc<Self>, operation: &str, input: Value) -> Result<Value> {
        // The blocking job owns the guard even if its requesting guest times out.
        let guard = self.lock.clone().lock_owned().await;
        let storage = self.clone();
        let operation = operation.to_owned();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            storage.run(&operation, &input)
        })
        .await?
    }
    fn run(&self, operation: &str, input: &Value) -> Result<Value> {
        let text = |key: &str| {
            input[key]
                .as_str()
                .ok_or_else(|| Error::bad("Missing storage argument."))
        };
        match operation {
            "storage-set" => {
                let key = name(text("key")?, true)?;
                let value = input
                    .get("value")
                    .ok_or_else(|| Error::bad("Missing JSON value."))?;
                self.write(&self.values, &key, &serde_json::to_vec(value)?, VALUE_BYTES)?;
                Ok(Value::Null)
            }
            "storage-get" => {
                let key = name(text("key")?, true)?;
                match Self::read(&self.values, &key, VALUE_BYTES)? {
                    Some(bytes) => {
                        Ok(json!({"found":true,"value":serde_json::from_slice::<Value>(&bytes)?}))
                    }
                    None => Ok(json!({"found":false})),
                }
            }
            "storage-delete" | "storage-file-delete" => {
                let key = operation == "storage-delete";
                let dir = if key { &self.values } else { &self.files };
                let name = name(text(if key { "key" } else { "path" })?, key)?;
                if Self::existing(dir, &name)?.is_none() {
                    return Ok(json!(false));
                }
                dir.remove_file(name).map_err(io)?;
                Ok(json!(true))
            }
            "storage-file-write" => {
                let path = text("path")?;
                let key = name(path, false)?;
                let encoded = text("data")?;
                if encoded.len() > (FILE_BYTES as usize).div_ceil(3) * 4 {
                    return Err(limit("Generated file exceeds 1 MiB."));
                }
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|_| Error::bad("Invalid file bytes."))?;
                self.write(&self.files, &key, &bytes, FILE_BYTES)?;
                Ok(json!({"path":path,"size":bytes.len()}))
            }
            "storage-file-read" => {
                let bytes = Self::read(&self.files, &name(text("path")?, false)?, FILE_BYTES)?
                    .ok_or_else(Error::not_found)?;
                Ok(json!({"data":STANDARD.encode(bytes)}))
            }
            "storage-file-info" => {
                let path = text("path")?;
                let size = Self::existing(&self.files, &name(path, false)?)?
                    .ok_or_else(Error::not_found)?;
                Ok(json!({"path":path,"size":size}))
            }
            "storage-list" => {
                let prefix = input["prefix"].as_str().unwrap_or("");
                let after = input["after"].as_str().unwrap_or("");
                if prefix.len() > 120 || after.len() > 120 {
                    return Err(Error::bad("Invalid storage cursor."));
                }
                self.scan()?;
                let mut entries = Vec::new();
                for entry in self.files.entries().map_err(io)? {
                    let entry = entry.map_err(io)?;
                    let path = String::from_utf8(
                        hex::decode(entry.file_name().to_string_lossy().as_bytes())
                            .map_err(|_| Error::bad("Invalid stored filename."))?,
                    )
                    .map_err(|_| Error::bad("Invalid stored filename."))?;
                    if path.starts_with(prefix) && path.as_str() > after {
                        let size = entry.metadata().map_err(io)?.len();
                        entries.push(json!({"path":path,"size":size}));
                    }
                }
                entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
                let more = entries.len() > 100;
                entries.truncate(100);
                let next = if more {
                    entries.last().map(|v| v["path"].clone())
                } else {
                    None
                };
                Ok(json!({"files":entries,"next":next}))
            }
            "storage-usage" => {
                let (bytes, entries) = self.scan()?;
                Ok(
                    json!({"bytes":bytes,"entries":entries,"limit_bytes":self.quota,"max_entries":self.entries,"max_file_bytes":FILE_BYTES,"max_value_bytes":VALUE_BYTES}),
                )
            }
            _ => Err(Error::bad("Unknown storage operation.")),
        }
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
        storage
            .run(
                "storage-set",
                &json!({"key":"prefs","value":{"theme":"green"}}),
            )
            .unwrap();
        for path in ["reports/枝.txt", "reports/BRANCH.txt", "reports/branch.txt"] {
            storage
                .run(
                    "storage-file-write",
                    &json!({"path":path,"data":STANDARD.encode(path)}),
                )
                .unwrap();
        }
        drop(storage);
        let storage = Storage::open(&dir.path().join("storage")).unwrap();
        assert_eq!(
            storage.run("storage-get", &json!({"key":"prefs"})).unwrap()["value"]["theme"],
            "green"
        );
        assert_eq!(
            storage
                .run("storage-get", &json!({"key":"absent"}))
                .unwrap()["found"],
            false
        );
        assert_eq!(
            storage
                .run("storage-list", &json!({"prefix":"reports/"}))
                .unwrap()["files"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        for path in ["reports/枝.txt", "reports/BRANCH.txt", "reports/branch.txt"] {
            let result = storage
                .run("storage-file-read", &json!({"path":path}))
                .unwrap();
            assert_eq!(
                STANDARD.decode(result["data"].as_str().unwrap()).unwrap(),
                path.as_bytes()
            );
        }
    }
    #[test]
    fn invalid_names_and_oversized_values_never_replace_existing_data() {
        let (_dir, storage) = storage();
        storage
            .run("storage-set", &json!({"key":"prefs","value":"old"}))
            .unwrap();
        assert_eq!(
            storage
                .run(
                    "storage-set",
                    &json!({"key":"prefs","value":"x".repeat(VALUE_BYTES as usize)})
                )
                .unwrap_err()
                .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            storage.run("storage-get", &json!({"key":"prefs"})).unwrap()["value"],
            "old"
        );
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
            assert!(
                storage
                    .run("storage-file-write", &json!({"path":path,"data":"eA=="}))
                    .is_err(),
                "{path}"
            );
        }
        assert!(
            storage
                .run(
                    "storage-file-write",
                    &json!({"path":"large","data":STANDARD.encode(vec![0;FILE_BYTES as usize+1])})
                )
                .is_err()
        );
        assert_eq!(
            storage.run("storage-usage", &Value::Null).unwrap()["entries"],
            1
        );
    }
    #[test]
    fn quota_and_entry_limits_allow_replacement_and_deletion() {
        let (_dir, mut storage) = storage();
        let writable = Arc::get_mut(&mut storage).unwrap();
        writable.quota = 10;
        writable.entries = 2;
        let write = |path: &str, bytes: &[u8]| {
            storage.run(
                "storage-file-write",
                &json!({"path":path,"data":STANDARD.encode(bytes)}),
            )
        };
        write("a", b"12345").unwrap();
        write("b", b"12345").unwrap();
        assert_eq!(
            write("c", b"1").unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            write("a", b"123456").unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        write("a", b"1").unwrap();
        assert_eq!(
            storage
                .run("storage-file-delete", &json!({"path":"b"}))
                .unwrap(),
            true
        );
        assert_eq!(
            storage
                .run("storage-file-delete", &json!({"path":"b"}))
                .unwrap(),
            false
        );
        write("c", b"123456789").unwrap();
        assert_eq!(
            storage.run("storage-usage", &Value::Null).unwrap()["bytes"],
            10
        );
    }
    #[test]
    fn listings_have_stable_bounded_cursors() {
        let (_dir, storage) = storage();
        for i in 0..105 {
            storage
                .run(
                    "storage-file-write",
                    &json!({"path":format!("notes/{i:03}.txt"),"data":""}),
                )
                .unwrap();
        }
        let first = storage
            .run("storage-list", &json!({"prefix":"notes/"}))
            .unwrap();
        assert_eq!(first["files"].as_array().unwrap().len(), 100);
        let second = storage
            .run(
                "storage-list",
                &json!({"prefix":"notes/","after":first["next"]}),
            )
            .unwrap();
        assert_eq!(second["files"].as_array().unwrap().len(), 5);
        assert_eq!(second["next"], Value::Null);
        assert_eq!(second["files"][0]["path"], "notes/100.txt");
    }
    #[tokio::test]
    async fn apps_are_isolated_and_concurrent_writes_respect_quota() {
        let (dir, mut storage) = storage();
        Arc::get_mut(&mut storage).unwrap().quota = 5;
        let other = Storage::open(&dir.path().join("another-app/storage")).unwrap();
        let (a, b) = tokio::join!(
            storage.call("storage-file-write", json!({"path":"a","data":"MTIzNDU="})),
            storage.call("storage-file-write", json!({"path":"b","data":"MTIzNDU="}))
        );
        assert_ne!(a.is_ok(), b.is_ok());
        assert_eq!(
            other
                .call("storage-get", json!({"key":"prefs"}))
                .await
                .unwrap()["found"],
            false
        );
        other
            .call("storage-set", json!({"key":"prefs","value":42}))
            .await
            .unwrap();
        assert_eq!(
            storage
                .call("storage-get", json!({"key":"prefs"}))
                .await
                .unwrap()["found"],
            false
        );
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
        assert!(
            storage
                .run("storage-file-read", &json!({"path":"escape"}))
                .is_err()
        );
        assert!(storage.run("storage-list", &Value::Null).is_err());
        assert!(
            storage
                .run(
                    "storage-file-write",
                    &json!({"path":"escape","data":"eA=="})
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret")).unwrap(),
            "private"
        );
        #[cfg(windows)]
        std::fs::remove_dir(link).unwrap();
    }
}
