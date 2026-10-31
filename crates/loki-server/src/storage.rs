//! Consistent offline restore and UI-driven storage relocation. Source is retained.
use crate::{Error, Result, State, apps, platform};
use std::path::{Path, PathBuf};

pub fn default_home() -> PathBuf {
    if let Ok(bytes) = std::fs::read(location_file())
        && let Ok(path) = serde_json::from_slice::<PathBuf>(&bytes)
    {
        return path;
    }
    directories::ProjectDirs::from("dev", "LOKI", "LOKI")
        .expect("user data directory")
        .data_local_dir()
        .to_path_buf()
}
fn location_file() -> PathBuf {
    if let Some(path) = std::env::var_os("LOKI_LOCATION_FILE") {
        return PathBuf::from(path);
    }
    directories::ProjectDirs::from("dev", "LOKI", "LOKI")
        .expect("user config directory")
        .config_dir()
        .join("data-location.json")
}
fn validated_archive(backup: &Path) -> anyhow::Result<zip::ZipArchive<std::fs::File>> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(backup)?)?;
    let mut seen = std::collections::HashSet::new();
    let mut total = 0u64;
    for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        let name = file.name();
        if !apps::safe_relative(name)
            || file.is_dir()
            || file.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
            || !seen.insert(name.to_string())
            || !(matches!(name, "loki.db" | "identity.pem")
                || ["apps/", "app-data/", "avatars/", "caddy/", "rollback-data/"]
                    .iter()
                    .any(|p| name.starts_with(p)))
        {
            anyhow::bail!("Unsafe backup entry");
        }
        total = total
            .checked_add(file.size())
            .ok_or_else(|| anyhow::anyhow!("Backup too large"))?;
        if total > 100 * 1024 * 1024 * 1024 {
            anyhow::bail!("Backup exceeds 100 GiB");
        }
    }
    if !seen.contains("loki.db") || !seen.contains("identity.pem") {
        anyhow::bail!("Missing database or identity key");
    }
    Ok(archive)
}
pub fn validate_backup(path: &Path) -> anyhow::Result<()> {
    validated_archive(path).map(|_| ())
}
pub fn restore(backup: &Path, data: &Path) -> anyhow::Result<()> {
    if std::fs::read_dir(data)?
        .filter_map(std::result::Result::ok)
        .any(|e| e.file_name() != "server.lock")
    {
        anyhow::bail!(
            "Restore requires an empty directory; the existing installation is preserved."
        );
    }
    let mut archive = validated_archive(backup)?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let target = data.join(file.name());
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&target)?;
        std::io::copy(&mut file, &mut out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                target,
                std::fs::Permissions::from_mode(file.unix_mode().unwrap_or(0o600) & 0o777),
            )?;
        }
    }
    platform::private_directory(data)?;
    Ok(())
}
pub async fn prepare_destination(state: &State, target: &str) -> Result<PathBuf> {
    let target = PathBuf::from(target);
    if !target.is_absolute() {
        return Err(Error::bad("Choose an absolute server directory."));
    }
    tokio::fs::create_dir_all(&target).await?;
    let target = tokio::fs::canonicalize(target).await?;
    if target.starts_with(&state.0.data)
        || state.0.data.starts_with(&target)
        || std::fs::read_dir(&target)?.next().is_some()
    {
        return Err(Error::bad(
            "Choose an empty folder outside the current data directory.",
        ));
    }
    Ok(target)
}
pub async fn switch_home(state: &State, target: &str, selected_backup: Option<&str>) -> Result<()> {
    let target = prepare_destination(state, target).await?;
    let restore_name = selected_backup.filter(|s| !s.is_empty());
    if restore_name.is_some_and(|s| {
        !apps::safe_relative(s)
            || s.contains('/')
            || !s.starts_with("loki-")
            || !s.ends_with(".zip")
    }) {
        return Err(Error::bad("Select a local LOKI backup."));
    }
    if state
        .0
        .maintenance
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return Err(Error::bad("A storage operation is already running."));
    }
    let pointer_before = std::fs::read(location_file()).ok();
    let mut pointer_changed = false;
    let result = async {
        // The caller owns one request permit. Drain all other in-flight handlers
        // before snapshotting; new API requests are rejected by maintenance mode.
        let _drain = state.0.requests.acquire_many(1023).await?;
        // No further requests may mutate identity state while the snapshot is made.
        let safety = crate::web::snapshot(state, false).await?;
        let archive = state
            .0
            .data
            .join("backups")
            .join(restore_name.unwrap_or(&safety));
        let destination = target.clone();
        tokio::task::spawn_blocking(move || restore(&archive, &destination)).await??;
        if let Some(mut caddy) = state.0.caddy.lock().await.take() {
            caddy.stop().await?;
        }
        let settings = state.0.settings.read().await.clone();
        if settings.startup {
            platform::startup(true, &target, state.0.bind).await?;
        }
        let pointer = location_file();
        if let Some(parent) = pointer.parent() {
            std::fs::create_dir_all(parent)?;
            platform::private_directory(parent)?;
        }
        std::fs::write(&pointer, serde_json::to_vec(&target)?)?;
        pointer_changed = true;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg("--data-dir")
            .arg(&target)
            .arg("--bind")
            .arg(state.0.bind.to_string())
            .arg("--wait-for-pid")
            .arg(std::process::id().to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        command.spawn()?;
        state
            .audit("owner", "storage-restart", &target.to_string_lossy())
            .await?;
        let shutdown = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            shutdown.0.shutdown.notify_one();
        });
        Ok::<_, Error>(())
    }
    .await;
    if result.is_err() {
        state
            .0
            .maintenance
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if pointer_changed {
            match pointer_before {
                Some(bytes) => {
                    let _ = std::fs::write(location_file(), bytes);
                }
                None => {
                    let _ = std::fs::remove_file(location_file());
                }
            }
        }
        if state.0.settings.read().await.startup {
            let _ = platform::startup(true, &state.0.data, state.0.bind).await;
        }
        let apps: Vec<String> = sqlx::query_scalar("SELECT id FROM apps WHERE enabled=1")
            .fetch_all(&state.0.db)
            .await?;
        for id in apps {
            let _ = apps::start_inner(state, &id).await;
        }
        let _ = crate::caddy::apply(state).await;
    }
    result
}
pub async fn wait_for_pid(pid: u32) -> anyhow::Result<()> {
    #[cfg(windows)]
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::*};
        unsafe {
            let process = OpenProcess(0x00100000, 0, pid);
            if !process.is_null() {
                let result = WaitForSingleObject(process, 60000);
                CloseHandle(process);
                if result != 0 {
                    anyhow::bail!("Previous LOKI did not stop");
                }
            }
        }
        Ok(())
    })
    .await??;
    #[cfg(unix)]
    {
        let mut done = false;
        for _ in 0..600 {
            if unsafe { libc::kill(pid as i32, 0) } != 0 {
                done = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if !done {
            anyhow::bail!("Previous LOKI did not stop");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restore_rejects_nonempty_destination() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("keep"), b"safe").unwrap();
        assert!(restore(Path::new("missing.zip"), tmp.path()).is_err());
        assert!(tmp.path().join("keep").exists());
    }
}
