use anyhow::{Context, Result, ensure};
use regex::Regex;
use reqwest::{
    StatusCode,
    blocking::{Client, Response, multipart},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const PASSWORD: &str = "development-owner-123";
pub const MEMBER_PASSWORD: &str = "development-member-123";

pub fn temporary(label: &str) -> Result<PathBuf> {
    let path = crate::root()
        .join("target")
        .join(format!("{label}-{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir_all(&path)?;
    Ok(path)
}
pub fn port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}
pub fn wait_for<T>(seconds: u64, mut check: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = check()? {
            return Ok(value);
        }
        ensure!(
            Instant::now() < deadline,
            "Timed out after {seconds} seconds"
        );
        thread::sleep(Duration::from_millis(100));
    }
}
pub fn capture(pattern: &str, text: &str) -> Result<String> {
    Regex::new(pattern)?
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
        .with_context(|| format!("Missing {pattern} in response"))
}
pub fn csrf(html: &Reply) -> Result<String> {
    capture(r#"name="csrf_token" value="([^"]+)""#, &html.text())
}
pub fn report(name: &str, value: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    fs::write(crate::root().join("target").join(name), &text)?;
    println!("{text}");
    Ok(())
}

pub struct Reply {
    pub status: StatusCode,
    pub body: Vec<u8>,
    pub url: String,
}
impl Reply {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn ok(&self) -> Result<()> {
        ensure!(
            self.status == StatusCode::OK,
            "{} returned {}: {}",
            self.url,
            self.status,
            String::from_utf8_lossy(&self.body[..self.body.len().min(2000)])
        );
        Ok(())
    }
    pub fn json(&self) -> Result<Value> {
        self.ok()?;
        Ok(serde_json::from_slice(&self.body)?)
    }
}
fn reply(response: Response) -> Result<Reply> {
    let status = response.status();
    let url = response.url().to_string();
    let mut body = Vec::new();
    response.take(8 * 1024 * 1024 + 1).read_to_end(&mut body)?;
    ensure!(
        body.len() <= 8 * 1024 * 1024,
        "Buffered response exceeded 8 MiB; use the streaming client"
    );
    Ok(Reply { status, body, url })
}
pub struct Browser {
    pub client: Client,
    pub csrf: String,
}
impl Browser {
    pub fn new(port: u16) -> Result<Self> {
        let mut builder = Client::builder()
            .no_proxy()
            .cookie_store(true)
            .timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(2));
        for app in ["bigshot-voter", "storage-notes", "sdk-smoke"] {
            builder = builder.resolve(
                &format!("{app}.localhost"),
                SocketAddr::from(([127, 0, 0, 1], port)),
            );
        }
        builder = builder.redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let url = attempt.url();
            let host = url.host_str().unwrap_or("");
            if attempt.previous().len() >= 12 {
                attempt.error("Too many redirects")
            } else if url.scheme() != "http"
                || url.port_or_known_default() != Some(port)
                || !(host == "127.0.0.1"
                    || matches!(
                        host,
                        "bigshot-voter.localhost"
                            | "storage-notes.localhost"
                            | "sdk-smoke.localhost"
                    ))
            {
                attempt.error("Smoke-test redirect left the isolated loopback server")
            } else {
                attempt.follow()
            }
        }));
        Ok(Self {
            client: builder.build()?,
            csrf: String::new(),
        })
    }
    pub fn get(&self, url: &str) -> Result<Reply> {
        reply(self.client.get(url).send()?)
    }
    pub fn post(&self, url: &str, data: &Value) -> Result<Reply> {
        reply(
            self.client
                .post(url)
                .header("x-csrf-token", &self.csrf)
                .json(data)
                .send()?,
        )
    }
    pub fn api(&self, url: &str) -> Result<Value> {
        self.get(url)?.json()
    }
    pub fn post_api(&self, url: &str, data: &Value) -> Result<Value> {
        self.post(url, data)?.json()
    }
    pub fn session(&mut self, origin: &str) -> Result<Value> {
        let session = self.api(&format!("{origin}/api/session"))?;
        self.csrf = session["csrf"]
            .as_str()
            .context("Missing session CSRF")?
            .into();
        Ok(session)
    }
    pub fn action(&self, origin: &str, action: &str, id: &str, data: Value) -> Result<Value> {
        self.post_api(
            &format!("{origin}/api/action"),
            &json!({"action":action,"id":id,"data":data}),
        )
    }
    pub fn form(&self, url: &str, fields: &[(&str, &str)]) -> Result<Reply> {
        reply(
            self.client
                .post(url)
                .header("x-csrf-token", &self.csrf)
                .form(fields)
                .send()?,
        )
    }
    pub fn multipart(&self, url: &str, form: multipart::Form) -> Result<Reply> {
        reply(
            self.client
                .post(url)
                .header("x-csrf-token", &self.csrf)
                .multipart(form)
                .send()?,
        )
    }
    pub fn upload(&self, url: &str, field: &str, path: &Path) -> Result<Reply> {
        self.multipart(url, multipart::Form::new().file(field.to_owned(), path)?)
    }
    pub fn login(&mut self, origin: &str, username: &str, password: &str) -> Result<()> {
        self.session(origin)?;
        self.post_api(
            &format!("{origin}/api/login"),
            &json!({"username":username,"password":password}),
        )?;
        self.session(origin)?;
        Ok(())
    }
}

pub struct Process {
    pub child: Child,
}
impl Process {
    pub fn spawn(command: &mut Command, log: &Path, group: bool) -> Result<Self> {
        let output = fs::OpenOptions::new().create(true).append(true).open(log)?;
        command
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output);
        #[cfg(windows)]
        if group {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x00000200); // CREATE_NEW_PROCESS_GROUP for CTRL_BREAK.
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(not(windows))]
        let _ = group;
        Ok(Self {
            child: command
                .spawn()
                .with_context(|| format!("Could not launch {command:?}"))?,
        })
    }
    pub fn running(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }
    pub fn wait(&mut self, seconds: u64) -> Result<()> {
        wait_for(seconds, || Ok(self.child.try_wait()?.map(|_| ())))
    }
    pub fn interrupt(&mut self) -> Result<()> {
        if !self.running()? {
            return Ok(());
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
            ensure!(
                unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.child.id()) } != 0,
                "Could not send Ctrl+Break to preview: {}",
                std::io::Error::last_os_error()
            );
        }
        #[cfg(unix)]
        ensure!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) } == 0,
            "Could not interrupt preview"
        );
        self.wait(20)
    }
    pub fn stop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            #[cfg(unix)]
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            #[cfg(windows)]
            let _ = self.child.kill();
            if self.wait(15).is_err() {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct Server {
    pub base: PathBuf,
    pub data: PathBuf,
    pub origin: String,
    pub port: u16,
    pub process: Option<Process>,
    binary: PathBuf,
    restarted: Vec<PathBuf>,
    retain: bool,
}
impl Server {
    pub fn new(binary: &Path, label: &str) -> Result<Self> {
        let base = temporary(label)?;
        let port = port()?;
        let mut server = Self {
            data: base.join("data"),
            base,
            origin: format!("http://127.0.0.1:{port}"),
            port,
            process: None,
            binary: binary.into(),
            restarted: Vec::new(),
            retain: false,
        };
        server.start()?;
        Ok(server)
    }
    pub fn start(&mut self) -> Result<()> {
        let mut command = Command::new(&self.binary);
        command
            .arg("--data-dir")
            .arg(&self.data)
            .arg("--bind")
            .arg(format!("127.0.0.1:{}", self.port))
            .arg("--no-tray")
            .env("LOKI_NO_TRAY", "true")
            .env("LOKI_LOCATION_FILE", self.base.join("location.json"));
        self.process = Some(Process::spawn(
            &mut command,
            &self.base.join("console.log"),
            false,
        )?);
        self.wait_ready()
    }
    pub fn wait_ready(&mut self) -> Result<()> {
        let client = Browser::new(self.port)?;
        wait_for(30, || {
            if let Some(process) = &mut self.process {
                ensure!(
                    process.running()?,
                    "Server exited; see {}",
                    self.base.join("console.log").display()
                );
            }
            Ok(client
                .get(&format!("{}/health", self.origin))
                .ok()
                .filter(|r| r.status == StatusCode::OK)
                .map(|_| ()))
        })
        .with_context(|| {
            format!(
                "Server startup failed; see {}",
                self.base.join("console.log").display()
            )
        })
    }
    pub fn initialize(&self) -> Result<Browser> {
        let mut owner = Browser::new(self.port)?;
        owner.session(&self.origin)?;
        owner.post_api(&format!("{}/api/setup",self.origin), &json!({"token":fs::read_to_string(self.data.join("setup-token.txt"))?,"username":"owner","password":PASSWORD,"name":"LOKI smoke test","public_url":self.origin,"startup":false}))?;
        owner.login(&self.origin, "owner", PASSWORD)?;
        Ok(owner)
    }
    pub fn install(&self, owner: &Browser, package: &Path, id: &str) -> Result<String> {
        owner
            .upload(
                &format!("{}/api/apps/import", self.origin),
                "package",
                package,
            )?
            .ok()?;
        owner.action(
            &self.origin,
            "app-action",
            id,
            json!({"operation":"enable"}),
        )?;
        self.app_ready(owner, id)
    }
    pub fn app_ready(&self, owner: &Browser, id: &str) -> Result<String> {
        wait_for(45, || {
            let state = owner.api(&format!("{}/api/state", self.origin))?;
            let app = resource(&state, id)?;
            Ok(if app["ready"] == true {
                app["url"].as_str().map(str::to_owned)
            } else {
                None
            })
        })
    }
    pub fn stop(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.stop();
        }
    }
    pub fn restore_offline(&mut self, backup: &str) -> Result<()> {
        let archive = self.data.join("backups").join(backup);
        self.stop();
        self.data = self.base.join("restored");
        crate::run(
            Command::new(&self.binary)
                .arg("--data-dir")
                .arg(&self.data)
                .arg("restore")
                .arg(archive)
                .env("LOKI_LOCATION_FILE", self.base.join("location.json")),
        )?;
        self.start()
    }
    pub fn expect_restart(&mut self, destination: &Path) -> Result<()> {
        ensure!(
            destination.starts_with(&self.base),
            "Restart destination must stay in disposable test data"
        );
        self.restarted.push(destination.into());
        Ok(())
    }
    pub fn finish_restart(&mut self, destination: &Path) -> Result<()> {
        self.process
            .as_mut()
            .context("Missing original server")?
            .wait(40)?;
        self.process.take();
        self.data = destination.into();
        self.wait_ready()
    }
    pub fn pid(&mut self) -> Result<u32> {
        if let Some(process) = &self.process {
            return Ok(process.child.id());
        }
        Ok(fs::read_to_string(self.data.join("server.pid"))?
            .trim()
            .parse()?)
    }
    pub fn keep(&mut self) {
        self.retain = true;
        if let Some(process) = self.process.take() {
            std::mem::forget(process);
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if self.retain {
            return;
        }
        self.stop();
        for dir in &self.restarted {
            if let Ok(text) = fs::read_to_string(dir.join("server.pid"))
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                terminate(pid);
            }
        }
    }
}
pub struct PidFileGuard(pub PathBuf);
impl Drop for PidFileGuard {
    fn drop(&mut self) {
        if let Ok(value) = fs::read_to_string(&self.0)
            && let Ok(pid) = value.trim().parse()
        {
            terminate(pid);
        }
    }
}
fn terminate(pid: u32) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                OpenProcess, PROCESS_TERMINATE, SYNCHRONIZATION_SYNCHRONIZE, TerminateProcess,
                WaitForSingleObject,
            },
        };
        let handle = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZATION_SYNCHRONIZE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 0);
            WaitForSingleObject(handle, 15000);
            CloseHandle(handle);
        }
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}
pub fn resource<'a>(state: &'a Value, id: &str) -> Result<&'a Value> {
    state["resources"]
        .as_array()
        .and_then(|r| r.iter().find(|r| r["id"] == id))
        .context("Missing app/share resource")
}
pub fn package_manifest(path: &Path) -> Result<Value> {
    let mut archive = zip::ZipArchive::new(fs::File::open(path)?)?;
    Ok(serde_json::from_reader(
        archive.by_name("manifest.json")?.take(131073),
    )?)
}
pub fn broken_package(
    source: &Path,
    destination: &Path,
    change: impl FnOnce(&mut Value),
) -> Result<()> {
    let mut input = zip::ZipArchive::new(fs::File::open(source)?)?;
    let mut manifest: Value = serde_json::from_reader(input.by_name("manifest.json")?)?;
    change(&mut manifest);
    let mut output = zip::ZipWriter::new(fs::File::create(destination)?);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    output.start_file("manifest.json", options)?;
    output.write_all(&serde_json::to_vec(&manifest)?)?;
    for i in 0..input.len() {
        let mut entry = input.by_index(i)?;
        if entry.name() == "manifest.json" {
            continue;
        }
        output.start_file(entry.name(), options)?;
        std::io::copy(&mut entry, &mut output)?;
    }
    output.finish()?;
    Ok(())
}
pub fn sql_pair(path: &Path, query: &str, param: &str) -> Result<(String, String)> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(path)
                        .read_only(true),
                )
                .await?;
            let result = sqlx::query_as(query).bind(param).fetch_one(&pool).await;
            pool.close().await;
            Ok(result?)
        })
}
pub fn votes(path: &Path) -> Result<i64> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(path)
                        .read_only(true),
                )
                .await?;
            let result = sqlx::query_scalar("SELECT count(*) FROM votes")
                .fetch_one(&pool)
                .await;
            pool.close().await;
            Ok(result?)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn html_capture_reports_missing_fields() {
        assert_eq!(
            capture(
                r#"name="entry_id" value="([^"]+)""#,
                r#"<input name="entry_id" value="abc">"#
            )
            .unwrap(),
            "abc"
        );
        assert!(capture("(absent)", "body").is_err());
    }
}
