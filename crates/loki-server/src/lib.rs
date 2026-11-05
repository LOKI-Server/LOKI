mod app_storage;
pub mod apps;
pub mod assets;
pub mod auth;
pub mod caddy;
pub mod components;
pub mod db;
pub mod device_codes;
pub mod devices;
pub mod files;
pub mod identity;
pub mod platform;
pub mod realtime;
pub mod registry;
pub mod startup;
pub mod storage;
pub mod sync;
pub mod web;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rsa::RsaPrivateKey;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::{Mutex, RwLock, Semaphore};

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub struct Error {
    pub status: StatusCode,
    /// Shown to people, so it says what to do rather than what broke inside.
    pub message: String,
    /// A stable machine-readable code for API clients, when they need one.
    pub code: Option<&'static str>,
}
impl Error {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
        }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "You do not have access to this resource.",
        )
    }
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Please sign in.")
    }
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Not found.")
    }
    pub fn coded(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            ..Self::new(status, message)
        }
    }
}
impl<E: Into<anyhow::Error>> From<E> for Error {
    fn from(value: E) -> Self {
        tracing::error!(error = %value.into(), "operation failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The operation failed. See the local server log for details.",
        )
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let body = match self.code {
            Some(code) => serde_json::json!({"error": self.message, "code": code}),
            None => serde_json::json!({"error": self.message}),
        };
        (self.status, Json(body)).into_response()
    }
}

/// Who may create an account from the sign-in page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Registration {
    /// Accounts are approved as soon as they are created.
    Open,
    /// Accounts wait for the owner's approval.
    #[default]
    Approval,
    /// Only the owner creates accounts.
    Closed,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaddyMode {
    /// LOKI starts and stops its own Caddy.
    #[default]
    Managed,
    /// LOKI loads its routes into a Caddy that is already running.
    Existing,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub name: String,
    pub public_url: String,
    pub registration: Registration,
    pub caddy_mode: CaddyMode,
    pub caddy_binary: String,
    pub caddy_admin: String,
    pub caddy_base: String,
    pub caddy_enabled: bool,
    pub startup: bool,
    pub sync_quota_mb: u64,
    pub sync_max_file_mb: u64,
    /// Where the app registry's `index.json` lives. Empty turns the registry off.
    pub registry_url: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            name: "LOKI".into(),
            public_url: "http://127.0.0.1:7070".into(),
            registration: Registration::default(),
            caddy_mode: CaddyMode::default(),
            caddy_binary: std::env::current_exe()
                .ok()
                .and_then(|p| {
                    p.parent()
                        .map(|p| p.join(if cfg!(windows) { "caddy.exe" } else { "caddy" }))
                })
                .filter(|p| p.is_file())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "caddy".into()),
            caddy_admin: "http://127.0.0.1:2019".into(),
            caddy_base: String::new(),
            caddy_enabled: false,
            startup: false,
            sync_quota_mb: 10240,
            sync_max_file_mb: 2048,
            registry_url: registry::DEFAULT_URL.into(),
        }
    }
}

/// Argon2 hashes at once; each one uses about 19 MiB.
const PASSWORD_HASHES: usize = 2;
/// Requests LOKI handles at once. Storage operations drain all but their own.
pub(crate) const REQUESTS: u32 = 1024;
/// WASI component requests running at once across every app: two per CPU core,
/// within 8–64.
fn component_requests() -> usize {
    std::thread::available_parallelism()
        .map_or(8, |n| n.get() * 2)
        .clamp(8, 64)
}
/// Long-lived realtime instances (an app's main instance and its rooms) across every app.
pub(crate) const REALTIME_INSTANCES: usize = 192;
/// Sync long polls waiting at once.
const SYNC_WAITERS: usize = 256;
/// Open realtime WebSocket connections across every app.
const SOCKETS: usize = 512;

#[derive(Clone)]
pub struct State(pub Arc<Inner>);
pub struct Inner {
    pub db: SqlitePool,
    pub data: PathBuf,
    pub bind: std::net::SocketAddr,
    pub settings: RwLock<Settings>,
    pub key: RsaPrivateKey,
    /// Signs identity tokens with `key`; see [`identity::signer`].
    pub signer: ring::signature::RsaKeyPair,
    pub setup_token: Mutex<Option<String>>,
    pub operations: Mutex<()>,
    pub runtimes: Mutex<HashMap<String, apps::Runtime>>,
    pub caddy: Mutex<Option<platform::Process>>,
    pub passwords: Semaphore,
    pub limits: Mutex<HashMap<String, (u32, i64)>>,
    pub shutdown: tokio::sync::Notify,
    pub maintenance: std::sync::atomic::AtomicBool,
    pub requests: Semaphore,
    pub wasm_engine: tokio::sync::OnceCell<wasmtime::Engine>,
    pub component_slots: Arc<Semaphore>,
    /// The size of `component_slots`.
    pub component_requests: usize,
    /// Claimed by each running realtime instance.
    pub realtime_instances: Arc<Semaphore>,
    /// Claimed by each idle instance an app keeps for its next request.
    pub warm_instances: Arc<Semaphore>,
    pub startup: startup::Manager,
    /// Bumped after every sync commit and before maintenance, waking long polls.
    pub sync_wake: tokio::sync::watch::Sender<u64>,
    pub sync_waiters: Semaphore,
    /// Open realtime WebSocket connections across every app.
    pub sockets: Arc<Semaphore>,
    /// Shared by blob uploads and commits; exclusive for garbage collection.
    pub sync_blobs: RwLock<()>,
    /// The last app registry index LOKI read; see [`registry`].
    pub registry: Mutex<Option<Arc<registry::Cache>>>,
}
impl State {
    pub async fn open(data: PathBuf, bind: std::net::SocketAddr) -> anyhow::Result<Self> {
        Self::open_with_startup(data, bind, startup::Manager::system()?).await
    }
    pub(crate) async fn open_with_startup(
        data: PathBuf,
        bind: std::net::SocketAddr,
        startup: startup::Manager,
    ) -> anyhow::Result<Self> {
        for p in [
            "apps",
            "app-data",
            "backups",
            "staging",
            "avatars",
            "logs",
            "caddy",
            "sync/blobs",
            "sync/staging",
        ] {
            std::fs::create_dir_all(data.join(p))?;
        }
        // Interrupted uploads never became blobs.
        for entry in std::fs::read_dir(data.join("sync/staging"))?.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
        platform::private_directory(&data)?;
        let db = db::open(&data.join("loki.db")).await?;
        let previous: Vec<String> =
            sqlx::query_scalar("SELECT id FROM apps WHERE previous IS NOT NULL")
                .fetch_all(&db)
                .await?;
        for id in previous {
            let restored = data.join("rollback-data").join(&id);
            if restored.is_dir() {
                sqlx::query("UPDATE apps SET previous=? WHERE id=?")
                    .bind(restored.to_string_lossy().as_ref())
                    .bind(id)
                    .execute(&db)
                    .await?;
            }
        }
        let settings = db::settings(&db).await?;
        let key_path = data.join("identity.pem");
        use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
        let key = if key_path.exists() {
            RsaPrivateKey::from_pkcs8_pem(&std::fs::read_to_string(&key_path)?)?
        } else {
            let key = tokio::task::spawn_blocking(|| {
                RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
            })
            .await??;
            platform::write_private(&key_path, key.to_pkcs8_pem(LineEnding::LF)?.as_bytes())?;
            key
        };
        let signer = identity::signer(&key)?;
        let setup_token =
            if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE owner=1")
                .fetch_one(&db)
                .await?
                == 0
            {
                Some(auth::random())
            } else {
                None
            };
        let http = Self(Arc::new(Inner {
            db,
            data,
            bind,
            settings: RwLock::new(settings),
            key,
            signer,
            setup_token: Mutex::new(setup_token),
            operations: Mutex::new(()),
            runtimes: Mutex::new(HashMap::new()),
            caddy: Mutex::new(None),
            passwords: Semaphore::new(PASSWORD_HASHES),
            limits: Mutex::new(HashMap::new()),
            shutdown: tokio::sync::Notify::new(),
            maintenance: std::sync::atomic::AtomicBool::new(false),
            requests: Semaphore::new(REQUESTS as usize),
            wasm_engine: tokio::sync::OnceCell::new(),
            component_slots: Arc::new(Semaphore::new(component_requests())),
            component_requests: component_requests(),
            realtime_instances: Arc::new(Semaphore::new(REALTIME_INSTANCES)),
            warm_instances: Arc::new(Semaphore::new(components::WARM_INSTANCES)),
            startup,
            sync_wake: tokio::sync::watch::Sender::new(0),
            sync_waiters: Semaphore::new(SYNC_WAITERS),
            sockets: Arc::new(Semaphore::new(SOCKETS)),
            sync_blobs: RwLock::new(()),
            registry: Mutex::new(None),
        }));
        Ok(http)
    }
    pub async fn audit(&self, actor: &str, action: &str, detail: &str) -> Result<()> {
        sqlx::query("INSERT INTO audit(actor,action,detail,created) VALUES(?,?,?,?)")
            .bind(actor)
            .bind(action)
            .bind(detail)
            .bind(auth::now())
            .execute(&self.0.db)
            .await?;
        Ok(())
    }
    /// Return every sync long poll promptly, e.g. before draining or shutting down.
    pub fn wake_sync(&self) {
        self.0.sync_wake.send_modify(|n| *n = n.wrapping_add(1));
    }
    pub async fn save_settings(&self, settings: Settings) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO settings(id,json) VALUES(1,?)")
            .bind(serde_json::to_string(&settings)?)
            .execute(&self.0.db)
            .await?;
        *self.0.settings.write().await = settings;
        Ok(())
    }
}
