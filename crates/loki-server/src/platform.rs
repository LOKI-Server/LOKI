use crate::{Error, Result};
use std::{path::Path, process::Stdio};
use tokio::process::{Child, Command};

pub fn private_directory(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        let _ = path; /* Inherits the per-user AppData ACL. */
    }
    Ok(())
}

#[derive(Clone)]
pub struct RollingLog(pub std::path::PathBuf);
impl std::io::Write for RollingLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK
            .lock()
            .map_err(|_| std::io::Error::other("log mutex poisoned"))?;
        if std::fs::metadata(&self.0).is_ok_and(|m| m.len() > 2097152) {
            let previous = self.0.with_extension("previous.log");
            let _ = std::fs::remove_file(&previous);
            std::fs::rename(&self.0, previous)?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)?
            .write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
pub struct Process {
    pub child: Child,
    #[cfg(unix)]
    pgid: Option<i32>,
    #[cfg(windows)]
    job: usize,
}
impl Process {
    pub fn spawn(command: &mut Command) -> anyhow::Result<Self> {
        command.kill_on_drop(true).stdin(Stdio::null());
        #[cfg(unix)]
        {
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            command.creation_flags(0x08000000);
        }
        let child = command.spawn()?;
        #[cfg(windows)]
        {
            use windows_sys::Win32::{
                Foundation::CloseHandle,
                System::{JobObjects::*, Threading::*},
            };
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(std::io::Error::last_os_error().into());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&info) as u32,
                ) == 0
                {
                    CloseHandle(job);
                    return Err(std::io::Error::last_os_error().into());
                }
                let process = OpenProcess(
                    PROCESS_SET_QUOTA | PROCESS_TERMINATE,
                    0,
                    child.id().unwrap_or(0),
                );
                if process.is_null() {
                    CloseHandle(job);
                    return Err(std::io::Error::last_os_error().into());
                }
                let success = AssignProcessToJobObject(job, process);
                CloseHandle(process);
                if success == 0 {
                    CloseHandle(job);
                    return Err(std::io::Error::last_os_error().into());
                }
                Ok(Self {
                    child,
                    job: job as usize,
                })
            }
        }
        #[cfg(not(windows))]
        {
            let pgid = child.id().map(|p| p as i32);
            Ok(Self { child, pgid })
        }
    }
    pub async fn stop(&mut self) -> anyhow::Result<()> {
        #[cfg(unix)]
        if let Some(pid) = self.pgid {
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1);
        }
        if tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            #[cfg(unix)]
            if let Some(pid) = self.child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = self.child.kill().await;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pgid.take() {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job as _);
        }
        #[cfg(unix)]
        if let Some(pid) = self.pgid.take() {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("TLS client")
}

pub async fn service(kind: &str, name: &str, action: &str) -> Result<String> {
    if !matches!(action, "start" | "stop" | "restart" | "status")
        || name.is_empty()
        || name.len() > 200
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.@-".contains(&c))
        || name.starts_with('-')
    {
        return Err(Error::bad("Invalid service name or action."));
    }
    let mut cmd = match kind {
        "windows" if cfg!(windows) => {
            let mut c = Command::new("sc.exe");
            c.arg(if action == "status" { "query" } else { action })
                .arg(name);
            c
        }
        "systemd" if cfg!(target_os = "linux") => {
            let mut c = Command::new("systemctl");
            c.arg(action).arg(name);
            c
        }
        "brew" if cfg!(target_os = "macos") => {
            let mut c = Command::new("brew");
            c.arg("services")
                .arg(if action == "status" { "info" } else { action })
                .arg(name);
            c
        }
        _ => {
            return Err(Error::bad(
                "This service adapter is not available on this operating system.",
            ));
        }
    };
    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000);
    }
    if kind == "windows" && action == "restart" {
        let status = service_boxed("windows", name, "status").await?;
        if !status.contains("STOPPED") {
            if !status.contains("STOP_PENDING") {
                service_boxed("windows", name, "stop").await?;
            }
            let mut stopped = false;
            for _ in 0..100 {
                if service_boxed("windows", name, "status")
                    .await?
                    .contains("STOPPED")
                {
                    stopped = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            if !stopped {
                return Err(Error::bad(
                    "The Windows service did not stop in time. Check its status before retrying.",
                ));
            }
        }
        return service_boxed("windows", name, "start").await;
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        cmd.kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| Error::bad("Service command timed out."))??;
    if !output.status.success() {
        return Err(Error::bad(format!(
            "Service command failed: {}",
            format!(
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .chars()
            .take(1000)
            .collect::<String>()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .chars()
        .take(4000)
        .collect())
}
fn service_boxed<'a>(
    kind: &'a str,
    name: &'a str,
    action: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(service(kind, name, action))
}

pub async fn startup(enable: bool, data: &Path, bind: std::net::SocketAddr) -> Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(windows)]
    {
        let mut cmd = Command::new("reg.exe");
        cmd.creation_flags(0x08000000);
        if enable {
            cmd.args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "LOKI",
                "/t",
                "REG_SZ",
                "/d",
            ]);
            cmd.arg(format!(
                "\"{}\" --data-dir \"{}\" --bind {}",
                exe.display(),
                data.display(),
                bind
            ))
            .arg("/f");
        } else {
            cmd.args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "LOKI",
                "/f",
            ]);
        }
        let output = cmd.output().await?;
        if enable && !output.status.success() {
            return Err(Error::bad("Could not register startup."));
        }
    }
    #[cfg(target_os = "linux")]
    {
        let dirs = directories::BaseDirs::new()
            .ok_or_else(|| Error::bad("No user configuration directory."))?;
        let dir = dirs.config_dir().join("autostart");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join("loki.desktop");
        if enable {
            let quote = |s: &Path| {
                format!(
                    "\"{}\"",
                    s.to_string_lossy()
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"")
                        .replace('`', "\\`")
                        .replace('$', "\\$")
                        .replace('%', "%%")
                )
            };
            tokio::fs::write(path,format!("[Desktop Entry]\nType=Application\nName=LOKI\nExec={} --data-dir {} --bind {} --no-tray\nTerminal=false\n",quote(&exe),quote(data),bind)).await?;
        } else if path.exists() {
            tokio::fs::remove_file(path).await?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = directories::BaseDirs::new().ok_or_else(|| Error::bad("No home directory."))?;
        let dir = home.home_dir().join("Library/LaunchAgents");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join("dev.loki.server.plist");
        let escape = |s: String| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        if enable {
            tokio::fs::write(&path,format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Label</key><string>dev.loki.server</string><key>ProgramArguments</key><array><string>{}</string><string>--data-dir</string><string>{}</string><string>--bind</string><string>{}</string><string>--no-tray</string></array><key>RunAtLoad</key><true/></dict></plist>",escape(exe.to_string_lossy().into()),escape(data.to_string_lossy().into()),bind)).await?;
        } else if path.exists() {
            tokio::fs::remove_file(path).await?;
        }
    }
    Ok(())
}
