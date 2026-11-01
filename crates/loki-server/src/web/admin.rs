//! Actions only the owner can take: people, groups, access, shares, routes, apps,
//! and server settings.
use super::{account::Proof, actions::ok};
use crate::{Error, Result, Settings, State, apps, auth, caddy, files, platform};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

#[derive(Deserialize)]
pub(super) struct NewUser {
    username: String,
}
pub(super) async fn user_create(state: &State, input: NewUser) -> Result<Value> {
    let username = &input.username;
    auth::validate_identity(username, "", username)?;
    let uid = auth::id();
    sqlx::query("INSERT INTO users(id,username,display_name,password,status,created) VALUES(?,?,?,'','approved',?)").bind(&uid).bind(username).bind(username).bind(auth::now()).execute(&state.0.db).await?;
    sqlx::query("INSERT INTO memberships(user_id,group_id) VALUES(?,'members')")
        .bind(&uid)
        .execute(&state.0.db)
        .await?;
    issue_reset(state, &uid, true).await
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum UserStatus {
    Pending,
    Approved,
    Rejected,
    Suspended,
}
impl UserStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Suspended => "suspended",
        }
    }
}
#[derive(Deserialize)]
pub(super) struct StatusChange {
    status: UserStatus,
}
pub(super) async fn user_status(state: &State, id: &str, input: StatusChange) -> Result<Value> {
    let mut tx = state.0.db.begin().await?;
    let n =
        sqlx::query("UPDATE users SET status=?,auth_version=auth_version+1 WHERE id=? AND owner=0")
            .bind(input.status.as_str())
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    if n == 0 {
        return Err(Error::bad("The owner cannot be suspended or removed."));
    }
    if input.status == UserStatus::Approved {
        sqlx::query("INSERT OR IGNORE INTO memberships(user_id,group_id) VALUES(?,'members')")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    for table in [
        "sessions",
        "tokens",
        "codes",
        "gateway_sessions",
        "sync_devices",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE user_id=?"))
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(ok())
}

/// A reset link for an account, or an activation link for one the owner created.
pub(super) async fn issue_reset(state: &State, id: &str, activation: bool) -> Result<Value> {
    let exists: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE id=? AND status IN('approved','pending')",
    )
    .bind(id)
    .fetch_one(&state.0.db)
    .await?;
    if exists == 0 {
        return Err(Error::bad("This account cannot receive a reset link."));
    }
    let raw = auth::random();
    let mut tx = state.0.db.begin().await?;
    sqlx::query("DELETE FROM resets WHERE user_id=?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO resets(id,user_id,expires,activation) VALUES(?,?,?,?)")
        .bind(auth::digest(&raw))
        .bind(id)
        .bind(auth::now() + 86400)
        .bind(activation)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({"url":format!("{}/reset#{}",state.0.settings.read().await.public_url,raw)}))
}

#[derive(Deserialize)]
pub(super) struct NewGroup {
    name: String,
}
pub(super) async fn group_create(state: &State, input: NewGroup) -> Result<Value> {
    let name = input.name.trim();
    if name.is_empty() || name.len() > 80 {
        return Err(Error::bad("Enter a group name of at most 80 characters."));
    }
    sqlx::query("INSERT INTO groups(id,name) VALUES(?,?)")
        .bind(auth::id())
        .bind(name)
        .execute(&state.0.db)
        .await?;
    Ok(ok())
}

pub(super) async fn group_delete(state: &State, id: &str) -> Result<Value> {
    if id == "members" {
        return Err(Error::bad("The default Members group cannot be removed."));
    }
    sqlx::query("DELETE FROM grants WHERE subject_kind='group' AND subject=?")
        .bind(id)
        .execute(&state.0.db)
        .await?;
    sqlx::query("DELETE FROM groups WHERE id=?")
        .bind(id)
        .execute(&state.0.db)
        .await?;
    Ok(ok())
}

#[derive(Deserialize)]
pub(super) struct Membership {
    group: String,
    #[serde(default)]
    enabled: bool,
}
/// Adds the person `id` to a group, or removes them.
pub(super) async fn membership(state: &State, id: &str, input: Membership) -> Result<Value> {
    let sql = if input.enabled {
        "INSERT OR IGNORE INTO memberships(user_id,group_id) VALUES(?,?)"
    } else {
        "DELETE FROM memberships WHERE user_id=? AND group_id=?"
    };
    sqlx::query(sql)
        .bind(id)
        .bind(&input.group)
        .execute(&state.0.db)
        .await?;
    Ok(ok())
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Visibility {
    Public,
    Members,
    Restricted,
}
impl Visibility {
    fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Members => "members",
            Self::Restricted => "restricted",
        }
    }
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SubjectKind {
    User,
    Group,
}
#[derive(Deserialize)]
struct Grant {
    subject_kind: SubjectKind,
    subject: String,
    role: String,
}
#[derive(Deserialize)]
pub(super) struct Access {
    visibility: Visibility,
    grants: Vec<Grant>,
}
pub(super) async fn resource_access(state: &State, id: &str, input: Access) -> Result<Value> {
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM resources WHERE id=?")
        .bind(id)
        .fetch_optional(&state.0.db)
        .await?;
    if kind.as_deref() == Some("sync") && input.visibility == Visibility::Public {
        return Err(Error::bad(
            "Sync requires a LOKI account. Choose Members or Restricted.",
        ));
    }
    if input.grants.len() > 1000 {
        return Err(Error::bad("Too many grants."));
    }
    let mut tx = state.0.db.begin().await?;
    sqlx::query("UPDATE resources SET visibility=? WHERE id=?")
        .bind(input.visibility.as_str())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM grants WHERE resource=?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for grant in &input.grants {
        if !apps::valid_id(&grant.role) {
            return Err(Error::bad("Invalid grant."));
        }
        let (kind, table) = match grant.subject_kind {
            SubjectKind::User => ("user", "users"),
            SubjectKind::Group => ("group", "groups"),
        };
        let exists: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE id=?"))
            .bind(&grant.subject)
            .fetch_one(&mut *tx)
            .await?;
        if exists == 0 {
            return Err(Error::bad("Unknown grant subject."));
        }
        sqlx::query(
            "INSERT OR IGNORE INTO grants(resource,subject,subject_kind,role) VALUES(?,?,?,?)",
        )
        .bind(id)
        .bind(&grant.subject)
        .bind(kind)
        .bind(&grant.role)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(ok())
}

#[derive(Deserialize)]
pub(super) struct Share {
    root: String,
    name: String,
    #[serde(default)]
    excluded_extensions: String,
}
/// Creates a share when `id` is empty, otherwise changes it.
pub(super) async fn share_save(state: &State, id: &str, input: Share) -> Result<Value> {
    let excluded = serde_json::to_string(&files::parse_excluded_extensions(
        &input.excluded_extensions,
    )?)?;
    let root = files::canonical(&input.root).await?;
    if !root.is_dir() {
        return Err(Error::bad("Choose a readable directory."));
    }
    let _ = std::fs::read_dir(&root)?;
    let name = &input.name;
    if name.is_empty() || name.len() > 100 {
        return Err(Error::bad("Enter a name of at most 100 characters."));
    }
    let rid = if id.is_empty() {
        auth::id()
    } else {
        id.to_string()
    };
    let mut tx = state.0.db.begin().await?;
    if id.is_empty() {
        sqlx::query("INSERT INTO resources(id,kind,name) VALUES(?,'share',?)")
            .bind(&rid)
            .bind(name)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO shares(id,root,excluded_extensions) VALUES(?,?,?)")
            .bind(&rid)
            .bind(root.to_string_lossy().as_ref())
            .bind(&excluded)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query("UPDATE resources SET name=? WHERE id=? AND kind='share'")
            .bind(name)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE shares SET root=?,excluded_extensions=? WHERE id=?")
            .bind(root.to_string_lossy().as_ref())
            .bind(&excluded)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(json!({"ok":true,"id":rid}))
}

/// Removes a share. Apps and routes have their own removal controls.
pub(super) async fn resource_delete(state: &State, id: &str) -> Result<Value> {
    let _guard = state.0.operations.lock().await;
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM resources WHERE id=?")
        .bind(id)
        .fetch_optional(&state.0.db)
        .await?;
    if kind.as_deref() != Some("share") {
        return Err(Error::bad("Use the app or route removal controls."));
    }
    sqlx::query("DELETE FROM resources WHERE id=?")
        .bind(id)
        .execute(&state.0.db)
        .await?;
    Ok(ok())
}

async fn unique_host(state: &State, id: &str, host: &str) -> Result<()> {
    caddy::hostname(host)?;
    let public = state.0.settings.read().await.public_url.clone();
    if url::Url::parse(&public)?.host_str() == Some(host) {
        return Err(Error::bad("This hostname belongs to LOKI."));
    }
    let count:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM routes WHERE hostname=? AND id!=?)+(SELECT count(*) FROM apps WHERE hostname=? AND id!=?)").bind(host).bind(id).bind(host).bind(id).fetch_one(&state.0.db).await?;
    if count > 0 {
        return Err(Error::bad("This hostname is already in use."));
    }
    Ok(())
}

/// The OS service manager a route's service runs under.
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ServiceKind {
    #[default]
    None,
    Windows,
    Systemd,
    Brew,
}
impl ServiceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Windows => "windows",
            Self::Systemd => "systemd",
            Self::Brew => "brew",
        }
    }
}
#[derive(Deserialize)]
pub(super) struct Route {
    name: String,
    hostname: String,
    upstream: String,
    #[serde(default)]
    gated: bool,
    #[serde(default)]
    service: String,
    #[serde(default)]
    service_kind: ServiceKind,
}
/// Creates a route when `id` is empty, otherwise changes it.
pub(super) async fn route_save(state: &State, id: &str, input: Route) -> Result<Value> {
    let _guard = state.0.operations.lock().await;
    let host = input.hostname.to_ascii_lowercase();
    unique_host(state, id, &host).await?;
    caddy::upstream(&input.upstream)?;
    if input.name.is_empty() || input.name.len() > 100 {
        return Err(Error::bad("Enter a route name of at most 100 characters."));
    }
    let rid = if id.is_empty() { auth::id() } else { id.into() };
    // Route edits are applied while disabled; an explicit Enable applies the candidate.
    let active: Option<bool> = sqlx::query_scalar("SELECT enabled FROM routes WHERE id=?")
        .bind(&rid)
        .fetch_optional(&state.0.db)
        .await?;
    if active == Some(true) {
        return Err(Error::bad("Disable this route before editing it."));
    }
    let mut tx = state.0.db.begin().await?;
    sqlx::query("INSERT INTO resources(id,kind,name) VALUES(?,'route',?) ON CONFLICT(id) DO UPDATE SET name=excluded.name").bind(&rid).bind(&input.name).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO routes(id,hostname,upstream,gated,service,service_kind) VALUES(?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET hostname=excluded.hostname,upstream=excluded.upstream,gated=excluded.gated,service=excluded.service,service_kind=excluded.service_kind").bind(&rid).bind(&host).bind(&input.upstream).bind(input.gated).bind(&input.service).bind(input.service_kind.as_str()).execute(&mut *tx).await?;
    let secret = auth::random();
    sqlx::query("INSERT INTO clients(id,secret,resource,redirect) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET redirect=excluded.redirect").bind(format!("gateway-{rid}")).bind(auth::digest(&secret)).bind(&rid).bind(format!("https://{}/_loki/callback",input.hostname)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(json!({"ok":true,"id":rid}))
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum RouteOperation {
    Enable,
    Disable,
    Delete,
}
#[derive(Deserialize)]
pub(super) struct Operation<T> {
    pub(super) operation: T,
}
pub(super) async fn route_action(
    state: &State,
    id: &str,
    input: Operation<RouteOperation>,
) -> Result<Value> {
    let operation = input.operation;
    let _guard = state.0.operations.lock().await;
    let old: bool = sqlx::query_scalar("SELECT enabled FROM routes WHERE id=?")
        .bind(id)
        .fetch_one(&state.0.db)
        .await?;
    sqlx::query("UPDATE routes SET enabled=? WHERE id=?")
        .bind(operation == RouteOperation::Enable)
        .bind(id)
        .execute(&state.0.db)
        .await?;
    if let Err(e) = caddy::apply(state).await {
        sqlx::query("UPDATE routes SET enabled=? WHERE id=?")
            .bind(old)
            .bind(id)
            .execute(&state.0.db)
            .await?;
        return Err(e);
    }
    if operation == RouteOperation::Delete {
        sqlx::query("DELETE FROM resources WHERE id=?")
            .bind(id)
            .execute(&state.0.db)
            .await?;
    }
    Ok(ok())
}

/// Starts, stops or restarts the OS service behind a route.
pub(super) async fn service(state: &State, id: &str, input: Operation<String>) -> Result<Value> {
    let row = sqlx::query("SELECT service,service_kind FROM routes WHERE id=?")
        .bind(id)
        .fetch_one(&state.0.db)
        .await?;
    let message = platform::service(
        row.get("service_kind"),
        row.get("service"),
        &input.operation,
    )
    .await?;
    Ok(json!({"ok":true,"message":message}))
}

/// Checks a realtime app's LAN listener address, such as `0.0.0.0:4767`. Empty turns it off.
async fn lan_bind(state: &State, id: &str, value: &str) -> Result<String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    let address: std::net::SocketAddr = value
        .parse()
        .map_err(|_| Error::bad("Enter the LAN address as IP:port, for example 0.0.0.0:4767."))?;
    if address.port() == 0 || address.port() == state.0.bind.port() {
        return Err(Error::bad("Choose a port other than LOKI's own."));
    }
    let taken: i64 = sqlx::query_scalar("SELECT count(*) FROM apps WHERE lan_bind=? AND id!=?")
        .bind(address.to_string())
        .bind(id)
        .fetch_one(&state.0.db)
        .await?;
    if taken > 0 {
        return Err(Error::bad("Another app already uses this LAN address."));
    }
    Ok(address.to_string())
}

#[derive(Deserialize)]
pub(super) struct AppConfig {
    hostname: String,
    #[serde(default)]
    lan_bind: String,
    #[serde(default)]
    config: Value,
}
/// Saves an app's hostname, LAN address and settings, rolling back if it won't start.
pub(super) async fn app_config(state: &State, id: &str, input: AppConfig) -> Result<Value> {
    let _guard = state.0.operations.lock().await;
    let app = apps::get(state, id).await?;
    let manifest: apps::Manifest = serde_json::from_str(&app.manifest)?;
    let host = input.hostname.to_ascii_lowercase();
    if !host.is_empty() {
        unique_host(state, id, &host).await?;
    }
    let lan_bind = if manifest.realtime().is_some() {
        lan_bind(state, id, input.lan_bind.trim()).await?
    } else {
        String::new()
    };
    let mut config = input.config;
    let old_config: Value = serde_json::from_str(&app.config)?;
    for f in &manifest.fields {
        if f.kind == "secret" && config[&f.key] == "********" {
            config[&f.key] = old_config[&f.key].clone();
        }
    }
    apps::validate_config(&manifest, &config)?;
    let serialized = serde_json::to_string(&config)?;
    let url = if host.is_empty() {
        format!("http://127.0.0.1:{}", app.port)
    } else {
        format!("https://{host}")
    };
    sqlx::query("UPDATE apps SET hostname=?,config=?,lan_bind=? WHERE id=?")
        .bind(&host)
        .bind(serialized)
        .bind(&lan_bind)
        .bind(id)
        .execute(&state.0.db)
        .await?;
    sqlx::query("UPDATE clients SET redirect=? WHERE id=?")
        .bind(format!("{url}/auth/callback"))
        .bind(id)
        .execute(&state.0.db)
        .await?;
    let result = async {
        if manifest.runtime == "wasi" {
            crate::components::register_client(state, &apps::get(state, id).await?).await?;
        }
        if app.enabled {
            apps::start_inner(state, id).await?;
        }
        caddy::apply(state).await
    }
    .await;
    if let Err(e) = result {
        sqlx::query("UPDATE apps SET hostname=?,config=?,lan_bind=? WHERE id=?")
            .bind(&app.hostname)
            .bind(&app.config)
            .bind(&app.lan_bind)
            .bind(id)
            .execute(&state.0.db)
            .await?;
        sqlx::query("UPDATE clients SET redirect=? WHERE id=?")
            .bind(format!("{}/auth/callback", apps::public_url(&app)))
            .bind(id)
            .execute(&state.0.db)
            .await?;
        if app.enabled {
            let _ = apps::start_inner(state, id).await;
        } else if manifest.runtime == "wasi" {
            let _ = crate::components::register_client(state, &app).await;
        }
        let _ = caddy::apply(state).await;
        return Err(e);
    }
    Ok(ok())
}

#[derive(Deserialize)]
pub(super) struct SettingsChange {
    #[serde(flatten)]
    settings: Settings,
    /// Agrees to replace another LOKI copy's sign-in startup entry.
    #[cfg_attr(not(windows), allow(dead_code))]
    startup_confirmation: Option<String>,
}
pub(super) async fn settings(state: &State, input: SettingsChange) -> Result<Value> {
    let _guard = state.0.operations.lock().await;
    let mut settings = input.settings;
    settings.public_url = auth::validate_public_url(&settings.public_url)?;
    if settings.name.is_empty()
        || settings.name.len() > 80
        || !(1..=100_000_000).contains(&settings.sync_quota_mb)
        || !(1..=1_048_576).contains(&settings.sync_max_file_mb)
    {
        return Err(Error::bad("Invalid settings."));
    }
    caddy::admin_origin(&settings)?;
    let old = state.0.settings.read().await.clone();
    if old.caddy_enabled
        && settings.caddy_enabled
        && (old.caddy_mode != settings.caddy_mode
            || old.caddy_binary != settings.caddy_binary
            || old.caddy_admin != settings.caddy_admin)
    {
        return Err(Error::bad(
            "Disable Caddy management before changing its mode, executable, or admin address.",
        ));
    }
    if old.public_url != settings.public_url {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM clients")
            .fetch_one(&state.0.db)
            .await?;
        if count > 0 {
            return Err(Error::bad(
                "Set the public URL before installing apps or gated routes. Changing an active issuer requires a fresh identity setup.",
            ));
        }
    }
    #[cfg(windows)]
    let startup_registration = if settings.startup || old.startup {
        match state.0.startup.change(
            settings.startup,
            &state.0.data,
            state.0.bind,
            input.startup_confirmation.as_deref(),
        )? {
            crate::startup::Change::Confirm(conflict) => return Ok(conflict.response()),
            crate::startup::Change::Ready(registration) => Some(registration),
        }
    } else {
        None
    };
    caddy::apply_settings(state, &settings).await?;
    #[cfg(not(windows))]
    if settings.startup != old.startup {
        platform::startup(settings.startup, &state.0.data, state.0.bind).await?;
    }
    state.save_settings(settings).await?;
    #[cfg(windows)]
    if let Some(registration) = startup_registration {
        registration.commit();
    }
    Ok(ok())
}

#[derive(Deserialize)]
pub(super) struct Relocation {
    #[serde(flatten)]
    proof: Proof,
    target: String,
    backup: Option<String>,
}
/// Moves LOKI's data, or restores a backup, into an empty directory and restarts.
pub(super) async fn storage(state: &State, user: &auth::User, input: Relocation) -> Result<Value> {
    input.proof.check(state, user).await?;
    crate::storage::switch_home(state, &input.target, input.backup.as_deref()).await?;
    Ok(json!({"ok":true,"restarting":true}))
}
