//! Ownership-aware Windows sign-in startup registration.
use crate::{Error, Result, auth};
use serde::Serialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    command: String,
    expandable: bool,
}
trait Store: Send + Sync {
    fn read(&self) -> Result<Option<Entry>>;
    fn write(&self, entry: Option<&Entry>) -> Result<()>;
}

#[derive(Serialize)]
pub struct Conflict {
    pub existing_executable: String,
    pub current_executable: String,
    pub existing_command: String,
    pub replacement_command: String,
    pub data_directory: String,
    pub confirmation: String,
}
impl Conflict {
    pub fn response(&self) -> serde_json::Value {
        serde_json::json!({"requires_confirmation":"startup","startup":self})
    }
}
#[derive(Serialize)]
pub struct Status {
    pub current_executable: String,
    pub registered_executable: Option<String>,
    pub needs_confirmation: bool,
}
pub enum Change {
    Ready(Registration),
    Confirm(Conflict),
}
/// Restore the old entry on an error, provided no other writer replaced ours.
pub struct Registration {
    store: Arc<dyn Store>,
    previous: Option<Entry>,
    replacement: Option<Entry>,
    committed: bool,
}
impl Registration {
    pub fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        if !self.committed && self.previous != self.replacement {
            match self.store.read() {
                Ok(current) if current == self.replacement => {
                    if let Err(error) = self.store.write(self.previous.as_ref()) {
                        tracing::error!(error=%error.1,"Could not roll back startup registration");
                    }
                }
                _ => {}
            }
        }
    }
}

pub struct Manager {
    executable: PathBuf,
    store: Arc<dyn Store>,
}
impl Manager {
    pub fn system() -> anyhow::Result<Self> {
        Ok(Self {
            executable: std::env::current_exe()?,
            #[cfg(windows)]
            store: Arc::new(registry::RegistryStore::system()),
            #[cfg(not(windows))]
            store: Arc::new(NoopStore),
        })
    }
    pub fn status(&self) -> Result<Status> {
        let entry = self.store.read()?;
        Ok(Status {
            current_executable: self.executable.to_string_lossy().into_owned(),
            registered_executable: entry
                .as_ref()
                .map(|entry| executable(&entry.command).unwrap_or_else(|| entry.command.clone())),
            needs_confirmation: entry.as_ref().is_some_and(|entry| !self.owns(entry)),
        })
    }
    fn owns(&self, entry: &Entry) -> bool {
        executable(&entry.command)
            .is_some_and(|exe| normalized(Path::new(&exe)) == normalized(&self.executable))
    }
    pub fn change(
        &self,
        enable: bool,
        data: &Path,
        bind: SocketAddr,
        confirmation: Option<&str>,
    ) -> Result<Change> {
        let previous = self.store.read()?;
        let command = format!(
            "{} --data-dir {} --bind {bind}",
            quote(&self.executable.to_string_lossy()),
            quote(&data.to_string_lossy())
        );
        if enable {
            // The Windows Run key has a 260-character command-line limit.
            if command.encode_utf16().count() > 260 {
                return Err(Error::bad(
                    "The startup command is too long for Windows. Choose shorter executable/data paths.",
                ));
            }
            if let Some(entry) = &previous
                && !self.owns(entry)
            {
                let proof = auth::digest(&serde_json::to_string(&(
                    &entry.command,
                    entry.expandable,
                    &command,
                ))?);
                if confirmation != Some(proof.as_str()) {
                    return Ok(Change::Confirm(Conflict {
                        existing_executable: executable(&entry.command)
                            .unwrap_or_else(|| entry.command.clone()),
                        current_executable: self.executable.to_string_lossy().into_owned(),
                        existing_command: entry.command.clone(),
                        replacement_command: command,
                        data_directory: data.to_string_lossy().into_owned(),
                        confirmation: proof,
                    }));
                }
            }
        }
        let replacement = if enable {
            Some(Entry {
                command,
                expandable: false,
            })
        } else if previous.as_ref().is_some_and(|entry| !self.owns(entry)) {
            previous.clone()
        } else {
            None
        };
        if previous != replacement {
            self.store.write(replacement.as_ref())?;
        }
        Ok(Change::Ready(Registration {
            store: self.store.clone(),
            previous,
            replacement,
            committed: false,
        }))
    }
}

fn normalized(path: &Path) -> String {
    // Avoid probing a deleted portable executable or an offline/network drive
    // just to display startup status. Equivalent verbatim/case variants match.
    let path = path.to_string_lossy().replace('/', "\\");
    let path = if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        path.strip_prefix("\\\\?\\").unwrap_or(&path).to_owned()
    };
    path.to_lowercase()
}
fn executable(command: &str) -> Option<String> {
    let command = command.trim_start();
    if let Some(quoted) = command.strip_prefix('"') {
        let (exe, rest) = quoted.split_once('"')?;
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return None;
        }
        (!exe.is_empty()).then(|| exe.to_owned())
    } else {
        command.split_whitespace().next().map(str::to_owned)
    }
}
fn quote(value: &str) -> String {
    let mut result = String::from("\"");
    let mut backslashes = 0;
    for c in value.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        result.push_str(&"\\".repeat(if c == '"' {
            backslashes * 2 + 1
        } else {
            backslashes
        }));
        result.push(c);
        backslashes = 0;
    }
    result.push_str(&"\\".repeat(backslashes * 2));
    result.push('"');
    result
}

#[cfg(not(windows))]
struct NoopStore;
#[cfg(not(windows))]
impl Store for NoopStore {
    fn read(&self) -> Result<Option<Entry>> {
        Ok(None)
    }
    fn write(&self, _: Option<&Entry>) -> Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
mod registry {
    use super::*;
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS},
        System::Registry::*,
    };

    pub(super) struct RegistryStore {
        pub(super) subkey: String,
    }
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    fn check(status: u32) -> Result<()> {
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(status as i32).into());
        }
        Ok(())
    }
    impl RegistryStore {
        pub(super) fn system() -> Self {
            Self {
                subkey: r"Software\Microsoft\Windows\CurrentVersion\Run".into(),
            }
        }
    }
    impl Store for RegistryStore {
        fn read(&self) -> Result<Option<Entry>> {
            let subkey = wide(&self.subkey);
            let name = wide("LOKI");
            let mut size = 0;
            let mut kind = 0;
            let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    name.as_ptr(),
                    flags,
                    &mut kind,
                    std::ptr::null_mut(),
                    &mut size,
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            check(status)?;
            for _ in 0..3 {
                if size > 65536 || size % 2 != 0 {
                    return Err(Error::bad("Invalid LOKI startup entry."));
                }
                let mut buffer = vec![0u16; (size as usize / 2).max(1)];
                let status = unsafe {
                    RegGetValueW(
                        HKEY_CURRENT_USER,
                        subkey.as_ptr(),
                        name.as_ptr(),
                        flags,
                        &mut kind,
                        buffer.as_mut_ptr().cast(),
                        &mut size,
                    )
                };
                if status == ERROR_FILE_NOT_FOUND {
                    return Ok(None);
                }
                if status == ERROR_MORE_DATA {
                    continue;
                }
                check(status)?;
                buffer.truncate(size as usize / 2);
                while buffer.last() == Some(&0) {
                    buffer.pop();
                }
                let command = String::from_utf16(&buffer)
                    .map_err(|_| Error::bad("Invalid LOKI startup entry."))?;
                if command.contains('\0') {
                    return Err(Error::bad("Invalid LOKI startup entry."));
                }
                return Ok(Some(Entry {
                    command,
                    expandable: kind == REG_EXPAND_SZ,
                }));
            }
            Err(Error::bad(
                "The LOKI startup entry kept changing. Try again.",
            ))
        }
        fn write(&self, entry: Option<&Entry>) -> Result<()> {
            let subkey = wide(&self.subkey);
            let name = wide("LOKI");
            let mut key = std::ptr::null_mut();
            let status = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_SET_VALUE,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                )
            };
            check(status)?;
            let status = if let Some(entry) = entry {
                let bytes = wide(&entry.command);
                unsafe {
                    RegSetValueExW(
                        key,
                        name.as_ptr(),
                        0,
                        if entry.expandable {
                            REG_EXPAND_SZ
                        } else {
                            REG_SZ
                        },
                        bytes.as_ptr().cast(),
                        (bytes.len() * 2) as u32,
                    )
                }
            } else {
                unsafe { RegDeleteValueW(key, name.as_ptr()) }
            };
            unsafe {
                RegCloseKey(key);
            }
            if entry.is_none() && status == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            check(status)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct Memory {
        entry: Mutex<Option<Entry>>,
        writes: AtomicUsize,
    }
    impl Store for Memory {
        fn read(&self) -> Result<Option<Entry>> {
            Ok(self.entry.lock().unwrap().clone())
        }
        fn write(&self, entry: Option<&Entry>) -> Result<()> {
            *self.entry.lock().unwrap() = entry.cloned();
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    fn entry(exe: &str) -> Entry {
        Entry {
            command: format!(
                "{} --data-dir \"F:\\Old data\" --bind 127.0.0.1:7070",
                quote(exe)
            ),
            expandable: false,
        }
    }
    fn manager() -> (Manager, Arc<Memory>) {
        let memory = Arc::new(Memory::default());
        (
            Manager {
                executable: r"C:\Apps\LOKI\loki-server.exe".into(),
                store: memory.clone(),
            },
            memory,
        )
    }
    fn proof(change: Change) -> Conflict {
        match change {
            Change::Confirm(value) => value,
            _ => panic!("Expected confirmation"),
        }
    }
    fn ready(change: Change) -> Registration {
        match change {
            Change::Ready(value) => value,
            _ => panic!("Unexpected confirmation"),
        }
    }
    fn address() -> SocketAddr {
        "127.0.0.1:7171".parse().unwrap()
    }

    #[test]
    fn migration_requires_confirmation_bound_to_the_old_entry_and_new_configuration() {
        let (manager, memory) = manager();
        let original = entry(r"F:\Portable LOKI\loki-server.exe");
        *memory.entry.lock().unwrap() = Some(original.clone());
        let data = Path::new(r"F:\Servers\LOKI");
        let conflict = proof(manager.change(true, data, address(), None).unwrap());
        assert!(manager.status().unwrap().needs_confirmation);
        assert_eq!(memory.read().unwrap(), Some(original.clone()));
        assert_eq!(memory.writes.load(Ordering::SeqCst), 0);
        assert!(
            conflict
                .replacement_command
                .contains(r#"--data-dir "F:\Servers\LOKI" --bind 127.0.0.1:7171"#)
        );
        proof(
            manager
                .change(true, data, address(), Some("wrong"))
                .unwrap(),
        );
        // Consent for one data location cannot silently enable another.
        proof(
            manager
                .change(
                    true,
                    Path::new(r"F:\Different"),
                    address(),
                    Some(&conflict.confirmation),
                )
                .unwrap(),
        );
        let changed = entry(r"F:\Another portable\loki-server.exe");
        *memory.entry.lock().unwrap() = Some(changed.clone());
        let next = proof(
            manager
                .change(true, data, address(), Some(&conflict.confirmation))
                .unwrap(),
        );
        assert_ne!(next.confirmation, conflict.confirmation);
        assert_eq!(memory.read().unwrap(), Some(changed));
        ready(
            manager
                .change(true, data, address(), Some(&next.confirmation))
                .unwrap(),
        )
        .commit();
        let saved = memory.read().unwrap().unwrap();
        assert_eq!(saved.command, next.replacement_command);
        assert!(!manager.status().unwrap().needs_confirmation);
    }
    #[test]
    fn own_entries_update_without_prompt_and_disabling_preserves_other_copies() {
        let (manager, memory) = manager();
        *memory.entry.lock().unwrap() = Some(entry(r"\\?\c:\APPS\LOKI\loki-server.exe"));
        ready(
            manager
                .change(true, Path::new(r"F:\New data"), address(), None)
                .unwrap(),
        )
        .commit();
        assert!(memory.read().unwrap().unwrap().command.contains("New data"));
        ready(
            manager
                .change(false, Path::new(r"F:\New data"), address(), None)
                .unwrap(),
        )
        .commit();
        assert!(memory.read().unwrap().is_none());
        let other = entry(r"F:\Portable\loki-server.exe");
        *memory.entry.lock().unwrap() = Some(other.clone());
        let before = memory.writes.load(Ordering::SeqCst);
        ready(
            manager
                .change(false, Path::new(r"F:\New data"), address(), None)
                .unwrap(),
        )
        .commit();
        assert_eq!(memory.read().unwrap(), Some(other));
        assert_eq!(memory.writes.load(Ordering::SeqCst), before);
    }
    #[test]
    fn failed_changes_restore_registration_but_do_not_overwrite_another_writer() {
        let (manager, memory) = manager();
        let mut original = entry(r"F:\Portable\loki-server.exe");
        original.expandable = true;
        *memory.entry.lock().unwrap() = Some(original.clone());
        let data = Path::new(r"F:\Servers\LOKI");
        let conflict = proof(manager.change(true, data, address(), None).unwrap());
        drop(ready(
            manager
                .change(true, data, address(), Some(&conflict.confirmation))
                .unwrap(),
        ));
        assert_eq!(memory.read().unwrap(), Some(original));
        let guard = ready(
            manager
                .change(true, data, address(), Some(&conflict.confirmation))
                .unwrap(),
        );
        let changed = entry(r"F:\Elsewhere\loki-server.exe");
        *memory.entry.lock().unwrap() = Some(changed.clone());
        drop(guard);
        assert_eq!(memory.read().unwrap(), Some(changed));
    }
    #[test]
    fn command_quoting_and_run_key_bounds() {
        assert_eq!(quote(r"F:\"), r#""F:\\""#);
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(
            executable(r#""C:\Program Files\LOKI\loki-server.exe" --bind 127.0.0.1:7070"#)
                .as_deref(),
            Some(r"C:\Program Files\LOKI\loki-server.exe")
        );
        assert!(executable(r#""C:\LOKI.exe"bad"#).is_none());
        let (manager, memory) = manager();
        assert!(
            manager
                .change(true, Path::new(&"x".repeat(300)), address(), None)
                .is_err()
        );
        assert_eq!(memory.writes.load(Ordering::SeqCst), 0);
    }

    #[cfg(windows)]
    #[test]
    fn native_registry_round_trip_uses_an_isolated_non_startup_key() {
        use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let path: Vec<u16> = self.0.encode_utf16().chain(Some(0)).collect();
                unsafe {
                    RegDeleteTreeW(HKEY_CURRENT_USER, path.as_ptr());
                }
            }
        }
        let path = format!(r"Software\LOKI\Tests\Startup-{}", auth::id());
        let _cleanup = Cleanup(path.clone());
        let store = Arc::new(registry::RegistryStore { subkey: path });
        assert!(store.read().unwrap().is_none());
        let mut original = entry(r"F:\Deleted portable\loki-server.exe");
        original.expandable = true;
        store.write(Some(&original)).unwrap();
        assert_eq!(store.read().unwrap(), Some(original.clone()));
        let manager = Manager {
            executable: r"C:\Apps\LOKI\loki-server.exe".into(),
            store: store.clone(),
        };
        let conflict = proof(
            manager
                .change(true, Path::new(r"F:\Servers\LOKI"), address(), None)
                .unwrap(),
        );
        assert_eq!(store.read().unwrap(), Some(original));
        ready(
            manager
                .change(
                    true,
                    Path::new(r"F:\Servers\LOKI"),
                    address(),
                    Some(&conflict.confirmation),
                )
                .unwrap(),
        )
        .commit();
        assert_eq!(
            store.read().unwrap().unwrap().command,
            conflict.replacement_command
        );
        ready(
            manager
                .change(false, Path::new(r"F:\Servers\LOKI"), address(), None)
                .unwrap(),
        )
        .commit();
        assert!(store.read().unwrap().is_none());
    }

    #[cfg(windows)]
    mod api {
        use super::*;
        use axum::{
            Router,
            body::Body,
            http::{Request, StatusCode, header},
        };
        use http_body_util::BodyExt;
        use serde_json::{Value, json};
        use tower::ServiceExt;
        struct Browser {
            app: Router,
            cookie: String,
            csrf: String,
        }
        impl Browser {
            async fn request(&mut self, path: &str, body: Option<Value>) -> (StatusCode, Value) {
                let mut request = Request::builder()
                    .method(if body.is_some() { "POST" } else { "GET" })
                    .uri(path)
                    .header(header::HOST, "127.0.0.1:7070")
                    .header(header::COOKIE, &self.cookie);
                let body = if let Some(body) = body {
                    request = request
                        .header("x-csrf-token", &self.csrf)
                        .header(header::CONTENT_TYPE, "application/json");
                    Body::from(body.to_string())
                } else {
                    Body::empty()
                };
                let response = self
                    .app
                    .clone()
                    .oneshot(request.body(body).unwrap())
                    .await
                    .unwrap();
                let status = response.status();
                if let Some(cookie) = response.headers().get(header::SET_COOKIE) {
                    self.cookie = cookie.to_str().unwrap().split(';').next().unwrap().into();
                }
                let value: Value = serde_json::from_slice(
                    &response.into_body().collect().await.unwrap().to_bytes(),
                )
                .unwrap();
                if let Some(csrf) = value["csrf"].as_str() {
                    self.csrf = csrf.into();
                }
                (status, value)
            }
            async fn settings(&mut self, data: Value) -> (StatusCode, Value) {
                self.request(
                    "/api/action",
                    Some(json!({"action":"settings","id":"","data":data})),
                )
                .await
            }
        }
        async fn installation() -> (tempfile::TempDir, crate::State, Arc<Memory>, Browser) {
            let temp = tempfile::tempdir().unwrap();
            let (manager, memory) = manager();
            *memory.entry.lock().unwrap() = Some(entry(r"F:\Portable\loki-server.exe"));
            let state = crate::State::open_with_startup(
                temp.path().into(),
                "127.0.0.1:7070".parse().unwrap(),
                manager,
            )
            .await
            .unwrap();
            let mut browser = Browser {
                app: crate::web::router(state.clone()),
                cookie: String::new(),
                csrf: String::new(),
            };
            browser.request("/api/session", None).await;
            (temp, state, memory, browser)
        }
        fn setup_input(state: &crate::State, token: String) -> Value {
            json!({"token":token,"username":"owner","password":"owner-password-123","name":"Original","public_url":format!("http://{}",state.0.bind),"startup":true})
        }
        #[tokio::test]
        async fn setup_conflict_does_not_create_owner_or_consume_token_before_confirmation() {
            let (_temp, state, memory, mut browser) = installation().await;
            let token = state.0.setup_token.lock().await.clone().unwrap();
            let mut input = setup_input(&state, token.clone());
            let (status, result) = browser.request("/api/setup", Some(input.clone())).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(result["requires_confirmation"], "startup");
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users")
                    .fetch_one(&state.0.db)
                    .await
                    .unwrap(),
                0
            );
            assert_eq!(
                state.0.setup_token.lock().await.as_deref(),
                Some(token.as_str())
            );
            assert_eq!(memory.writes.load(Ordering::SeqCst), 0);
            input["startup_confirmation"] = result["startup"]["confirmation"].clone();
            let (status, result) = browser.request("/api/setup", Some(input)).await;
            assert_eq!(status, StatusCode::OK, "{result}");
            assert_eq!(result["ok"], true);
            assert!(state.0.setup_token.lock().await.is_none());
            assert!(!state.0.startup.status().unwrap().needs_confirmation);
            state.0.db.close().await;
        }
        #[tokio::test]
        async fn restored_enabled_settings_still_prompt_and_failed_saves_roll_back_startup() {
            let (_temp, state, memory, mut browser) = installation().await;
            let token = state.0.setup_token.lock().await.clone().unwrap();
            let mut input = setup_input(&state, token);
            input["startup"] = json!(false);
            assert_eq!(
                browser.request("/api/setup", Some(input)).await.0,
                StatusCode::OK
            );
            assert_eq!(
                browser
                    .request(
                        "/api/login",
                        Some(json!({"username":"owner","password":"owner-password-123"}))
                    )
                    .await
                    .0,
                StatusCode::OK
            );
            browser.request("/api/session", None).await;
            let mut saved = state.0.settings.read().await.clone();
            saved.startup = true;
            state.save_settings(saved.clone()).await.unwrap();
            let original = memory.read().unwrap();
            let state_json = browser.request("/api/state", None).await.1;
            assert_eq!(state_json["startup"]["needs_confirmation"], true);
            let mut proposed = serde_json::to_value(&saved).unwrap();
            proposed["name"] = json!("Installed copy");
            let (status, result) = browser.settings(proposed.clone()).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(result["requires_confirmation"], "startup");
            assert_eq!(state.0.settings.read().await.name, "Original");
            assert_eq!(memory.read().unwrap(), original);
            // Invalid confirmation cannot replace a changed entry.
            proposed["startup_confirmation"] = json!("not-approved");
            assert_eq!(
                browser.settings(proposed.clone()).await.1["requires_confirmation"],
                "startup"
            );
            proposed["startup_confirmation"] = result["startup"]["confirmation"].clone();
            let (status, result) = browser.settings(proposed).await;
            assert_eq!(status, StatusCode::OK, "{result}");
            assert_eq!(state.0.settings.read().await.name, "Installed copy");
            assert!(!state.0.startup.status().unwrap().needs_confirmation);
            let before = memory.read().unwrap();
            let mut bad = serde_json::to_value(state.0.settings.read().await.clone()).unwrap();
            bad["startup"] = json!(false);
            bad["caddy_enabled"] = json!(true);
            bad["caddy_binary"] = json!("__missing_loki_test_caddy__");
            assert!(!browser.settings(bad).await.0.is_success());
            assert_eq!(memory.read().unwrap(), before);
            assert!(state.0.settings.read().await.startup);
            state.0.db.close().await;
        }
    }
}
