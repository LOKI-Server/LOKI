//! Portable WASIp2 HTTP components. No ambient filesystem, sockets, or secrets.
use crate::{Error, Result, State, app_storage, apps, auth, files, realtime};
use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use futures_util::TryStreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::Semaphore,
};
use wasmtime::component::{Component, ComponentType, Lift, Linker, Lower, ResourceTable};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    WasiHttpCtx,
    p2::{
        WasiHttpCtxView, WasiHttpView,
        bindings::{
            ProxyPre,
            http::types::{ErrorCode, Scheme},
        },
    },
};

pub use loki_sdk::manifest::Limits;

pub struct Runtime {
    pre: ProxyPre<Host>,
    pool: Option<sqlx::SqlitePool>,
    storage: Option<Arc<app_storage::Storage>>,
    slots: Arc<Semaphore>,
    active: AtomicBool,
    manifest: apps::Manifest,
    data: PathBuf,
    package: PathBuf,
    config: Value,
    origin: String,
    pub(crate) realtime: std::sync::OnceLock<Arc<realtime::Handle>>,
}
impl Runtime {
    pub async fn stop(&self) {
        self.active.store(false, Ordering::SeqCst);
        // Long-lived connections end first, so nothing holds the app open.
        if let Some(realtime) = self.realtime.get() {
            realtime.stop().await;
        }
        let _drain = self
            .slots
            .acquire_many(self.manifest.limits.concurrent_requests as u32)
            .await;
        if let Some(pool) = &self.pool {
            pool.close().await;
        }
        if let Some(storage) = &self.storage {
            storage.drain().await;
        }
    }
}
pub(crate) struct Host {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: DenyNetwork,
    table: ResourceTable,
    limits: StoreLimits,
    runtime: Arc<Runtime>,
    info: Value,
    form: Value,
    pending: Vec<PathBuf>,
    file: Option<(String, FileSource, String)>,
    /// Set only in an app's realtime instance.
    connections: Option<Arc<realtime::Connections>>,
}
/// Where an app's file lives. Matches `file-source` in the WIT, in the same order.
#[derive(Clone, Copy, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
pub(crate) enum FileSource {
    #[component(name = "upload")]
    Upload,
    #[component(name = "storage")]
    Storage,
    #[component(name = "asset")]
    Asset,
}
impl Drop for Host {
    fn drop(&mut self) {
        for p in &self.pending {
            let _ = std::fs::remove_file(p);
        }
    }
}
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}
struct DenyNetwork;
impl wasmtime_wasi_http::p2::WasiHttpHooks for DenyNetwork {
    fn send_request(
        &mut self,
        _: Request<wasmtime_wasi_http::p2::body::HyperOutgoingBody>,
        _: wasmtime_wasi_http::p2::types::OutgoingRequestConfig,
    ) -> wasmtime_wasi_http::p2::HttpResult<wasmtime_wasi_http::p2::types::HostFutureIncomingResponse>
    {
        Err(ErrorCode::HttpRequestDenied.into())
    }
    fn outgoing_body_chunk_size(&mut self) -> usize {
        65536
    }
}
impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

async fn engine(state: &State) -> Result<Engine> {
    Ok(state
        .0
        .wasm_engine
        .get_or_try_init(|| async {
            tokio::task::spawn_blocking(|| {
                let mut config = wasmtime::Config::new();
                config
                    .wasm_component_model(true)
                    .consume_fuel(true)
                    .max_wasm_stack(512 * 1024);
                Engine::new(&config)
            })
            .await?
        })
        .await?
        .clone())
}

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
async fn database(path: PathBuf) -> Result<sqlx::SqlitePool> {
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
pub async fn start(
    state: &State,
    app: &apps::App,
    manifest: &apps::Manifest,
) -> Result<Arc<Runtime>> {
    let engine = engine(state).await?;
    let path = state
        .0
        .data
        .join("apps")
        .join(&app.id)
        .join(&manifest.entry);
    if std::fs::metadata(&path)?.len() > 32 * 1024 * 1024 {
        return Err(Error::bad("Component exceeds the 32 MiB code limit."));
    }
    let compiler = engine.clone();
    let component =
        tokio::task::spawn_blocking(move || Component::from_file(&compiler, path)).await??;
    let mut linker = Linker::<Host>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    linker
        .instance("loki:app/platform@0.1.0")?
        .func_wrap_async(
            "call",
            |mut context, (operation, input): (String, String)| {
                Box::new(async move {
                    let result = context
                        .data_mut()
                        .call(&operation, &input)
                        .await
                        .map(|v| v.to_string())
                        .map_err(|e| {
                            json!({"loki_error":1,"status":e.0.as_u16(),"message":e.1}).to_string()
                        });
                    Ok((result,))
                })
            },
        )?;
    link_files(&mut linker)?;
    realtime::link(&mut linker)?;
    let pre = ProxyPre::new(linker.instantiate_pre(&component)?)?;
    let data = state.0.data.join("app-data").join(&app.id);
    let pool = if manifest.capabilities.iter().any(|c| c == "database") {
        Some(database(data.join("app.db")).await?)
    } else {
        None
    };
    std::fs::create_dir_all(data.join("uploads"))?;
    let storage = if manifest.capabilities.iter().any(|c| c == "storage") {
        let path = data.join("storage");
        Some(tokio::task::spawn_blocking(move || app_storage::Storage::open(&path)).await??)
    } else {
        None
    };
    let runtime = Arc::new(Runtime {
        pre,
        pool,
        storage,
        slots: Arc::new(Semaphore::new(manifest.limits.concurrent_requests)),
        active: AtomicBool::new(true),
        manifest: manifest.clone(),
        data,
        package: state.0.data.join("apps").join(&app.id),
        config: serde_json::from_str(&app.config)?,
        origin: apps::public_url(app),
        realtime: std::sync::OnceLock::new(),
    });
    register_client(state, app).await?;
    let request = Request::builder()
        .uri(&manifest.health)
        .header(header::HOST, url::Url::parse(&runtime.origin)?.authority())
        .body(Body::empty())?;
    let permit = enter(state, &runtime).await?;
    let response = execute(
        state,
        runtime.clone(),
        request,
        json!({"user":null,"csrf":"","issuer":state.0.settings.read().await.public_url}),
        Value::Null,
        Vec::new(),
    )
    .await;
    drop(permit);
    let response = response?;
    if !response.status().is_success() {
        runtime.stop().await;
        return Err(Error::bad("Component readiness check failed."));
    }
    if manifest.realtime().is_some() {
        match realtime::start(state, runtime.clone(), app).await {
            Ok(handle) => {
                let _ = runtime.realtime.set(handle);
            }
            Err(error) => {
                runtime.stop().await;
                return Err(error);
            }
        }
    }
    Ok(runtime)
}
pub async fn register_client(state: &State, app: &apps::App) -> Result<()> {
    sqlx::query("INSERT INTO clients(id,secret,resource,redirect) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET redirect=excluded.redirect")
        .bind(format!("component-{}",app.id)).bind(auth::digest(&app.secret)).bind(&app.id).bind(format!("{}/_loki/callback",apps::public_url(app))).execute(&state.0.db).await?;
    Ok(())
}
async fn execute(
    _state: &State,
    runtime: Arc<Runtime>,
    mut request: Request<Body>,
    info: Value,
    form: Value,
    pending: Vec<PathBuf>,
) -> Result<Response> {
    let _cleanup = UploadCleanup(pending.clone());
    if !runtime.active.load(Ordering::SeqCst) {
        return Err(Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "This application is stopped.".into(),
        ));
    }
    let limits = runtime.manifest.limits.clone();
    let mut store = new_store(&runtime, info, form, pending, None)?;
    store.set_fuel(limits.fuel)?;
    // Authentication credentials stay in the host; identity comes from context.
    request.headers_mut().remove(header::COOKIE);
    request.headers_mut().remove(header::AUTHORIZATION);
    let headers = request.headers().clone();
    let request = request.map(|body| {
        http_body_util::Limited::new(body, limits.request_bytes)
            .map_err(|_| ErrorCode::HttpRequestBodySize(None))
    });
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let incoming = store.data_mut().http().new_incoming_request(
        if runtime.origin.starts_with("https:") {
            Scheme::Https
        } else {
            Scheme::Http
        },
        request,
    )?;
    let out = store.data_mut().http().new_response_outparam(sender)?;
    let run = async {
        let proxy = runtime.pre.instantiate_async(&mut store).await?;
        let call = proxy
            .wasi_http_incoming_handler()
            .call_handle(&mut store, incoming, out);
        tokio::pin!(call);
        // Read the response concurrently, so WASI stream backpressure cannot deadlock.
        let response = async {
            let response = receiver
                .await
                .map_err(|_| anyhow::anyhow!("Component did not return a response"))?
                .map_err(|e| anyhow::anyhow!("Component HTTP error: {e:?}"))?;
            let (parts, body) = response.into_parts();
            let bytes = http_body_util::Limited::new(body, 8 * 1024 * 1024)
                .collect()
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .to_bytes();
            Ok::<_, anyhow::Error>(Response::from_parts(parts, Body::from(bytes)))
        };
        let (_, response) =
            tokio::try_join!(async { call.await.map_err(anyhow::Error::from) }, response)?;
        Ok::<_, anyhow::Error>(response)
    };
    let result = tokio::time::timeout(Duration::from_secs(limits.timeout_seconds), run).await;
    let mut response = match result {
        Ok(Ok(response)) => response,
        failed => {
            let detail = match failed {
                Ok(Err(e)) => format!("{e:#}"),
                _ => "Component request exceeded its time limit".into(),
            };
            component_log(&runtime, &detail);
            return Err(Error(StatusCode::BAD_GATEWAY,"The application request failed or exceeded its resource limit. LOKI is still available.".into()));
        }
    };
    if let Some((path, asset, options)) = store.data_mut().file.take()
        && response.status().is_success()
    {
        response = stream_file(&runtime, &path, asset, &options, &headers).await?;
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    response.headers_mut().remove(header::SET_COOKIE);
    Ok(response)
}
/// A store for one component instance, with the app's memory limits and no ambient access.
pub(crate) fn new_store(
    runtime: &Arc<Runtime>,
    mut info: Value,
    form: Value,
    pending: Vec<PathBuf>,
    connections: Option<Arc<realtime::Connections>>,
) -> Result<Store<Host>> {
    info["config"] = runtime.config.clone();
    info["origin"] = json!(runtime.origin);
    info["app_id"] = json!(runtime.manifest.id);
    let limits = &runtime.manifest.limits;
    let mut wasi = WasiCtx::builder();
    // No inherited environment, stdio, preopened directories, or network access.
    wasi.allow_tcp(false)
        .allow_udp(false)
        .allow_ip_name_lookup(false);
    let mut table = ResourceTable::new();
    table.set_max_capacity(256);
    let mut http = WasiHttpCtx::new();
    http.set_field_size_limit(64 * 1024);
    let mut store = Store::new(
        runtime.pre.engine(),
        Host {
            wasi: wasi.build(),
            http,
            hooks: DenyNetwork,
            table,
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.memory_mib * 1024 * 1024)
                .table_elements(100_000)
                .instances(64)
                .memories(8)
                .tables(8)
                .build(),
            runtime: runtime.clone(),
            info,
            form,
            pending,
            file: None,
            connections,
        },
    );
    store.limiter(|host| &mut host.limits);
    store.fuel_async_yield_interval(Some(10_000))?;
    Ok(store)
}
impl Runtime {
    pub(crate) fn pre(&self) -> &ProxyPre<Host> {
        &self.pre
    }
    pub(crate) fn manifest(&self) -> &apps::Manifest {
        &self.manifest
    }
    pub(crate) fn log(&self, message: &str) {
        component_log(self, message);
    }
}
fn link_files(linker: &mut Linker<Host>) -> Result<()> {
    let mut files = linker.instance("loki:app/files@0.1.0")?;
    files.func_wrap_async(
        "size",
        |mut context, (source, path): (FileSource, String)| {
            Box::new(async move {
                let result = match context.data_mut().open(source, &path).await {
                    Ok(file) => file.metadata().map(|m| m.len()).map_err(|e| e.to_string()),
                    Err(e) => Err(e.1),
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
                    Err(e) => Err(e.1),
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
async fn enter(
    state: &State,
    runtime: &Runtime,
) -> Result<(
    tokio::sync::OwnedSemaphorePermit,
    tokio::sync::OwnedSemaphorePermit,
)> {
    let global = tokio::time::timeout(
        Duration::from_secs(10),
        state.0.component_slots.clone().acquire_owned(),
    )
    .await
    .map_err(|_| {
        Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Components are busy; try again shortly.".into(),
        )
    })??;
    let local = tokio::time::timeout(
        Duration::from_secs(10),
        runtime.slots.clone().acquire_owned(),
    )
    .await
    .map_err(|_| {
        Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "This component is busy; try again shortly.".into(),
        )
    })??;
    if !runtime.active.load(Ordering::SeqCst) {
        return Err(Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The component is stopped.".into(),
        ));
    }
    Ok((global, local))
}
fn component_log(runtime: &Runtime, message: &str) {
    use std::io::Write;
    let path = runtime
        .data
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("logs")
        .join(format!("{}.stderr.log", runtime.manifest.id));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1024 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            file,
            "{} {}",
            auth::now(),
            message.chars().take(2000).collect::<String>()
        );
    }
}

struct UploadCleanup(Vec<PathBuf>);
impl Drop for UploadCleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
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
    async fn call(&mut self, operation: &str, input: &str) -> Result<Value> {
        if input.len() > 2 * 1024 * 1024 {
            return Err(Error::bad("Host request is too large."));
        }
        let value: Value = if input.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(input)?
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
                        Error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "The realtime instance is not running.".into(),
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

async fn stream_file(
    runtime: &Runtime,
    path: &str,
    source: FileSource,
    options: &str,
    headers: &HeaderMap,
) -> Result<Response> {
    let asset = matches!(source, FileSource::Asset);
    let relative = std::path::Path::new(path);
    let file = if matches!(source, FileSource::Storage) {
        runtime
            .storage
            .as_ref()
            .ok_or_else(Error::forbidden)?
            .open_file(path)
            .await?
    } else {
        let root = if asset {
            runtime.package.clone()
        } else {
            runtime.data.join("uploads")
        };
        let (dir, relative) = files::confined(&root.to_string_lossy(), path)?;
        dir.open(&relative).map_err(|_| Error::not_found())?
    };
    if !file.metadata()?.is_file() {
        return Err(Error::not_found());
    }
    let length = file.metadata()?.len();
    let mut status = StatusCode::OK;
    let (start, end) = if let Some(range) = headers.get(header::RANGE) {
        if let Some(range) = range.to_str().ok().and_then(|r| files::range(r, length)) {
            status = StatusCode::PARTIAL_CONTENT;
            range
        } else {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{length}"))],
            )
                .into_response());
        }
    } else {
        (0, length.saturating_sub(1))
    };
    let count = if length == 0 { 0 } else { end - start + 1 };
    let mut file = tokio::fs::File::from_std(file.into_std());
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let options: Value = serde_json::from_str(options)?;
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, count)
        .header(header::ACCEPT_RANGES, "bytes");
    let content_type = if asset {
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string()
    } else {
        files::preview_type(relative).unwrap_or_else(|| "application/octet-stream".into())
    };
    response = response.header(header::CONTENT_TYPE, content_type);
    if !asset {
        let filename = options["filename"].as_str().unwrap_or("download");
        let encoded = url::form_urlencoded::byte_serialize(filename.as_bytes())
            .collect::<String>()
            .replace('+', "%20");
        let download = options["download"].as_bool().unwrap_or(false)
            || files::preview_type(relative).is_none();
        response = response.header(
            header::CONTENT_DISPOSITION,
            format!(
                "{}; filename=\"download\"; filename*=UTF-8''{encoded}",
                if download { "attachment" } else { "inline" }
            ),
        );
        if download {
            response = response.header("Content-Security-Policy", "sandbox; default-src 'none'");
        }
    }
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{length}"),
        );
    }
    let body = if headers.get("x-loki-head").is_some() {
        Body::empty()
    } else {
        Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
            file.take(count),
            65536,
        ))
    };
    Ok(response.body(body)?)
}

pub async fn resolve(state: &State, headers: &HeaderMap) -> Result<Option<apps::App>> {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return Ok(None);
    };
    let url = url::Url::parse(&format!("http://{host}"))?;
    let hostname = url.host_str().unwrap_or("");
    let local = hostname.strip_suffix(".localhost");
    let candidates: Vec<apps::App> = if let Some(id) = local {
        if !apps::valid_id(id) || url.port_or_known_default() != Some(state.0.bind.port()) {
            return Ok(None);
        }
        sqlx::query_as("SELECT * FROM apps WHERE id=? AND hostname='' ")
            .bind(id)
            .fetch_all(&state.0.db)
            .await?
    } else {
        sqlx::query_as("SELECT * FROM apps WHERE hostname=? AND hostname!=''")
            .bind(hostname)
            .fetch_all(&state.0.db)
            .await?
    };
    for app in candidates {
        if serde_json::from_str::<apps::Manifest>(&app.manifest)?.runtime == "wasi" {
            return Ok(Some(app));
        }
    }
    Ok(None)
}
fn session_cookie(origin: &str) -> &'static str {
    if origin.starts_with("https:") {
        "__Host-loki_component"
    } else {
        "loki_component"
    }
}
pub async fn serve(state: &State, app: apps::App, mut request: Request<Body>) -> Result<Response> {
    let origin = apps::public_url(&app);
    let path = request.uri().path().to_string();
    if !app.enabled {
        return Err(Error::not_found());
    }
    if path == "/_loki/start" {
        return login_start(state, &app, request.headers()).await;
    }
    if path == "/_loki/callback" {
        return login_callback(state, &app, request).await;
    }
    // A device signed in with a code sends its access token instead of a cookie.
    let bearer = match request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        Some(token) => {
            crate::identity::token_user(state, token, Some(&format!("component-{}", app.id)))
                .await?
                .map(|(user, ..)| user)
        }
        None => None,
    };
    let jar = axum_extra::extract::cookie::CookieJar::from_headers(request.headers());
    let raw = jar
        .get(session_cookie(&origin))
        .map(|c| c.value().to_string())
        .filter(|_| bearer.is_none());
    let uid = if let Some(raw) = &raw {
        sqlx::query_scalar::<_, String>(
            "SELECT user_id FROM gateway_sessions WHERE id=? AND resource=? AND expires>?",
        )
        .bind(auth::digest(raw))
        .bind(&app.id)
        .bind(auth::now())
        .fetch_optional(&state.0.db)
        .await?
    } else {
        None
    };
    let user = if let Some(uid) = uid {
        sqlx::query_as::<_, auth::User>("SELECT * FROM users WHERE id=? AND status='approved'")
            .bind(uid)
            .fetch_optional(&state.0.db)
            .await?
    } else {
        bearer.clone()
    };
    let manifest: apps::Manifest = serde_json::from_str(&app.manifest)?;
    let allowed = auth::allowed(state, user.as_ref(), &app.id, "access").await?;
    // Realtime sockets are open to anyone who can reach the app, so players need no
    // LOKI account. A signed-in, same-origin browser also says who it is.
    if let Some(settings) = manifest.realtime()
        && path == settings.path
        && request.method() == axum::http::Method::GET
        && realtime::is_upgrade(request.headers())
    {
        let same_origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|o| o == origin);
        let user = match user
            .as_ref()
            .filter(|_| allowed && (same_origin || bearer.is_some()))
        {
            Some(user) => user_info(state, user, &app.id, &manifest).await?,
            None => Value::Null,
        };
        return realtime::upgrade(state, &app.id, request, user).await;
    }
    if !allowed {
        return if user.is_none() {
            Ok(Redirect::to("/_loki/start").into_response())
        } else {
            Err(Error::forbidden())
        };
    }
    let csrf = raw
        .as_ref()
        .map(|r| auth::digest(&format!("component-csrf:{r}")))
        .unwrap_or_default();
    let user_value = match &user {
        Some(user) => user_info(state, user, &app.id, &manifest).await?,
        None => Value::Null,
    };
    let info = json!({"user":user_value,"csrf":csrf,"issuer":state.0.settings.read().await.public_url,"dashboard_url":dashboard_url(state,&origin).await});
    let runtime = running(state, &app.id).await?;
    let _permit = enter(state, &runtime).await?;
    let mut form = Value::Null;
    let mut pending = Vec::new();
    let method = request.method().clone();
    if !matches!(
        method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    ) {
        if user.is_none() {
            return Err(Error::unauthorized());
        }
        // Bearer tokens are never sent by browsers on their own, so need no CSRF token.
        let cookie = bearer.is_none();
        if cookie
            && request
                .headers()
                .get(header::ORIGIN)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|o| o != origin)
        {
            return Err(Error::forbidden());
        }
        if manifest.capabilities.iter().any(|c| c == "forms")
            && request
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|t| {
                    t.starts_with("application/x-www-form-urlencoded")
                        || t.starts_with("multipart/form-data")
                })
        {
            let (parts, body) = request.into_parts();
            let parsed = tokio::time::timeout(
                Duration::from_secs(manifest.limits.timeout_seconds),
                parse_form(&runtime, &parts.headers, body),
            )
            .await
            .map_err(|_| Error::bad("Upload timed out."))??;
            form = parsed.0;
            pending = parsed.1;
            let mut cleanup = UploadCleanup(pending.clone());
            if cookie && !auth::equal(form["fields"]["csrf_token"].as_str().unwrap_or(""), &csrf) {
                return Err(Error::forbidden());
            }
            cleanup.0.clear();
            request = Request::from_parts(parts, Body::empty());
        } else if cookie
            && !auth::equal(
                request
                    .headers()
                    .get("x-csrf-token")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or(""),
                &csrf,
            )
        {
            return Err(Error::forbidden());
        }
    }
    if path == "/logout" && method == axum::http::Method::POST {
        if let Some(raw) = raw {
            sqlx::query("DELETE FROM gateway_sessions WHERE id=? AND resource=?")
                .bind(auth::digest(&raw))
                .bind(&app.id)
                .execute(&state.0.db)
                .await?;
        }
        return Ok(Redirect::to(&dashboard_url(state, &origin).await).into_response());
    }
    request.headers_mut().remove("x-loki-head");
    if method == axum::http::Method::HEAD {
        request
            .headers_mut()
            .insert("x-loki-head", "1".parse().unwrap());
    }
    let mut response = execute(state, runtime, request, info, form, pending).await?;
    if method == axum::http::Method::HEAD {
        *response.body_mut() = Body::empty();
    }
    Ok(response)
}

/// The signed-in user as the guest sees them, with the app roles they hold.
async fn user_info(
    state: &State,
    user: &auth::User,
    app: &str,
    manifest: &apps::Manifest,
) -> Result<Value> {
    let mut roles = vec!["access"];
    for role in &manifest.roles {
        if auth::allowed(state, Some(user), app, role).await? {
            roles.push(role.as_str());
        }
    }
    Ok(
        json!({"id":user.id,"username":user.username,"name":user.display_name,"email":user.email,"roles":roles}),
    )
}
/// The app's runtime, if it is ready to serve.
pub(crate) async fn running(state: &State, app: &str) -> Result<Arc<Runtime>> {
    state
        .0
        .runtimes
        .lock()
        .await
        .get(app)
        .filter(|r| r.ready)
        .and_then(|r| r.component.clone())
        .ok_or_else(|| {
            Error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The component is not running.".into(),
            )
        })
}
/// Serves the app's health path anonymously, for the app's LAN listener.
pub(crate) async fn health(
    state: &State,
    runtime: Arc<Runtime>,
    request: Request<Body>,
) -> Result<Response> {
    let _permit = enter(state, &runtime).await?;
    let info = json!({"user":null,"csrf":"","issuer":state.0.settings.read().await.public_url});
    execute(state, runtime, request, info, Value::Null, Vec::new()).await
}
async fn parse_form(
    runtime: &Runtime,
    headers: &HeaderMap,
    body: Body,
) -> Result<(Value, Vec<PathBuf>)> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("application/x-www-form-urlencoded") {
        let bytes = axum::body::to_bytes(body, 65536)
            .await
            .map_err(|_| Error::bad("Form is too large."))?;
        let mut fields = serde_json::Map::new();
        for (key, value) in url::form_urlencoded::parse(&bytes) {
            if fields.insert(key.into_owned(), json!(value)).is_some() {
                return Err(Error::bad("Duplicate form field."));
            }
        }
        return Ok((json!({"fields":fields,"files":[]}), Vec::new()));
    }
    if !runtime.manifest.capabilities.iter().any(|c| c == "uploads") {
        return Err(Error::forbidden());
    }
    let boundary =
        multer::parse_boundary(content_type).map_err(|_| Error::bad("Invalid multipart form."))?;
    let max = runtime.config["max-upload"]
        .as_u64()
        .unwrap_or(runtime.manifest.limits.request_bytes as u64)
        .min(runtime.manifest.limits.request_bytes as u64);
    let constraints = multer::Constraints::new().size_limit(
        multer::SizeLimit::new()
            .whole_stream(runtime.manifest.limits.request_bytes as u64)
            .per_field(max),
    );
    let mut multipart =
        multer::Multipart::with_constraints(body.into_data_stream(), boundary, constraints);
    let mut cleanup = UploadCleanup(Vec::new());
    let mut fields = serde_json::Map::new();
    let mut uploads = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| Error::bad("Upload exceeds its limit or is malformed."))?
    {
        let name = field.name().unwrap_or("").to_string();
        if seen.len() >= 64 || name.len() > 64 || !seen.insert(name.clone()) {
            return Err(Error::bad("Too many or duplicate form fields."));
        }
        if let Some(filename) = field
            .file_name()
            .filter(|n| !n.is_empty())
            .map(str::to_string)
        {
            if uploads.len() >= 8 || filename.len() > 1024 {
                return Err(Error::bad("Too many files or filename too long."));
            }
            let token = format!(".pending-{}", auth::id());
            let path = runtime.data.join("uploads").join(&token);
            let content_type = field
                .content_type()
                .map(ToString::to_string)
                .unwrap_or_else(|| "application/octet-stream".into());
            let mut output = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await?;
            cleanup.0.push(path);
            let mut size = 0u64;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|_| Error::bad("Upload exceeds its limit or is malformed."))?
            {
                size += chunk.len() as u64;
                if size > max {
                    return Err(Error::bad("File exceeds the configured upload limit."));
                }
                output.write_all(&chunk).await?;
            }
            output.flush().await?;
            uploads.push(json!({"field":name,"name":filename,"token":token,"content_type":content_type,"size":size}));
        } else {
            let mut text = Vec::new();
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|_| Error::bad("Invalid form field."))?
            {
                if text.len() + chunk.len() > 65536 {
                    return Err(Error::bad("Form field is too large."));
                }
                text.extend_from_slice(&chunk);
            }
            fields.insert(
                name,
                json!(
                    String::from_utf8(text)
                        .map_err(|_| Error::bad("Form fields must be UTF-8."))?
                ),
            );
        }
    }
    let pending = std::mem::take(&mut cleanup.0);
    Ok((json!({"fields":fields,"files":uploads}), pending))
}

async fn dashboard_url(state: &State, origin: &str) -> String {
    if origin.starts_with("http:") {
        format!("http://127.0.0.1:{}", state.0.bind.port())
    } else {
        state.0.settings.read().await.public_url.clone()
    }
}
async fn login_start(state: &State, app: &apps::App, headers: &HeaderMap) -> Result<Response> {
    use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    let raw = auth::random();
    let verifier = auth::random();
    let origin = apps::public_url(app);
    sqlx::query("INSERT INTO gateway_flows(id,resource,verifier,expires) VALUES(?,?,?,?)")
        .bind(auth::digest(&raw))
        .bind(&app.id)
        .bind(&verifier)
        .bind(auth::now() + 300)
        .execute(&state.0.db)
        .await?;
    let mut url = url::Url::parse(&format!(
        "{}/oidc/authorize",
        dashboard_url(state, &origin).await
    ))?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", format!("component-{}", app.id)),
        ("redirect_uri", format!("{origin}/_loki/callback")),
        ("response_type", "code".into()),
        ("scope", "openid".into()),
        ("state", raw.clone()),
        ("nonce", auth::random()),
        (
            "code_challenge",
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        ),
        ("code_challenge_method", "S256".into()),
    ]);
    let name = if origin.starts_with("https:") {
        "__Host-loki_component_flow"
    } else {
        "loki_component_flow"
    };
    let cookie = Cookie::build((name, raw))
        .http_only(true)
        .secure(origin.starts_with("https:"))
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::minutes(5))
        .build();
    Ok((
        CookieJar::from_headers(headers).add(cookie),
        Redirect::to(url.as_str()),
    )
        .into_response())
}
async fn login_callback(
    state: &State,
    app: &apps::App,
    request: Request<Body>,
) -> Result<Response> {
    use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    let query: std::collections::HashMap<_, _> =
        url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let raw = query.get("state").ok_or_else(Error::forbidden)?;
    let code = query.get("code").ok_or_else(Error::forbidden)?;
    let origin = apps::public_url(app);
    let jar = CookieJar::from_headers(request.headers());
    let flow_name = if origin.starts_with("https:") {
        "__Host-loki_component_flow"
    } else {
        "loki_component_flow"
    };
    if !jar
        .get(flow_name)
        .is_some_and(|c| auth::equal(c.value(), raw))
    {
        return Err(Error::forbidden());
    }
    let mut tx = state.0.db.begin().await?;
    let verifier: String = sqlx::query_scalar(
        "DELETE FROM gateway_flows WHERE id=? AND resource=? AND expires>? RETURNING verifier",
    )
    .bind(auth::digest(raw))
    .bind(&app.id)
    .bind(auth::now())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(Error::forbidden)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let uid:String=sqlx::query_scalar("DELETE FROM codes WHERE id=? AND client=? AND redirect=? AND challenge=? AND expires>? AND auth_version=(SELECT auth_version FROM users WHERE users.id=codes.user_id) RETURNING user_id").bind(auth::digest(code)).bind(format!("component-{}",app.id)).bind(format!("{origin}/_loki/callback")).bind(challenge).bind(auth::now()).fetch_optional(&mut *tx).await?.ok_or_else(Error::forbidden)?;
    let session = auth::random();
    sqlx::query("INSERT INTO gateway_sessions(id,resource,user_id,expires) SELECT ?,?,?,? WHERE EXISTS(SELECT 1 FROM users WHERE id=? AND status='approved')").bind(auth::digest(&session)).bind(&app.id).bind(&uid).bind(auth::now()+604800).bind(&uid).execute(&mut *tx).await?;
    tx.commit().await?;
    let user: auth::User = sqlx::query_as("SELECT * FROM users WHERE id=?")
        .bind(&uid)
        .fetch_one(&state.0.db)
        .await?;
    if !auth::allowed(state, Some(&user), &app.id, "access").await? {
        return Err(Error::forbidden());
    }
    let cookie = Cookie::build((session_cookie(&origin), session))
        .http_only(true)
        .secure(origin.starts_with("https:"))
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::days(7))
        .build();
    Ok((
        jar.remove(Cookie::build((flow_name, "")).path("/").build())
            .add(cookie),
        Redirect::to("/"),
    )
        .into_response())
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
