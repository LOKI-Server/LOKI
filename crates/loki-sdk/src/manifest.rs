use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    path::{Component, Path},
};

/// LOKI refuses fields it doesn't know. A package from a newer SDK, which may
/// have new fields, is refused by its `built_with` version first, with a request
/// to update LOKI.
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
    /// Hosts an app with the `outbound` capability may send HTTP(S) requests to:
    /// `example.com`, `*.example.com` (any subdomain), optionally with `:port`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outbound: Vec<String>,
    /// Jobs LOKI runs on a timer, as `POST /_loki/schedule/NAME` to the app.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<Schedule>,
}
/// A job LOKI runs every `every_minutes` while the app is enabled.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub name: String,
    pub every_minutes: u32,
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
    /// Keep instances between requests, so the app's memory (parsed templates,
    /// caches) survives from one request to the next. Off by default.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reuse_instances: bool,
    /// The app's storage quota in MiB, which the owner can change.
    #[serde(skip_serializing_if = "is_default_storage")]
    pub storage_mib: u64,
    /// How many values and files the app's storage may hold.
    #[serde(skip_serializing_if = "is_default_entries")]
    pub storage_entries: usize,
}
/// Storage quota defaults, as before apps could choose them.
pub const STORAGE_MIB: u64 = 64;
pub const STORAGE_ENTRIES: usize = 4096;
fn is_default_storage(value: &u64) -> bool {
    *value == STORAGE_MIB
}
fn is_default_entries(value: &usize) -> bool {
    *value == STORAGE_ENTRIES
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
    /// Separate instances for rooms, reached at `{path}/{room}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rooms: Option<Rooms>,
}
/// Rooms: one instance each, with its own memory and connections, running in parallel.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Rooms {
    /// Rooms open at once, 1–64.
    pub max: usize,
    /// A room with no connections closes after this many seconds, 5–3600.
    pub idle_seconds: u64,
}
impl Default for Rooms {
    fn default() -> Self {
        Self {
            max: 16,
            idle_seconds: 60,
        }
    }
}
impl Default for Realtime {
    fn default() -> Self {
        Self {
            path: "/multiplayer".into(),
            max_connections: 64,
            max_message_bytes: 64 * 1024,
            tick_ms: None,
            event_fuel: 5_000_000,
            rooms: None,
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
            && self
                .rooms
                .as_ref()
                .is_none_or(|r| (1..=64).contains(&r.max) && (5..=3600).contains(&r.idle_seconds))
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
            reuse_instances: false,
            storage_mib: STORAGE_MIB,
            storage_entries: STORAGE_ENTRIES,
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
            && (1..=4096).contains(&self.storage_mib)
            && (16..=65_536).contains(&self.storage_entries)
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
/// The app services an app can ask LOKI for.
pub const CAPABILITIES: [&str; 6] = [
    "database", "uploads", "forms", "storage", "realtime", "outbound",
];
/// Whether `pattern` is a valid `outbound` entry: a hostname or IP, optionally
/// with a `*.` prefix and a `:port`.
pub fn valid_outbound(pattern: &str) -> bool {
    let (host, port) = match pattern.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, Some(port)),
        _ => (pattern, None),
    };
    let host = host.strip_prefix("*.").unwrap_or(host);
    pattern.len() <= 253
        && port.is_none_or(|p| p.parse::<u16>().is_ok_and(|p| p != 0))
        && !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
        && host.bytes().any(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && !host.bytes().any(|b| b.is_ascii_uppercase())
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
        || m.capabilities
            .iter()
            .any(|s| !CAPABILITIES.contains(&s.as_str()))
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
        || m.outbound.len() > 32
        || !m.outbound.iter().all(|o| valid_outbound(o))
        || (!m.outbound.is_empty() && !m.capabilities.iter().any(|c| c == "outbound"))
        || m.schedules.len() > 16
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
    let mut names = HashSet::new();
    for schedule in &m.schedules {
        if !valid_id(&schedule.name)
            || !names.insert(&schedule.name)
            || !(1..=10_080).contains(&schedule.every_minutes)
        {
            return Err(Error::bad(
                "Invalid schedule: use a unique lowercase name and every_minutes of 1–10080.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn manifest(extra: serde_json::Value) -> Result<Manifest> {
        let mut value = json!({"protocol":1,"id":"game","name":"Game","version":"1.0.0","runtime":"wasi","target":"wasm32-wasip2","entry":"game.wasm","health":"/health","files":{"game.wasm":"00"}});
        for (key, item) in extra.as_object().unwrap() {
            value[key] = item.clone();
        }
        let manifest: Manifest = serde_json::from_value(value)?;
        validate_manifest(&manifest)?;
        Ok(manifest)
    }
    #[test]
    fn manifests_from_before_these_fields_keep_their_meaning() {
        let m = manifest(json!({})).unwrap();
        assert!(m.outbound.is_empty() && m.schedules.is_empty());
        assert!(!m.limits.reuse_instances);
        assert_eq!(m.limits.storage_mib, STORAGE_MIB);
        assert_eq!(m.limits.storage_entries, STORAGE_ENTRIES);
        // Defaults stay out of the manifest a newer SDK writes.
        let written = serde_json::to_value(&m).unwrap();
        for key in ["outbound", "schedules"] {
            assert!(written.get(key).is_none(), "{key}");
        }
        assert_eq!(written["limits"].as_object().unwrap().len(), 5);
    }
    #[test]
    fn outbound_hosts_need_the_capability_and_valid_patterns() {
        let ok = json!({"capabilities":["outbound"],"outbound":["api.example.com","*.discord.com","127.0.0.1:8080"]});
        assert!(manifest(ok).is_ok());
        assert!(manifest(json!({"outbound":["api.example.com"]})).is_err());
        for bad in ["", "*", "https://x.com", "x.com/path", "x.com:0", "X.com", "a..b", "-a.com", "a.com:99999"] {
            assert!(
                manifest(json!({"capabilities":["outbound"],"outbound":[bad]})).is_err(),
                "{bad}"
            );
        }
    }
    #[test]
    fn schedules_rooms_and_storage_are_bounded() {
        assert!(manifest(json!({"schedules":[{"name":"tick","every_minutes":5}]})).is_ok());
        for bad in [
            json!([{"name":"tick","every_minutes":0}]),
            json!([{"name":"tick","every_minutes":10081}]),
            json!([{"name":"Tick","every_minutes":5}]),
            json!([{"name":"a","every_minutes":5},{"name":"a","every_minutes":6}]),
        ] {
            assert!(manifest(json!({"schedules":bad})).is_err(), "{bad}");
        }
        let rooms = |rooms| json!({"capabilities":["realtime"],"limits":{"realtime":{"rooms":rooms}}});
        assert!(manifest(rooms(json!({"max":4}))).is_ok());
        assert!(manifest(rooms(json!({"max":0}))).is_err());
        assert!(manifest(rooms(json!({"max":65}))).is_err());
        assert!(manifest(rooms(json!({"idle_seconds":1}))).is_err());
        assert!(manifest(json!({"limits":{"storage_mib":4096,"storage_entries":65536}})).is_ok());
        assert!(manifest(json!({"limits":{"storage_mib":4097}})).is_err());
        assert!(manifest(json!({"limits":{"storage_entries":8}})).is_err());
    }
}
