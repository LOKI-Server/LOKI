mod app_storage;
pub mod apps;
pub mod auth;
pub mod caddy;
pub mod components;
pub mod db;
pub mod files;
pub mod identity;
pub mod platform;
pub mod startup;
pub mod storage;
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
pub struct Error(pub StatusCode, pub String);
impl Error {
    pub fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
    pub fn forbidden() -> Self {
        Self(
            StatusCode::FORBIDDEN,
            "You do not have access to this resource.".into(),
        )
    }
    pub fn unauthorized() -> Self {
        Self(StatusCode::UNAUTHORIZED, "Please sign in.".into())
    }
    pub fn not_found() -> Self {
        Self(StatusCode::NOT_FOUND, "Not found.".into())
    }
}
impl<E: Into<anyhow::Error>> From<E> for Error {
    fn from(value: E) -> Self {
        tracing::error!(error = %value.into(), "operation failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The operation failed. See the local server log for details.".into(),
        )
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub name: String,
    pub public_url: String,
    pub registration: String,
    pub caddy_mode: String,
    pub caddy_binary: String,
    pub caddy_admin: String,
    pub caddy_base: String,
    pub caddy_enabled: bool,
    pub startup: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            name: "LOKI".into(),
            public_url: "http://127.0.0.1:7070".into(),
            registration: "approval".into(),
            caddy_mode: "managed".into(),
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
        }
    }
}

#[derive(Clone)]
pub struct State(pub Arc<Inner>);
pub struct Inner {
    pub db: SqlitePool,
    pub data: PathBuf,
    pub bind: std::net::SocketAddr,
    pub settings: RwLock<Settings>,
    pub key: RsaPrivateKey,
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
    pub startup: startup::Manager,
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
            "apps", "app-data", "backups", "staging", "avatars", "logs", "caddy",
        ] {
            std::fs::create_dir_all(data.join(p))?;
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
            let key =
                tokio::task::spawn_blocking(|| RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048))
                    .await??;
            platform::write_private(&key_path, key.to_pkcs8_pem(LineEnding::LF)?.as_bytes())?;
            key
        };
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
            setup_token: Mutex::new(setup_token),
            operations: Mutex::new(()),
            runtimes: Mutex::new(HashMap::new()),
            caddy: Mutex::new(None),
            passwords: Semaphore::new(2),
            limits: Mutex::new(HashMap::new()),
            shutdown: tokio::sync::Notify::new(),
            maintenance: std::sync::atomic::AtomicBool::new(false),
            requests: Semaphore::new(1024),
            wasm_engine: tokio::sync::OnceCell::new(),
            component_slots: Arc::new(Semaphore::new(8)),
            startup,
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
    pub async fn save_settings(&self, settings: Settings) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO settings(id,json) VALUES(1,?)")
            .bind(serde_json::to_string(&settings)?)
            .execute(&self.0.db)
            .await?;
        *self.0.settings.write().await = settings;
        Ok(())
    }
}
