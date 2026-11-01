use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    path::{Component, Path},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub protocol: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub target: String,
    pub entry: String,
    pub health: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub fields: Vec<Field>,
    pub files: BTreeMap<String, String>,
    /// Always `wasi`. Packages from before LOKI 0.3 may say `native`, which no
    /// longer runs.
    #[serde(default = "native_runtime")]
    pub runtime: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub limits: Limits,
    /// The app describes a panel that LOKI draws in its dashboard (`/_loki/panel`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub panel: bool,
    /// False for apps with no pages of their own, such as a game server managed
    /// entirely from its panel. The dashboard then offers no link to the app.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub site: bool,
    /// What built the package. LOKI checks the SDK version to tell whether it can
    /// run the app. Packages from before LOKI 0.3 have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_with: Option<BuiltWith>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltWith {
    /// The `loki-sdk` version the app was compiled against.
    pub sdk: String,
    /// The `cargo-loki` version that packaged it.
    pub cargo_loki: String,
}
fn yes() -> bool {
    true
}
fn is_yes(value: &bool) -> bool {
    *value
}
impl Manifest {
    /// The realtime settings of an app that declares the `realtime` capability.
    pub fn realtime(&self) -> Option<Realtime> {
        self.capabilities
            .iter()
            .any(|c| c == "realtime")
            .then(|| self.limits.realtime.clone().unwrap_or_default())
    }
}
fn native_runtime() -> String {
    "native".into()
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub key: String,
    pub label: String,
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub memory_mib: usize,
    pub fuel: u64,
    pub timeout_seconds: u64,
    pub concurrent_requests: usize,
    pub request_bytes: usize,
    /// Present only for apps with the `realtime` capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realtime: Option<Realtime>,
}
/// Limits for an app's long-lived realtime instance and its WebSocket endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Realtime {
    /// The path on the app's origin that accepts WebSocket connections.
    pub path: String,
    pub max_connections: usize,
    pub max_message_bytes: usize,
    /// How often `on-tick` runs, if at all.
    pub tick_ms: Option<u64>,
    /// Fuel for each event the instance handles.
    pub event_fuel: u64,
}
impl Default for Realtime {
    fn default() -> Self {
        Self {
            path: "/multiplayer".into(),
            max_connections: 64,
            max_message_bytes: 64 * 1024,
            tick_ms: None,
            event_fuel: 5_000_000,
        }
    }
}
impl Realtime {
    pub fn valid(&self) -> bool {
        self.path.starts_with('/')
            && !self.path.starts_with("//")
            && !self.path.starts_with("/_loki")
            && self.path != "/logout"
            && self.path.len() <= 200
            && !self.path.contains(['\r', '\n', '?', '#'])
            && (1..=256).contains(&self.max_connections)
            && (1024..=2 * 1024 * 1024).contains(&self.max_message_bytes)
            && self.tick_ms.is_none_or(|t| (20..=1000).contains(&t))
            && (100_000..=100_000_000).contains(&self.event_fuel)
    }
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_mib: 64,
            fuel: 50_000_000,
            timeout_seconds: 30,
            concurrent_requests: 4,
            request_bytes: 16 * 1024 * 1024,
            realtime: None,
        }
    }
}
impl Limits {
    pub fn valid(&self) -> bool {
        (16..=256).contains(&self.memory_mib)
            && (100_000..=10_000_000_000).contains(&self.fuel)
            && (1..=120).contains(&self.timeout_seconds)
            && (1..=8).contains(&self.concurrent_requests)
            && (1024..=1024 * 1024 * 1024).contains(&self.request_bytes)
            && self.realtime.as_ref().is_none_or(Realtime::valid)
    }
}

pub fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\\', ':', '\0'])
        && !value.starts_with('/')
        && value
            .split('/')
            .all(|p| !matches!(p, "" | "." | "..") && !p.ends_with([' ', '.']) && !reserved(p))
        && Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
fn reserved(value: &str) -> bool {
    let name = value.split('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(name.as_str(), "con" | "prn" | "aux" | "nul")
        || ((name.starts_with("com") || name.starts_with("lpt"))
            && name.len() == 4
            && matches!(name.as_bytes()[3], b'1'..=b'9'))
}
pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !value.starts_with('-')
}
/// Checks a manifest. LOKI runs only WASI components (`wasm32-wasip2`).
pub fn validate_manifest(m: &Manifest) -> Result<()> {
    if m.protocol != 1
        || !valid_id(&m.id)
        || m.id.starts_with("loki-")
        || m.name.is_empty()
        || m.name.len() > 100
        || semver::Version::parse(&m.version).is_err()
        || m.runtime != "wasi"
        || m.target != "wasm32-wasip2"
        || !m.entry.ends_with(".wasm")
        || m.capabilities.iter().any(|s| {
            !matches!(
                s.as_str(),
                "database" | "uploads" | "forms" | "storage" | "realtime"
            )
        })
        || !m.limits.valid()
        || !safe_relative(&m.entry)
        || !m.files.contains_key(&m.entry)
        || !m.health.starts_with('/')
        || m.health.starts_with("//")
        || m.health.contains(['\r', '\n', '?', '#'])
        || m.description.len() > 2000
        || m.roles.len() > 32
        || m.fields.len() > 32
        || m.realtime().is_some_and(|r| r.path == m.health)
    {
        return Err(Error::bad(
            "Invalid or incompatible app manifest. LOKI accepts protocol 1 WASI components (wasm32-wasip2).",
        ));
    }
    let mut keys = HashSet::new();
    for field in &m.fields {
        if !valid_id(&field.key)
            || !keys.insert(&field.key)
            || !matches!(field.kind.as_str(), "text" | "secret" | "number" | "bool")
            || field.label.len() > 100
        {
            return Err(Error::bad("Invalid app configuration field."));
        }
    }
    if m.roles.iter().any(|r| !valid_id(r)) {
        return Err(Error::bad("Invalid app role."));
    }
    Ok(())
}
