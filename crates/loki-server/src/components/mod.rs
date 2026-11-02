//! Portable WASIp2 HTTP components. No ambient filesystem, sockets, or secrets.
mod host;
mod http;
mod outbound;

use crate::{Error, Result, State, app_storage, apps, auth, realtime};
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use host::database;
pub(crate) use host::{ConnectionPeer, ServeOptions, WitError};
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
        bindings::ProxyPre,
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
    /// Lines for the app's log; see [`component_log`].
    log: tokio::sync::mpsc::Sender<String>,
    /// Idle instances kept for the next request, with `limits.reuse_instances`.
    warm: std::sync::Mutex<Vec<http::Warm>>,
    warm_slots: Arc<Semaphore>,
    pub(crate) realtime_slots: Arc<Semaphore>,
    /// The hosts the app may send requests to, with the `outbound` capability.
    outbound: Option<Arc<outbound::Policy>>,
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
        self.warm.lock().unwrap().clear();
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
    hooks: outbound::Outbound,
    table: ResourceTable,
    limits: StoreLimits,
    runtime: Arc<Runtime>,
    info: Value,
    form: Value,
    pending: Vec<PathBuf>,
    /// The file to answer with instead of the app's response body; see `serve-file`.
    file: http::ServedFile,
    /// Set only in an app's realtime instances.
    instance: Option<Arc<realtime::Instance>>,
}
/// Where an app's file lives. Matches `file-source` in the WIT's `types`, in the same order.
#[allow(dead_code, reason = "only guests create these, through Lift")]
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
impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

/// Instances kept warm across every app, apart from running requests.
pub(crate) const WARM_INSTANCES: usize = 64;

async fn engine(state: &State) -> Result<Engine> {
    let instances = state.0.component_requests + crate::REALTIME_INSTANCES + WARM_INSTANCES;
    let cache = state.0.data.join("cache").join("components");
    let db = state.0.db.clone();
    Ok(state
        .0
        .wasm_engine
        .get_or_try_init(|| async move {
            let engine = tokio::task::spawn_blocking(move || {
                let mut config = wasmtime::Config::new();
                config
                    .wasm_component_model(true)
                    .consume_fuel(true)
                    .max_wasm_stack(512 * 1024);
                let mut pooled = config.clone();
                pooled.allocation_strategy(pool(instances));
                // A pool reserves address space up front; without it, allocate per instance.
                Engine::new(&pooled).or_else(|error| {
                    tracing::warn!(%error, "pooled WASI instances unavailable; allocating on demand");
                    Engine::new(&config)
                })
            })
            .await??;
            let manifests: Vec<String> = sqlx::query_scalar("SELECT manifest FROM apps")
                .fetch_all(&db)
                .await?;
            let keep: Vec<String> = manifests
                .iter()
                .filter_map(|m| serde_json::from_str::<apps::Manifest>(m).ok())
                .filter_map(|m| m.files.get(&m.entry).map(|sha| cache_name(&engine, sha)))
                .collect();
            tokio::task::spawn_blocking(move || prune_cache(&cache, &keep)).await?;
            Ok::<_, Error>(engine)
        })
        .await?
        .clone())
}
/// Pooled instance slots for every component LOKI may run at once. Each slot fits
/// the largest component the manifest limits allow, so no app that ran before fails here.
fn pool(instances: usize) -> wasmtime::InstanceAllocationStrategy {
    let instances = instances as u32;
    let mut pool = wasmtime::PoolingAllocationConfig::default();
    pool.total_component_instances(instances)
        .total_core_instances(instances * 8)
        .total_memories(instances * 2)
        .total_tables(instances * 4)
        .total_stacks(instances)
        .max_core_instances_per_component(64)
        .max_memories_per_component(8)
        .max_tables_per_component(8)
        .max_memory_size(256 * 1024 * 1024)
        .table_elements(100_000)
        .max_unused_warm_slots(instances / 4);
    wasmtime::InstanceAllocationStrategy::Pooling(pool)
}
/// The cache file for a component with this SHA-256 compiled by this engine.
fn cache_name(engine: &Engine, sha: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    format!("{sha}-{:016x}.cwasm", hasher.finish())
}
/// Removes compiled components no installed app uses, or another engine made.
fn prune_cache(cache: &std::path::Path, keep: &[String]) {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
/// Loads the component compiled earlier, or compiles and caches it. Compiling takes
/// seconds for a large app; loading the cache takes milliseconds.
fn load_component(
    engine: &Engine,
    path: &std::path::Path,
    cache: &std::path::Path,
    sha: &str,
) -> anyhow::Result<Component> {
    let cached = cache.join(cache_name(engine, sha));
    if cached.is_file() {
        // SAFETY: LOKI wrote this file with this engine; a mismatch is refused, not run.
        match unsafe { Component::deserialize_file(engine, &cached) } {
            Ok(component) => return Ok(component),
            Err(error) => tracing::warn!(%error, "recompiling a cached component"),
        }
    }
    let compiled = engine.precompile_component(&std::fs::read(path)?)?;
    std::fs::create_dir_all(cache)?;
    let temp = cache.join(format!(".pending-{}", crate::auth::id()));
    std::fs::write(&temp, &compiled)?;
    std::fs::rename(&temp, &cached).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    // SAFETY: as above; the bytes were just produced by this engine.
    Ok(unsafe { Component::deserialize(engine, &compiled)? })
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
    let cache = state.0.data.join("cache").join("components");
    let sha = manifest.files[&manifest.entry].clone();
    let component =
        tokio::task::spawn_blocking(move || load_component(&compiler, &path, &cache, &sha))
            .await??;
    let mut linker = Linker::<Host>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    host::link(&mut linker)?;
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
        // The owner's quota for the app, or the one it asked for.
        let quota = app
            .storage_mib
            .map_or(manifest.limits.storage_mib, |mib| mib as u64)
            * 1024
            * 1024;
        let entries = manifest.limits.storage_entries;
        Some(
            tokio::task::spawn_blocking(move || {
                app_storage::Storage::open_with(&path, quota, entries)
            })
            .await??,
        )
    } else {
        None
    };
    let (log, lines) = tokio::sync::mpsc::channel(LOG_QUEUE);
    tokio::spawn(log_writer(
        state
            .0
            .data
            .join("logs")
            .join(format!("{}.stderr.log", manifest.id)),
        lines,
    ));
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
        log,
        warm: std::sync::Mutex::new(Vec::new()),
        warm_slots: state.0.warm_instances.clone(),
        realtime_slots: state.0.realtime_instances.clone(),
        outbound: manifest
            .capabilities
            .iter()
            .any(|c| c == "outbound")
            .then(|| {
                Arc::new(outbound::Policy {
                    hosts: manifest.outbound.clone(),
                    in_flight: Arc::new(Semaphore::new(outbound::IN_FLIGHT)),
                })
            }),
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
        permit,
    )
    .await?;
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
    for schedule in &manifest.schedules {
        tokio::spawn(scheduled(
            state.clone(),
            Arc::downgrade(&runtime),
            schedule.clone(),
        ));
    }
    Ok(runtime)
}
/// Runs one of the app's scheduled jobs every `every_minutes` until the app stops,
/// as `POST /_loki/schedule/NAME` without a user. A run that is still going when the
/// next is due makes that one skip.
async fn scheduled(
    state: State,
    runtime: std::sync::Weak<Runtime>,
    schedule: loki_sdk::manifest::Schedule,
) {
    let every = Duration::from_secs(u64::from(schedule.every_minutes) * 60);
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let Some(runtime) = runtime.upgrade() else {
            return;
        };
        if !runtime.active.load(Ordering::SeqCst) {
            return;
        }
        if let Err(error) = scheduled_run(&state, runtime.clone(), &schedule.name).await {
            component_log(
                &runtime,
                &format!("Scheduled job {} failed: {}", schedule.name, error.message),
            );
        }
    }
}
/// Runs one of an app's scheduled jobs now.
pub async fn run_schedule(state: &State, app: &str, name: &str) -> Result<()> {
    let runtime = running(state, app).await?;
    if !runtime.manifest.schedules.iter().any(|s| s.name == name) {
        return Err(Error::not_found());
    }
    scheduled_run(state, runtime, name).await
}
async fn scheduled_run(state: &State, runtime: Arc<Runtime>, name: &str) -> Result<()> {
    let permit = enter(state, &runtime).await?;
    let request = Request::builder()
        .method(axum::http::Method::POST)
        .uri(format!("/_loki/schedule/{name}"))
        .header(header::HOST, url::Url::parse(&runtime.origin)?.authority())
        .body(Body::empty())?;
    let info = json!({"user":null,"csrf":"","issuer":state.0.settings.read().await.public_url});
    let response = execute(state, runtime, request, info, Value::Null, Vec::new(), permit).await?;
    let status = response.status();
    // Read the whole answer, so the job runs to its end before the next can start.
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await;
    if !status.is_success() {
        return Err(Error::new(status, format!("The app answered {status}.")));
    }
    Ok(())
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
/// Adds what every request's context says about the app itself.
pub(crate) fn with_runtime(runtime: &Runtime, mut info: Value) -> Value {
    info["config"] = runtime.config.clone();
    info["origin"] = json!(runtime.origin);
    info["app_id"] = json!(runtime.manifest.id);
    info["lan_bind"] = json!(runtime.lan_bind);
    info["lan_ips"] = json!(runtime.lan_ips);
    info
}
/// A store for one component instance, with the app's memory limits and no ambient access.
pub(crate) fn new_store(
    runtime: &Arc<Runtime>,
    info: Value,
    form: Value,
    pending: Vec<PathBuf>,
    instance: Option<Arc<realtime::Instance>>,
) -> Result<Store<Host>> {
    let info = with_runtime(runtime, info);
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
            hooks: outbound::Outbound::new(runtime.outbound.clone()),
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
            file: http::ServedFile::default(),
            instance,
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
/// A place among the components running across LOKI, and among the app's own.
pub(crate) type Permit = (
    tokio::sync::OwnedSemaphorePermit,
    tokio::sync::OwnedSemaphorePermit,
);
async fn enter(state: &State, runtime: &Runtime) -> Result<Permit> {
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
/// Queues a line for the app's log. Lines are dropped, never waited for, when the
/// writer falls behind.
fn component_log(runtime: &Runtime, message: &str) {
    let line = format!(
        "{} {}
",
        auth::now(),
        message.chars().take(2000).collect::<String>()
    );
    let _ = runtime.log.try_send(line);
}
/// Lines queued for an app's log before new ones are dropped.
const LOG_QUEUE: usize = 1024;
/// Writes an app's log lines in batches until the runtime is gone. The file starts
/// afresh past 1 MiB.
async fn log_writer(path: PathBuf, mut lines: tokio::sync::mpsc::Receiver<String>) {
    use tokio::io::AsyncWriteExt;
    let open = |path: PathBuf| async move {
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .ok()
    };
    let mut size = tokio::fs::metadata(&path).await.map_or(0, |m| m.len());
    let mut file = None;
    while let Some(line) = lines.recv().await {
        let mut batch = line;
        while batch.len() < 64 * 1024
            && let Ok(line) = lines.try_recv()
        {
            batch.push_str(&line);
        }
        if size > 1024 * 1024 {
            file = None;
            let _ = tokio::fs::remove_file(&path).await;
            size = 0;
        }
        if file.is_none() {
            file = open(path.clone()).await;
        }
        if let Some(f) = file.as_mut() {
            if f.write_all(batch.as_bytes()).await.is_err() || f.flush().await.is_err() {
                file = None;
            }
            size += batch.len() as u64;
        }
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
    let permit = enter(state, &runtime).await?;
    let info = json!({"user":null,"csrf":"","issuer":state.0.settings.read().await.public_url});
    execute(state, runtime, request, info, Value::Null, Vec::new(), permit).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_instance_pool_fits_the_largest_app_limits() {
        let mut config = wasmtime::Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .allocation_strategy(pool(64 + crate::REALTIME_INSTANCES + WARM_INSTANCES));
        Engine::new(&config).expect("the pooled engine could not start");
    }
    #[test]
    fn compiled_components_are_kept_only_while_an_app_uses_them() {
        let engine = Engine::default();
        let directory = tempfile::tempdir().unwrap();
        let (used, stale) = (cache_name(&engine, "aa"), cache_name(&engine, "bb"));
        assert_ne!(used, stale);
        assert!(used.starts_with("aa-") && used.ends_with(".cwasm"));
        for name in [&used, &stale, &"aa-0000000000000000.cwasm".to_string()] {
            std::fs::write(directory.path().join(name), b"x").unwrap();
        }
        prune_cache(directory.path(), std::slice::from_ref(&used));
        let left: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left, vec![used]);
    }
}
