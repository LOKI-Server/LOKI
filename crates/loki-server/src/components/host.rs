//! The typed host API an app imports (`loki:app@0.3.0`): its request context and
//! form, SQLite database, uploads, storage, file responses, and logs.
use super::{FileSource, Host, component_log};
use crate::{Error, Result, app_storage, apps, files, realtime};
use axum::http::StatusCode;
use futures_util::TryStreamExt;
use serde::Deserialize;
use serde_json::Value;
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::{path::PathBuf, sync::Arc, time::Duration};
use wasmtime::component::{ComponentType, Lift, Linker, Lower};

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
/// Why a host call failed. Matches `error` in the WIT.
#[derive(Debug, ComponentType, Lower)]
#[component(record)]
pub(crate) struct WitError {
    status: u16,
    message: String,
}
impl From<Error> for WitError {
    fn from(error: Error) -> Self {
        Self {
            status: error.status.as_u16(),
            message: error.message,
        }
    }
}
type HostResult<T> = std::result::Result<T, WitError>;

/// Matches `user` in the WIT. Read from the JSON LOKI builds for each request.
#[derive(Clone, Debug, Deserialize, ComponentType, Lower)]
#[component(record)]
pub(crate) struct User {
    id: String,
    username: String,
    name: String,
    email: String,
    #[serde(default)]
    bio: String,
    #[serde(default)]
    avatar: bool,
    roles: Vec<String>,
}
/// Matches `upload` in the WIT.
#[derive(Clone, Debug, Deserialize, ComponentType, Lower)]
#[component(record)]
struct Upload {
    field: String,
    name: String,
    #[component(name = "content-type")]
    content_type: String,
    size: u64,
    token: String,
}
/// Matches `panel-action` in the WIT.
#[derive(Debug, Deserialize, ComponentType, Lower)]
#[component(record)]
struct PanelAction {
    action: String,
    #[serde(default)]
    value: String,
    #[serde(default, deserialize_with = "pairs")]
    fields: Vec<(String, String)>,
    #[serde(default)]
    files: Vec<Upload>,
}
/// Matches `context` in the WIT.
#[derive(Debug, Default, Deserialize, ComponentType, Lower)]
#[component(record)]
#[serde(default)]
struct Context {
    #[component(name = "app-id")]
    app_id: String,
    origin: String,
    issuer: String,
    #[component(name = "dashboard-url")]
    dashboard_url: String,
    user: Option<User>,
    csrf: String,
    #[serde(deserialize_with = "json_text")]
    config: String,
    #[component(name = "panel-action")]
    panel_action: Option<PanelAction>,
    #[component(name = "lan-bind")]
    lan_bind: String,
    #[component(name = "lan-ips")]
    lan_ips: Vec<String>,
}
/// Matches `form` in the WIT.
#[derive(Debug, Default, Deserialize, ComponentType, Lower)]
#[component(record)]
struct Form {
    #[serde(default, deserialize_with = "pairs")]
    fields: Vec<(String, String)>,
    #[serde(default)]
    files: Vec<Upload>,
}
/// Who opened a realtime connection. Matches `peer` in the WIT's `realtime-handler`.
#[derive(Debug, ComponentType, Lower)]
#[component(record)]
pub(crate) struct ConnectionPeer {
    pub remote: String,
    pub user: Option<User>,
}
fn pairs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<(String, String)>, D::Error> {
    Ok(
        std::collections::BTreeMap::<String, String>::deserialize(deserializer)?
            .into_iter()
            .collect(),
    )
}
fn json_text<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    Ok(Value::deserialize(deserializer)?.to_string())
}
/// Matches `value` in the WIT's `database`.
#[derive(Debug, ComponentType, Lift, Lower)]
#[component(variant)]
enum SqlValue {
    #[component(name = "null")]
    Null,
    #[component(name = "integer")]
    Integer(i64),
    #[component(name = "real")]
    Real(f64),
    #[component(name = "text")]
    Text(String),
}
#[derive(Debug, ComponentType, Lift)]
#[component(record)]
struct Statement {
    sql: String,
    params: Vec<SqlValue>,
}
#[derive(Debug, ComponentType, Lower)]
#[component(record)]
struct QueryResult {
    columns: Vec<String>,
    rows: Vec<Vec<SqlValue>>,
    affected: u64,
}
/// Matches `serve-options` in the WIT.
#[derive(Clone, Debug, Default, ComponentType, Lift)]
#[component(record)]
pub(crate) struct ServeOptions {
    pub filename: Option<String>,
    pub download: bool,
}

/// Links every host interface an app can import, except `realtime`.
pub(super) fn link(linker: &mut Linker<Host>) -> Result<()> {
    let mut request = linker.instance("loki:app/request@0.3.0")?;
    request.func_wrap("get-context", |store, ()| {
        let context: Context = serde_json::from_value(store.data().info.clone())?;
        Ok((context,))
    })?;
    request.func_wrap("get-form", |store, ()| Ok((store.data().form(),)))?;
    request.func_wrap("log", |store, (message,): (String,)| {
        component_log(&store.data().runtime, &message);
        Ok(())
    })?;

    let mut database = linker.instance("loki:app/database@0.3.0")?;
    database.func_wrap_async("transaction", |store, (statements,): (Vec<Statement>,)| {
        Box::new(async move {
            let host = store.data();
            let pool = host
                .capability("database")
                .map(|()| host.runtime.pool.clone().unwrap());
            let result = match pool {
                Ok(pool) => query_database(&pool, statements).await,
                Err(e) => Err(e),
            };
            Ok((result.map_err(WitError::from),))
        })
    })?;

    let mut uploads = linker.instance("loki:app/uploads@0.3.0")?;
    uploads.func_wrap("retain", |mut store, (token, path): (String, String)| {
        Ok((store
            .data_mut()
            .retain(&token, &path)
            .map_err(WitError::from),))
    })?;
    uploads.func_wrap("delete", |store, (path,): (String,)| {
        Ok((store.data().delete_upload(&path).map_err(WitError::from),))
    })?;

    let mut storage = linker.instance("loki:app/storage@0.3.0")?;
    macro_rules! storage {
        ($name:literal, |$s:ident, $args:tt: $types:ty| $work:expr) => {
            storage.func_wrap_async($name, |store, $args: $types| {
                Box::new(async move {
                    let storage = store.data().storage();
                    let result = match storage {
                        Ok(storage) => storage.job(move |$s| $work).await,
                        Err(e) => Err(e),
                    };
                    Ok((result.map_err(WitError::from),))
                })
            })?;
        };
    }
    storage!("set", |s, (key, json): (String, String)| s
        .set_value(&key, &json));
    storage!("get", |s, (key,): (String,)| s.value(&key));
    storage!("delete", |s, (key,): (String,)| s.delete_value(&key));
    storage!("write-file", |s, (path, data): (String, Vec<u8>)| s
        .write_file(&path, &data));
    storage!("read-file", |s, (path,): (String,)| s.read_file(&path));
    storage!("get-file-info", |s, (path,): (String,)| s.file_info(&path));
    storage!("delete-file", |s, (path,): (String,)| s.delete_file(&path));
    storage!(
        "list-files",
        |s, (prefix, after): (String, Option<String>)| s.list_files(&prefix, after.as_deref())
    );
    storage!("get-usage", |s, (): ()| s.usage());

    let mut responses = linker.instance("loki:app/responses@0.3.0")?;
    responses.func_wrap_async(
        "serve-file",
        |mut store, (source, path, options): (FileSource, String, ServeOptions)| {
            Box::new(async move {
                let result = store.data_mut().serve_file(source, path, options).await;
                Ok((result.map_err(WitError::from),))
            })
        },
    )?;

    let mut files = linker.instance("loki:app/files@0.3.0")?;
    files.func_wrap_async("size", |mut store, (source, path): (FileSource, String)| {
        Box::new(async move {
            let result = match store.data_mut().open(source, &path).await {
                Ok(file) => file.metadata().map(|m| m.len()).map_err(Error::from),
                Err(e) => Err(e),
            };
            Ok((result.map_err(WitError::from),))
        })
    })?;
    files.func_wrap_async(
        "read",
        |mut store, (source, path, offset, length): (FileSource, String, u64, u32)| {
            Box::new(async move {
                let result = match store.data_mut().open(source, &path).await {
                    Ok(file) => {
                        tokio::task::spawn_blocking(move || read_range(file, offset, length))
                            .await
                            .map_err(Error::from)
                            .and_then(|r| r.map_err(Error::from))
                    }
                    Err(e) => Err(e),
                };
                Ok((result.map_err(WitError::from),))
            })
        },
    )?;

    linker
        .instance("loki:app/realtime-control@0.3.0")?
        .func_wrap_async("restart", |mut store, ()| {
            Box::new(async move {
                let result = store.data_mut().restart_realtime().await;
                Ok((result.map_err(WitError::from),))
            })
        })?;
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
            FileSource::Storage => self.storage()?.open_file(path).await?,
            FileSource::Upload | FileSource::Asset => {
                let root = if matches!(source, FileSource::Asset) {
                    self.asset(path)?;
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
    /// Assets are the package's declared files, never the component itself.
    fn asset(&self, path: &str) -> Result<()> {
        if !apps::safe_relative(path)
            || !self.runtime.manifest.files.contains_key(path)
            || path == self.runtime.manifest.entry
        {
            return Err(Error::forbidden());
        }
        Ok(())
    }
    fn storage(&self) -> Result<Arc<app_storage::Storage>> {
        self.capability("storage")?;
        Ok(self.runtime.storage.clone().unwrap())
    }
    fn form(&self) -> HostResult<Form> {
        self.capability("forms")?;
        if self.form.is_null() {
            return Ok(Form::default());
        }
        Ok(serde_json::from_value(self.form.clone()).map_err(Error::from)?)
    }
    /// Moves an upload from this request to `path` in the app's uploads.
    fn retain(&mut self, token: &str, path: &str) -> Result<()> {
        self.capability("uploads")?;
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
        Ok(())
    }
    fn delete_upload(&self, path: &str) -> Result<()> {
        self.capability("uploads")?;
        let (dir, path) =
            files::confined(&self.runtime.data.join("uploads").to_string_lossy(), path)?;
        dir.remove_file(path)?;
        Ok(())
    }
    /// Marks the response to be one of the app's files, after checking it may be.
    async fn serve_file(
        &mut self,
        source: FileSource,
        path: String,
        options: ServeOptions,
    ) -> Result<()> {
        if let Some(filename) = &options.filename
            && (filename.is_empty()
                || filename.len() > 255
                || filename.contains(['/', '\\'])
                || filename.chars().any(char::is_control))
        {
            return Err(Error::bad(
                "Use a download filename of 1–255 UTF-8 bytes without separators or control characters.",
            ));
        }
        match source {
            FileSource::Asset => self.asset(&path)?,
            FileSource::Upload => {
                self.capability("uploads")?;
                if !apps::safe_relative(&path) {
                    return Err(Error::forbidden());
                }
            }
            // Resolve and validate before committing the response marker.
            FileSource::Storage => {
                self.storage()?.open_file(&path).await?;
            }
        }
        self.file = Some((path, source, options));
        Ok(())
    }
    async fn restart_realtime(&mut self) -> Result<()> {
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
        Ok(())
    }
}
async fn query_database(
    pool: &sqlx::SqlitePool,
    statements: Vec<Statement>,
) -> Result<Vec<QueryResult>> {
    if statements.is_empty() || statements.len() > 64 {
        return Err(Error::bad("Provide 1–64 SQL statements."));
    }
    let mut tx = pool.begin().await?;
    {
        let mut handle = tx.lock_handle().await?;
        let end = std::time::Instant::now() + Duration::from_secs(5);
        handle.set_progress_handler(1000, move || std::time::Instant::now() < end);
    }
    let mut results = Vec::new();
    let mut total = 0usize;
    for statement in &statements {
        if statement.sql.len() > 65536 {
            return Err(Error::bad("Invalid SQL statement."));
        }
        if statement.params.len() > 256 {
            return Err(Error::bad("Too many SQL parameters."));
        }
        let mut query = sqlx::query(&statement.sql).persistent(false);
        for parameter in &statement.params {
            query = match parameter {
                SqlValue::Null => query.bind(Option::<String>::None),
                SqlValue::Integer(v) => query.bind(*v),
                SqlValue::Real(v) => query.bind(*v),
                SqlValue::Text(v) => query.bind(v),
            };
        }
        // SQLx 0.8's SQLite stream keeps result sizes bounded, including DML.
        #[allow(deprecated)]
        let mut stream = query.fetch_many(&mut *tx);
        let mut result = QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            affected: 0,
        };
        while let Some(item) = stream.try_next().await? {
            match item {
                sqlx::Either::Left(done) => result.affected += done.rows_affected(),
                sqlx::Either::Right(row) => {
                    if result.rows.is_empty() {
                        result.columns = row.columns().iter().map(|c| c.name().into()).collect();
                    }
                    let mut values = Vec::with_capacity(row.columns().len());
                    for index in 0..row.columns().len() {
                        let raw = row.try_get_raw(index)?;
                        let value = if raw.is_null() {
                            SqlValue::Null
                        } else {
                            match raw.type_info().name() {
                                "INTEGER" => SqlValue::Integer(row.try_get(index)?),
                                "REAL" => SqlValue::Real(row.try_get(index)?),
                                "BLOB" => {
                                    return Err(Error::bad(
                                        "Store binary data using uploads, not SQL blobs.",
                                    ));
                                }
                                _ => SqlValue::Text(row.try_get(index)?),
                            }
                        };
                        total += match &value {
                            SqlValue::Text(text) => text.len() + 8,
                            _ => 8,
                        };
                        values.push(value);
                    }
                    result.rows.push(values);
                    if result.rows.len() > 2000 || total > 4 * 1024 * 1024 {
                        return Err(Error::bad(
                            "Query result exceeds its limit. Use pagination.",
                        ));
                    }
                }
            }
        }
        results.push(result);
    }
    tx.commit().await?;
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(sql: &str) -> Vec<Statement> {
        vec![Statement {
            sql: sql.into(),
            params: Vec::new(),
        }]
    }
    #[tokio::test]
    async fn component_sql_cannot_attach_files_change_pragmas_or_escape_transactions() {
        let directory = tempfile::tempdir().unwrap();
        let pool = database(directory.path().join("app.db")).await.unwrap();
        query_database(&pool, run("CREATE TABLE values_test(value TEXT)"))
            .await
            .unwrap();
        for sql in [
            "ATTACH DATABASE ':memory:' AS other",
            "PRAGMA trusted_schema=ON",
            "SELECT * FROM pragma_database_list",
            "SELECT load_extension('evil')",
            "VACUUM INTO 'escaped.db'",
            "SELECT zeroblob(2000000)",
            "SELECT zeroblob(1)",
        ] {
            assert!(query_database(&pool, run(sql)).await.is_err(), "{sql}");
        }
        let failed = vec![
            Statement {
                sql: "INSERT INTO values_test VALUES(?)".into(),
                params: vec![SqlValue::Text("must roll back".into())],
            },
            Statement {
                sql: "INSERT INTO nonexistent VALUES(1)".into(),
                params: Vec::new(),
            },
        ];
        assert!(query_database(&pool, failed).await.is_err());
        let result = query_database(&pool, run("SELECT count(*) AS count FROM values_test"))
            .await
            .unwrap();
        assert_eq!(result[0].columns, ["count"]);
        assert!(
            matches!(result[0].rows[..], [ref row] if matches!(row[..], [SqlValue::Integer(0)]))
        );
        pool.close().await;
    }
    #[tokio::test]
    async fn values_round_trip_with_their_types() {
        let directory = tempfile::tempdir().unwrap();
        let pool = database(directory.path().join("app.db")).await.unwrap();
        let statements = vec![
            Statement {
                sql: "CREATE TABLE t(i INTEGER, r REAL, s TEXT, n TEXT)".into(),
                params: Vec::new(),
            },
            Statement {
                sql: "INSERT INTO t VALUES(?,?,?,?)".into(),
                params: vec![
                    SqlValue::Integer(-7),
                    SqlValue::Real(2.5),
                    SqlValue::Text("枝".into()),
                    SqlValue::Null,
                ],
            },
            Statement {
                sql: "SELECT i,r,s,n FROM t".into(),
                params: Vec::new(),
            },
        ];
        let results = query_database(&pool, statements).await.unwrap();
        assert_eq!(results[1].affected, 1);
        assert_eq!(results[2].columns, ["i", "r", "s", "n"]);
        assert!(matches!(
            &results[2].rows[0][..],
            [SqlValue::Integer(-7), SqlValue::Real(r), SqlValue::Text(s), SqlValue::Null]
                if *r == 2.5 && s == "枝"
        ));
        pool.close().await;
    }
}
