//! Portable WASIp2 HTTP components. No ambient filesystem, sockets, or secrets.
mod host;
mod http;

use crate::{Error, Result, State, app_storage, apps, auth, realtime};
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use host::{database, link_files};
use http::execute;
pub use http::{panel, resolve, serve};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;
use wasmtime::component::{Component, ComponentType, Lift, Linker, Lower, ResourceTable};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    WasiHttpCtx,
    p2::{
        WasiHttpCtxView, WasiHttpView,
        bindings::{ProxyPre, http::types::ErrorCode},
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
    /// The owner's LAN address for a realtime app, and this server's own address
    /// there, so the app can tell people where to connect.
    lan_bind: String,
    lan_ips: Vec<String>,
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
                            json!({"loki_error":1,"status":e.status.as_u16(),"message":e.message})
                                .to_string()
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
        lan_bind: app.lan_bind.clone(),
        lan_ips: lan_ips(&app.lan_bind),
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
/// The address LAN devices reach for `lan_bind`: its own IP, or for a wildcard this
/// machine's address on its default route. Connecting a UDP socket sends nothing.
fn lan_ips(lan_bind: &str) -> Vec<String> {
    let Ok(bind) = lan_bind.parse::<std::net::SocketAddr>() else {
        return Vec::new();
    };
    if !bind.ip().is_unspecified() {
        return vec![bind.ip().to_string()];
    }
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("192.0.2.1:9")?;
            socket.local_addr()
        })
        .ok()
        .map(|address| address.ip())
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        .map(|ip| vec![ip.to_string()])
        .unwrap_or_default()
}
pub async fn register_client(state: &State, app: &apps::App) -> Result<()> {
    sqlx::query("INSERT INTO clients(id,secret,resource,redirect) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET redirect=excluded.redirect")
        .bind(format!("component-{}",app.id)).bind(auth::digest(&app.secret)).bind(&app.id).bind(format!("{}/_loki/callback",apps::public_url(app))).execute(&state.0.db).await?;
    Ok(())
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
    info["lan_bind"] = json!(runtime.lan_bind);
    info["lan_ips"] = json!(runtime.lan_ips);
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
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Components are busy; try again shortly.",
        )
    })??;
    let local = tokio::time::timeout(
        Duration::from_secs(10),
        runtime.slots.clone().acquire_owned(),
    )
    .await
    .map_err(|_| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This component is busy; try again shortly.",
        )
    })??;
    if !runtime.active.load(Ordering::SeqCst) {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The component is stopped.",
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
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The component is not running.",
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
