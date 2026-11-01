//! The host API an app calls: its SQLite database, files, storage, and logs.
use super::{FileSource, Host, component_log};
use crate::{Error, Result, apps, files, realtime};
use axum::http::StatusCode;
use futures_util::TryStreamExt;
use serde_json::{Value, json};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::{path::PathBuf, sync::Arc, time::Duration};

use wasmtime::component::Linker;
// SQLite must not become an escape from the component's filesystem sandbox.
unsafe extern "C" fn authorize_sql(
    _: *mut std::ffi::c_void,
    action: i32,
    _: *const std::ffi::c_char,
    name: *const std::ffi::c_char,
    _: *const std::ffi::c_char,
    _: *const std::ffi::c_char,
) -> i32 {
    use libsqlite3_sys::*;
    if matches!(
        action,
        SQLITE_ATTACH | SQLITE_DETACH | SQLITE_PRAGMA | SQLITE_CREATE_VTABLE | SQLITE_DROP_VTABLE
    ) {
        return SQLITE_DENY;
    }
    if action == SQLITE_FUNCTION && !name.is_null() {
        // SAFETY: SQLite supplies NUL-terminated callback arguments for this invocation.
        let function = unsafe { std::ffi::CStr::from_ptr(name) }
            .to_string_lossy()
            .to_ascii_lowercase();
        if function.starts_with("pragma_")
            || matches!(
                function.as_str(),
                "load_extension" | "readfile" | "writefile" | "fts3_tokenizer"
            )
        {
            return SQLITE_DENY;
        }
    }
    SQLITE_OK
}
pub(super) async fn database(path: PathBuf) -> Result<sqlx::SqlitePool> {
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .pragma("trusted_schema", "OFF")
        .pragma("max_page_count", "65536")
        .pragma("temp_store", "FILE")
        .pragma("cache_size", "-2048");
    Ok(SqlitePoolOptions::new()
        .max_connections(2)
        .after_connect(|connection, _| {
            Box::pin(async move {
                let mut handle = connection.lock_handle().await?;
                // SAFETY: the exclusive connection handle is valid for these calls.
                unsafe {
                    let db = handle.as_raw_handle().as_ptr();
                    libsqlite3_sys::sqlite3_limit(
                        db,
                        libsqlite3_sys::SQLITE_LIMIT_LENGTH,
                        1024 * 1024,
                    );
                    libsqlite3_sys::sqlite3_limit(
                        db,
                        libsqlite3_sys::SQLITE_LIMIT_SQL_LENGTH,
                        65536,
                    );
                    libsqlite3_sys::sqlite3_limit(db, libsqlite3_sys::SQLITE_LIMIT_COLUMN, 128);
                    libsqlite3_sys::sqlite3_limit(db, libsqlite3_sys::SQLITE_LIMIT_EXPR_DEPTH, 64);
                }
                // SAFETY: the handle is exclusively locked; the callback is static, uses no
                // user-data pointer, and lives for the entire connection lifetime.
                let code = unsafe {
                    libsqlite3_sys::sqlite3_set_authorizer(
                        handle.as_raw_handle().as_ptr(),
                        Some(authorize_sql),
                        std::ptr::null_mut(),
                    )
                };
                if code != libsqlite3_sys::SQLITE_OK {
                    return Err(sqlx::Error::Protocol(
                        "Could not restrict component database".into(),
                    ));
                }
                Ok(())
            })
        })
        .connect_with(options)
        .await?)
}
pub(super) fn link_files(linker: &mut Linker<Host>) -> Result<()> {
    let mut files = linker.instance("loki:app/files@0.1.0")?;
    files.func_wrap_async(
        "size",
        |mut context, (source, path): (FileSource, String)| {
            Box::new(async move {
                let result = match context.data_mut().open(source, &path).await {
                    Ok(file) => file.metadata().map(|m| m.len()).map_err(|e| e.to_string()),
                    Err(e) => Err(e.message),
                };
                Ok((result,))
            })
        },
    )?;
    files.func_wrap_async(
        "read",
        |mut context, (source, path, offset, length): (FileSource, String, u64, u32)| {
            Box::new(async move {
                let result = match context.data_mut().open(source, &path).await {
                    Ok(file) => {
                        tokio::task::spawn_blocking(move || read_range(file, offset, length))
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r.map_err(|e| e.to_string()))
                    }
                    Err(e) => Err(e.message),
                };
                Ok((result,))
            })
        },
    )?;
    Ok(())
}
/// Reads at most 1 MiB from `offset`; less at the end of the file.
fn read_range(mut file: std::fs::File, offset: u64, length: u32) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek};
    file.seek(std::io::SeekFrom::Start(offset))?;
    let mut data = Vec::new();
    file.take(u64::from(length.min(1024 * 1024)))
        .read_to_end(&mut data)?;
    Ok(data)
}
impl Host {
    pub(crate) fn connections(&self) -> Option<&Arc<realtime::Connections>> {
        self.connections.as_ref()
    }
    /// Opens one of the app's own files, applying the same rules as serving it.
    pub(crate) async fn open(&mut self, source: FileSource, path: &str) -> Result<std::fs::File> {
        let file = match source {
            FileSource::Storage => {
                self.capability("storage")?;
                self.runtime
                    .storage
                    .as_ref()
                    .unwrap()
                    .open_file(path)
                    .await?
            }
            FileSource::Upload | FileSource::Asset => {
                let root = if matches!(source, FileSource::Asset) {
                    if !apps::safe_relative(path)
                        || !self.runtime.manifest.files.contains_key(path)
                        || path == self.runtime.manifest.entry
                    {
                        return Err(Error::forbidden());
                    }
                    self.runtime.package.clone()
                } else {
                    self.capability("uploads")?;
                    self.runtime.data.join("uploads")
                };
                let (dir, relative) = files::confined(&root.to_string_lossy(), path)?;
                dir.open(&relative).map_err(|_| Error::not_found())?
            }
        };
        if !file.metadata()?.is_file() {
            return Err(Error::not_found());
        }
        Ok(file.into_std())
    }
    fn capability(&self, name: &str) -> Result<()> {
        if self.runtime.manifest.capabilities.iter().any(|s| s == name) {
            Ok(())
        } else {
            Err(Error::forbidden())
        }
    }
    pub(super) async fn call(&mut self, operation: &str, input: &str) -> Result<Value> {
        if input.len() > 2 * 1024 * 1024 {
            return Err(Error::bad("Host request is too large."));
        }
        let value: Value = if input.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(input)
                .map_err(|_| Error::bad("Host request is not valid JSON."))?
        };
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| Error::bad("Missing host argument."))
        };
        match operation {
            "context" => Ok(self.info.clone()),
            "form" => {
                self.capability("forms")?;
                Ok(self.form.clone())
            }
            "database" => {
                self.capability("database")?;
                query_database(self.runtime.pool.as_ref().unwrap(), &value).await
            }
            "asset" => {
                let path = text("path")?;
                if !apps::safe_relative(path)
                    || !self.runtime.manifest.files.contains_key(path)
                    || path == self.runtime.manifest.entry
                {
                    return Err(Error::forbidden());
                }
                self.file = Some((path.into(), FileSource::Asset, value.to_string()));
                Ok(Value::Null)
            }
            "file" => {
                self.capability("uploads")?;
                let path = text("path")?;
                if !apps::safe_relative(path) {
                    return Err(Error::forbidden());
                }
                self.file = Some((path.into(), FileSource::Upload, value.to_string()));
                Ok(Value::Null)
            }
            "retain-upload" => {
                self.capability("uploads")?;
                let token = text("token")?;
                let path = text("path")?;
                if !apps::safe_relative(path) || path.starts_with(".pending-") {
                    return Err(Error::forbidden());
                }
                let source = self
                    .pending
                    .iter()
                    .find(|p| p.file_name().and_then(|s| s.to_str()) == Some(token))
                    .ok_or_else(Error::forbidden)?
                    .clone();
                let root = cap_std::fs::Dir::open_ambient_dir(
                    self.runtime.data.join("uploads"),
                    cap_std::ambient_authority(),
                )?;
                if let Some(parent) = std::path::Path::new(path).parent() {
                    root.create_dir_all(parent)?;
                }
                if root.try_exists(path)? {
                    return Err(Error::bad("Upload path already exists."));
                }
                root.rename(source.file_name().unwrap(), &root, path)?;
                Ok(json!({"path":path}))
            }
            "delete-file" => {
                self.capability("uploads")?;
                let path = text("path")?;
                let (dir, path) =
                    files::confined(&self.runtime.data.join("uploads").to_string_lossy(), path)?;
                dir.remove_file(path)?;
                Ok(Value::Null)
            }
            "storage-file-serve" => {
                self.capability("storage")?;
                let path = text("path")?;
                let filename = text("filename")?;
                if filename.is_empty()
                    || filename.len() > 255
                    || filename.contains(['/', '\\'])
                    || filename.chars().any(char::is_control)
                {
                    return Err(Error::bad(
                        "Use a download filename of 1–255 UTF-8 bytes without separators or control characters.",
                    ));
                }
                // Resolve and validate before committing the response marker.
                self.runtime
                    .storage
                    .as_ref()
                    .unwrap()
                    .open_file(path)
                    .await?;
                self.file = Some((path.into(), FileSource::Storage, value.to_string()));
                Ok(Value::Null)
            }
            operation if operation.starts_with("storage-") => {
                self.capability("storage")?;
                self.runtime
                    .storage
                    .as_ref()
                    .unwrap()
                    .call(operation, value)
                    .await
            }
            "log" => {
                component_log(&self.runtime, text("message")?);
                Ok(Value::Null)
            }
            "realtime-restart" => {
                self.capability("realtime")?;
                self.runtime
                    .realtime
                    .get()
                    .ok_or_else(|| {
                        Error::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "The realtime instance is not running.",
                        )
                    })?
                    .restart()
                    .await;
                Ok(Value::Null)
            }
            _ => Err(Error::bad("Unknown LOKI host operation.")),
        }
    }
}
async fn query_database(pool: &sqlx::SqlitePool, value: &Value) -> Result<Value> {
    let statements = value["statements"]
        .as_array()
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .ok_or_else(|| Error::bad("Provide 1–64 SQL statements."))?;
    let mut tx = pool.begin().await?;
    {
        let mut handle = tx.lock_handle().await?;
        let end = std::time::Instant::now() + Duration::from_secs(5);
        handle.set_progress_handler(1000, move || std::time::Instant::now() < end);
    }
    let mut results = Vec::new();
    let mut total = 0usize;
    for statement in statements {
        let sql = statement["sql"]
            .as_str()
            .filter(|s| s.len() <= 65536)
            .ok_or_else(|| Error::bad("Invalid SQL statement."))?;
        let mut query = sqlx::query(sql).persistent(false);
        if let Some(parameters) = statement["params"].as_array() {
            if parameters.len() > 256 {
                return Err(Error::bad("Too many SQL parameters."));
            }
            for parameter in parameters {
                query = match parameter {
                    Value::Null => query.bind(Option::<String>::None),
                    Value::Bool(v) => query.bind(*v),
                    Value::String(v) => query.bind(v),
                    Value::Number(v) if v.is_i64() => query.bind(v.as_i64().unwrap()),
                    Value::Number(v) => query.bind(v.as_f64().unwrap()),
                    _ => return Err(Error::bad("SQL parameters must be scalar values.")),
                };
            }
        }
        // SQLx 0.8's SQLite stream keeps result sizes bounded, including DML.
        #[allow(deprecated)]
        let mut stream = query.fetch_many(&mut *tx);
        let mut rows = Vec::new();
        let mut affected = 0;
        while let Some(item) = stream.try_next().await? {
            match item {
                sqlx::Either::Left(result) => affected += result.rows_affected(),
                sqlx::Either::Right(row) => {
                    let mut object = serde_json::Map::new();
                    for (index, column) in row.columns().iter().enumerate() {
                        let raw = row.try_get_raw(index)?;
                        let value = if raw.is_null() {
                            Value::Null
                        } else {
                            match raw.type_info().name() {
                                "INTEGER" => json!(row.try_get::<i64, _>(index)?),
                                "REAL" => json!(row.try_get::<f64, _>(index)?),
                                "BLOB" => {
                                    return Err(Error::bad(
                                        "Store binary data using uploads, not SQL blobs.",
                                    ));
                                }
                                _ => json!(row.try_get::<String, _>(index)?),
                            }
                        };
                        object.insert(column.name().into(), value);
                    }
                    let value = Value::Object(object);
                    total += value.to_string().len();
                    rows.push(value);
                    if rows.len() > 2000 || total > 4 * 1024 * 1024 {
                        return Err(Error::bad(
                            "Query result exceeds its limit. Use pagination.",
                        ));
                    }
                }
            }
        }
        results.push(json!({"rows":rows,"affected":affected}));
    }
    tx.commit().await?;
    Ok(Value::Array(results))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn component_sql_cannot_attach_files_change_pragmas_or_escape_transactions() {
        let directory = tempfile::tempdir().unwrap();
        let pool = database(directory.path().join("app.db")).await.unwrap();
        let run = |sql: &str| json!({"statements":[{"sql":sql,"params":[]}]});
        query_database(&pool, &run("CREATE TABLE values_test(value TEXT)"))
            .await
            .unwrap();
        for sql in [
            "ATTACH DATABASE ':memory:' AS other",
            "PRAGMA trusted_schema=ON",
            "SELECT * FROM pragma_database_list",
            "SELECT load_extension('evil')",
            "VACUUM INTO 'escaped.db'",
            "SELECT zeroblob(2000000)",
        ] {
            assert!(query_database(&pool, &run(sql)).await.is_err(), "{sql}");
        }
        let failed = json!({"statements":[{"sql":"INSERT INTO values_test VALUES(?)","params":["must roll back"]},{"sql":"INSERT INTO nonexistent VALUES(1)","params":[]}]});
        assert!(query_database(&pool, &failed).await.is_err());
        let result = query_database(&pool, &run("SELECT count(*) AS count FROM values_test"))
            .await
            .unwrap();
        assert_eq!(result[0]["rows"][0]["count"], 0);
        pool.close().await;
    }
}
