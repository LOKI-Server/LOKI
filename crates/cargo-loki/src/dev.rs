//! An explicitly isolated preview; never imports into an existing installation.
use crate::{
    Build, package,
    project::{Project, absolute},
};
use anyhow::{Context, Result, bail, ensure};
use rand::RngCore;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

#[derive(Serialize, Deserialize)]
struct Credentials {
    plugin: String,
    username: String,
    password: String,
    #[serde(default)]
    port: Option<u16>,
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
struct Session {
    client: Client,
    origin: String,
    cookie: String,
    csrf: String,
}
impl Session {
    fn request(&mut self, path: &str, data: Option<Value>) -> Result<Value> {
        let url = format!("{}{path}", self.origin);
        let request = if let Some(data) = data {
            self.client
                .post(url)
                .header("x-csrf-token", &self.csrf)
                .json(&data)
        } else {
            self.client.get(url)
        };
        let response = request.header("cookie", &self.cookie).send()?;
        if let Some(cookie) = response
            .headers()
            .get("set-cookie")
            .and_then(|h| h.to_str().ok())
        {
            self.cookie = cookie.split(';').next().unwrap_or("").into();
        }
        let status = response.status();
        let value: Value = response.json()?;
        ensure!(
            status.is_success(),
            "LOKI: {}",
            value["error"].as_str().unwrap_or("request failed")
        );
        if let Some(csrf) = value["csrf"].as_str() {
            self.csrf = csrf.into();
        }
        Ok(value)
    }
    fn install(&mut self, package: &Path, id: &str) -> Result<String> {
        let form = reqwest::blocking::multipart::Form::new().file("package", package)?;
        let response = self
            .client
            .post(format!("{}/api/apps/import", self.origin))
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .multipart(form)
            .send()?;
        let status = response.status();
        let value: Value = response.json()?;
        ensure!(
            status.is_success(),
            "Import failed: {}",
            value["error"].as_str().unwrap_or("unknown error")
        );
        self.request(
            "/api/action",
            Some(json!({"action":"app-action","id":id,"data":{"operation":"enable"}})),
        )?;
        let state = self.request("/api/state", None)?;
        state["resources"]
            .as_array()
            .and_then(|r| r.iter().find(|r| r["id"] == id))
            .and_then(|r| r["url"].as_str())
            .map(str::to_string)
            .context("LOKI did not return the preview URL")
    }
}
pub fn run(build: Build, server: Option<PathBuf>, port: u16, no_watch: bool) -> Result<()> {
    let project = Project::load(&build)?;
    let wasm = project.build(&build)?;
    let archive = package::create(&project, &wasm, None)?;
    let id = project.app_id()?;
    ensure!(loki_sdk::manifest::valid_id(&id), "Invalid plugin ID");
    let server = locate_server(server)?;
    let data = project.target.join("loki/dev").join(&id);
    std::fs::create_dir_all(&data)?;
    let credentials_path = data.join("cargo-loki-dev.json");
    let mut credentials = if credentials_path.exists() {
        let credentials: Credentials = serde_json::from_slice(&std::fs::read(&credentials_path)?)?;
        ensure!(
            credentials.plugin == id,
            "Development directory belongs to another plugin"
        );
        credentials
    } else {
        ensure!(
            std::fs::read_dir(&data)?.next().is_none(),
            "Refusing to use a nonempty installation as a dev profile: {}",
            data.display()
        );
        let mut bytes = [0; 24];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let credentials = Credentials {
            plugin: id.clone(),
            username: "developer".into(),
            password: hex::encode(bytes),
            port: None,
        };
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        options
            .open(&credentials_path)?
            .write_all(&serde_json::to_vec(&credentials)?)?;
        credentials
    };
    let address = if let Some(saved) = credentials.port {
        ensure!(
            port == 0 || port == saved,
            "This development profile uses port {saved}. Reuse that port (or --port 0), or choose a new Cargo target directory for a fresh profile."
        );
        format!("127.0.0.1:{saved}").parse()?
    } else if port == 0 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?
    } else {
        format!("127.0.0.1:{port}").parse()?
    };
    // Keep the identity issuer stable when resuming an automatically assigned port.
    if credentials.port.is_none() {
        credentials.port = Some(address.port());
        std::fs::write(&credentials_path, serde_json::to_vec(&credentials)?)?;
    }
    let stopped = Arc::new(AtomicBool::new(false));
    let interrupt = stopped.clone();
    ctrlc::set_handler(move || interrupt.store(true, Ordering::SeqCst))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(data.join("dev-server.log"))?;
    let child = Command::new(server)
        .args(["--data-dir"])
        .arg(&data)
        .args(["--bind", &address.to_string(), "--no-tray"])
        .env("LOKI_NO_TRAY", "true")
        .env("LOKI_LOCATION_FILE", data.join("location.json"))
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    let mut server = Server(child);
    let origin = format!("http://{address}");
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut ready = false;
    for _ in 0..200 {
        if server.0.try_wait()?.is_some() {
            bail!(
                "Preview server exited; see {}. Check --port for conflicts.",
                data.join("dev-server.log").display()
            );
        }
        if std::fs::read_to_string(data.join("server.pid"))
            .ok()
            .is_some_and(|pid| pid.trim() == server.0.id().to_string())
            && client
                .get(format!("{origin}/health"))
                .send()
                .is_ok_and(|r| r.status().is_success())
        {
            ready = true;
            break;
        }
        if stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    ensure!(
        ready,
        "Preview server did not become ready; see {}",
        data.join("dev-server.log").display()
    );
    let mut session = Session {
        client: Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(120))
            .build()?,
        origin,
        cookie: String::new(),
        csrf: String::new(),
    };
    if session.request("/api/session", None)?["setup"] == true {
        let token = std::fs::read_to_string(data.join("setup-token.txt"))?;
        session.request("/api/setup",Some(json!({"token":token,"username":credentials.username,"password":credentials.password,"name":"LOKI development","public_url":session.origin,"startup":false})))?;
    }
    session.request(
        "/api/login",
        Some(json!({"username":credentials.username,"password":credentials.password})),
    )?;
    session.request("/api/session", None)?;
    let url = session.install(&archive, &id)?;
    println!(
        "Plugin: {url}\nDashboard: {}\nDevelopment sign-in: {} / {}\nProfile: {}\nCtrl+C stops this preview.{}",
        session.origin,
        credentials.username,
        credentials.password,
        data.display(),
        if no_watch {
            ""
        } else {
            " Watching source and assets."
        }
    );
    let mut snapshot = snapshot_files(&project)?;
    while !stopped.load(Ordering::SeqCst) {
        if server.0.try_wait()?.is_some() {
            bail!(
                "The preview server stopped; see {}",
                data.join("dev-server.log").display()
            );
        }
        std::thread::sleep(Duration::from_millis(400));
        if no_watch {
            continue;
        }
        let next = snapshot_files(&project)?;
        if next != snapshot {
            std::thread::sleep(Duration::from_millis(150));
            snapshot = snapshot_files(&project)?;
            let result = (|| {
                let project = Project::load(&build)?;
                let wasm = project.build(&build)?;
                let package = package::create(&project, &wasm, None)?;
                session.install(&package, &id)
            })();
            match result {
                Ok(url) => println!("Updated {url}"),
                Err(error) => eprintln!("Update failed; last working plugin retained: {error:#}"),
            }
        }
    }
    Ok(())
}
fn locate_server(server: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(server) = server {
        let path = absolute(&server)?;
        ensure!(
            path.is_file(),
            "Server executable not found: {}",
            path.display()
        );
        return Ok(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name(if cfg!(windows) {
            "loki-server.exe"
        } else {
            "loki-server"
        });
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    for path in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let server = path.join(if cfg!(windows) {
            "loki-server.exe"
        } else {
            "loki-server"
        });
        if server.is_file() {
            return Ok(server);
        }
    }
    bail!("LOKI server not found. Pass --server PATH or set LOKI_SERVER.")
}
type Snapshot = BTreeMap<PathBuf, (u64, SystemTime)>;
fn snapshot_files(project: &Project) -> Result<Snapshot> {
    fn visit(path: &Path, files: &mut Snapshot) -> Result<()> {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return Ok(());
        };
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Ok(());
            }
        }
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        if metadata.is_file() {
            ensure!(files.len() < 10000, "Too many watched files");
            files.insert(path.into(), (metadata.len(), metadata.modified()?));
        } else if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                visit(&entry?.path(), files)?;
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    for name in ["src", "templates", "Cargo.toml", "Cargo.lock"] {
        visit(&project.root.join(name), &mut files)?;
    }
    if let Some(assets) = project.config()?["assets"].as_array() {
        for path in assets {
            if let Some(path) = path.as_str() {
                ensure!(loki_sdk::manifest::safe_relative(path), "Unsafe watch path");
                visit(&project.root.join(path), &mut files)?;
            }
        }
    }
    Ok(files)
}
