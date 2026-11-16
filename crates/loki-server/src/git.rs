//! Private bare repositories and live LOKI authorization.
mod collaboration;
mod transport;
mod web;

use crate::{Error, Result, State};
use axum::http::StatusCode;
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};
#[doc(hidden)]
pub use transport::receive_hook;
pub(crate) use web::router;

const MIB: u64 = 1024 * 1024;
const REPO_BYTES: u64 = 1024 * MIB;
const TOTAL_BYTES: u64 = 20 * 1024 * MIB;
const PUSH_BYTES: usize = 64 * MIB as usize;
const DEFAULT_REF: &str = "refs/heads/main";
const ZERO: &str = "0000000000000000000000000000000000000000";
const MAX_REPOS: i64 = 500;
const CONFIG: &str = "[core]\n\tbare = true\n\trepositoryformatversion = 0\n\tlogAllRefUpdates = false\n\tfsync = committed\n[receive]\n\tfsckObjects = true\n\tdenyNonFastForwards = true\n\tmaxInputSize = 64m\n\tunpackLimit = 1\n\thideRefs = refs/loki/\n\tadvertisePushOptions = false\n[uploadpack]\n\thideRefs = refs/loki/\n\tallowTipSHA1InWant = false\n\tallowReachableSHA1InWant = false\n\tallowAnySHA1InWant = false\n";

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
struct Repo {
    id: String,
    owner_id: String,
    name: String,
    upstream: Option<String>,
    created: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
enum Access {
    Read,
    Write,
    Manage,
}

fn repo_dir(data: &Path, id: &str) -> PathBuf {
    data.join("git-repos").join(format!("{id}.git"))
}

fn valid_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
}

pub(crate) fn backup_entry(path: &str) -> anyhow::Result<()> {
    let (repo, rest) = path
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("Invalid Git backup path"))?;
    anyhow::ensure!(
        repo.strip_suffix(".git").is_some_and(valid_id),
        "Invalid Git backup repository"
    );
    let local = if let Some(pull) = rest.strip_prefix("loki-pulls/") {
        let (id, nested) = pull
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("Invalid pull request path"))?;
        anyhow::ensure!(valid_id(id), "Invalid pull request directory");
        nested
    } else {
        rest
    };
    let allowed = matches!(local, "HEAD" | "config" | "packed-refs" | "shallow")
        || ["refs/heads/", "refs/tags/", "refs/loki/"]
            .iter()
            .any(|prefix| local.starts_with(prefix))
        || local.strip_prefix("objects/").is_some_and(|path| {
            if let Some(file) = path.strip_prefix("pack/pack-") {
                return file.split_once('.').is_some_and(|(object, extension)| {
                    oid(object) && matches!(extension, "pack" | "idx" | "rev" | "bitmap")
                });
            }
            path.split_once('/')
                .is_some_and(|(a, b)| a.len() == 2 && b.len() == 38 && oid(&format!("{a}{b}")))
        });
    anyhow::ensure!(allowed, "Unsafe Git backup entry");
    Ok(())
}

pub(crate) fn restore_config(data: &Path) -> anyhow::Result<()> {
    let root = data.join("git-repos");
    if !root.is_dir() {
        return Ok(());
    }
    for repo in std::fs::read_dir(root)? {
        let repository = repo?.path();
        write_config(&repository, DEFAULT_REF)?;
        let pulls = repository.join("loki-pulls");
        if pulls.is_dir() {
            for pull in std::fs::read_dir(pulls)? {
                write_config(&pull?.path(), "refs/heads/base")?;
            }
        }
    }
    Ok(())
}

fn write_config(repository: &Path, head: &str) -> anyhow::Result<()> {
    std::fs::write(repository.join("config"), CONFIG)?;
    std::fs::write(repository.join("HEAD"), format!("ref: {head}\n"))?;
    Ok(())
}

pub(crate) async fn recover(data: &Path, db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let root = data.join("git-repos");
    if let Ok(meta) = std::fs::symlink_metadata(&root) {
        safe_entry(&meta)?;
    }
    std::fs::create_dir_all(&root)?;
    crate::platform::private_directory(&root)?;
    let live: std::collections::HashSet<String> = sqlx::query_scalar("SELECT id FROM git_repos")
        .fetch_all(db)
        .await?
        .into_iter()
        .collect();
    let snapshots: std::collections::HashSet<String> =
        sqlx::query_scalar("SELECT snapshot FROM git_pull_snapshots")
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".git"))
            .filter(|id| valid_id(id))
        else {
            continue;
        };
        safe_entry(&entry.path().symlink_metadata()?)?;
        if !live.contains(id) {
            std::fs::remove_dir_all(entry.path())?;
            continue;
        }
        let pulls = entry.path().join("loki-pulls");
        if let Ok(meta) = pulls.symlink_metadata() {
            safe_entry(&meta)?;
            for snapshot in std::fs::read_dir(pulls)? {
                let snapshot = snapshot?;
                if snapshot
                    .file_name()
                    .to_str()
                    .is_some_and(|id| valid_id(id) && !snapshots.contains(id))
                {
                    safe_entry(&snapshot.path().symlink_metadata()?)?;
                    std::fs::remove_dir_all(snapshot.path())?;
                }
            }
        }
    }
    Ok(())
}

fn safe_entry(meta: &std::fs::Metadata) -> anyhow::Result<()> {
    anyhow::ensure!(
        !meta.file_type().is_symlink() && (meta.is_dir() || meta.is_file()),
        "Unsafe Git filesystem entry"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        anyhow::ensure!(
            meta.file_attributes() & 0x400 == 0,
            "Git repositories cannot use reparse points"
        );
    }
    Ok(())
}

fn size(path: &Path, max: u64) -> anyhow::Result<u64> {
    let mut pending = vec![path.to_owned()];
    let (mut total, mut entries) = (0u64, 0usize);
    while let Some(path) = pending.pop() {
        let meta = path.symlink_metadata()?;
        safe_entry(&meta)?;
        entries += 1;
        anyhow::ensure!(entries <= 250_000, "Git file-count limit reached");
        if meta.is_dir() {
            for child in std::fs::read_dir(path)? {
                pending.push(child?.path());
            }
        } else {
            total = total.saturating_add(meta.len());
            anyhow::ensure!(total <= max, "Git storage limit reached");
        }
    }
    Ok(total)
}

fn reserves(data: &Path, repo: Option<&Path>, additional: u64) -> Result<()> {
    let full = || {
        Error::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "Git storage limit reached.",
        )
    };
    crate::storage::ensure_disk_space(data, additional)?;
    let global = size(&data.join("git-repos"), TOTAL_BYTES).map_err(|_| full())?;
    if global.saturating_add(additional) > TOTAL_BYTES || additional > REPO_BYTES {
        return Err(full());
    }
    if let Some(repo) = repo
        && size(repo, REPO_BYTES)
            .map_err(|_| full())?
            .saturating_add(additional)
            > REPO_BYTES
    {
        return Err(full());
    }
    Ok(())
}

async fn find(state: &State, id: &str) -> Result<Repo> {
    if !valid_id(id) {
        return Err(Error::not_found());
    }
    sqlx::query_as("SELECT * FROM git_repos WHERE id=?")
        .bind(id)
        .fetch_optional(&state.0.db)
        .await?
        .ok_or_else(Error::not_found)
}

async fn access(db: &sqlx::SqlitePool, repo: &Repo, user: &str) -> Result<Option<Access>> {
    let level: i64 = sqlx::query_scalar("SELECT coalesce((SELECT CASE WHEN owner OR id=?2 THEN 3 ELSE coalesce((SELECT max(CASE role WHEN 'manage' THEN 3 WHEN 'write' THEN 2 WHEN 'read' THEN 1 END) FROM grants WHERE resource=?3 AND ((subject_kind='user' AND subject=?1) OR (subject_kind='group' AND subject IN(SELECT group_id FROM memberships WHERE user_id=?1)))),0) END FROM users WHERE id=?1 AND status='approved'),0)")
        .bind(user).bind(&repo.owner_id).bind(&repo.id).fetch_one(db).await?;
    Ok(match level {
        3 => Some(Access::Manage),
        2 => Some(Access::Write),
        1 => Some(Access::Read),
        _ => None,
    })
}

async fn require(state: &State, id: &str, user: &str, minimum: Access) -> Result<(Repo, Access)> {
    let repo = find(state, id).await?;
    let role = access(&state.0.db, &repo, user)
        .await?
        .ok_or_else(Error::not_found)?;
    if role < minimum {
        return Err(Error::forbidden());
    }
    Ok((repo, role))
}

/// The Git that Windows installers ship beside LOKI (MinGit, under `git/`).
#[cfg(windows)]
fn bundled() -> Option<PathBuf> {
    let exe = dunce::canonicalize(std::env::current_exe().ok()?).ok()?;
    Some(exe.parent()?.join("git").join("cmd").join("git.exe")).filter(|git| git.is_file())
}

/// `LOKI_GIT_EXECUTABLE`, then (on Windows) the bundled MinGit and a standard
/// Git for Windows install, then `PATH`.
fn executable() -> Result<PathBuf> {
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    let path = std::env::var_os("LOKI_GIT_EXECUTABLE")
        .map(PathBuf::from)
        .or_else(|| {
            #[cfg(windows)]
            {
                if let Some(git) = bundled() {
                    return Some(git);
                }
                if let Some(program_files) = std::env::var_os("ProgramFiles") {
                    let git = PathBuf::from(program_files).join("Git/cmd/git.exe");
                    if git.is_file() {
                        return Some(git);
                    }
                }
            }
            std::env::split_paths(&std::env::var_os("PATH")?)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(name))
                .find(|path| path.is_file())
        });
    let Some(path) = path.filter(|path| path.is_absolute() && path.is_file()) else {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Install Git 2.54 or later, or set LOKI_GIT_EXECUTABLE to its absolute path.",
        ));
    };
    Ok(path)
}

fn command(binary: &Path) -> Command {
    let mut git = Command::new(binary);
    git.env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .env(
            "PATH",
            binary.parent().unwrap_or_else(|| Path::new("/usr/bin")),
        )
        .args([
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "core.protectNTFS=true",
            "-c",
            "core.protectHFS=true",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        git.env("SystemRoot", root);
    }
    git
}

async fn checked_binary(state: &State) -> Result<PathBuf> {
    if let Some(binary) = state.0.git_binary.get() {
        return Ok(binary.clone());
    }
    let binary = executable()?;
    let mut version = command(&binary);
    version.arg("--version");
    let version_bytes = output(version, 1024).await.map_err(|_| Error::new(StatusCode::SERVICE_UNAVAILABLE,"Could not run Git. Set LOKI_GIT_EXECUTABLE to the absolute path of a supported Git binary."))?;
    let version = String::from_utf8_lossy(&version_bytes);
    let mut components = version
        .trim()
        .strip_prefix("git version ")
        .unwrap_or("")
        .split('.');
    let major = components
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_default();
    let minor = components
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_default();
    if (major, minor) < (2, 54) {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "LOKI Git hosting requires Git 2.54 or later.",
        ));
    }
    let mut hook = command(&binary);
    hook.args([
        "-c",
        "hook.loki-policy.event=pre-receive",
        "-c",
        "hook.loki-policy.command=false",
        "hook",
        "list",
        "pre-receive",
    ]);
    if !output(hook, 1024).await.as_ref().is_ok_and(|stdout| {
        String::from_utf8_lossy(stdout)
            .lines()
            .any(|line| line == "loki-policy")
    }) {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This Git executable lacks the configured-hook support (Git 2.54 or later) LOKI requires.",
        ));
    }
    let _ = state.0.git_binary.set(binary.clone());
    Ok(binary)
}

fn in_repo(binary: &Path, repo: &Path) -> Command {
    let mut git = command(binary);
    git.arg("--git-dir").arg(repo);
    git
}

async fn run(binary: &Path, repo: &Path, arguments: &[&str], max: usize) -> Result<Vec<u8>> {
    let mut git = in_repo(binary, repo);
    git.args(arguments);
    output(git, max).await
}

async fn output(mut command: Command, max: usize) -> Result<Vec<u8>> {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut git = crate::platform::Process::spawn_piped(&mut command).map_err(|_| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not start Git. Check the configured executable.",
            )
        })?;
        let mut bytes = Vec::new();
        git.child
            .stdout
            .take()
            .expect("piped Git output")
            .take(max as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > max {
            return Err(Error::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Git output exceeded its limit.",
            ));
        }
        if !git.child.wait().await?.success() {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "Git could not complete this operation.",
            ));
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| Error::new(StatusCode::GATEWAY_TIMEOUT, "Git operation timed out."))?
}

fn oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

async fn resolve(binary: &Path, repo: &Path, reference: &str) -> Result<String> {
    let value = run(
        binary,
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
        1024,
    )
    .await?;
    let value = String::from_utf8_lossy(&value).trim().to_owned();
    if !oid(&value) {
        return Err(Error::not_found());
    }
    Ok(value)
}

fn branch(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 200
        || value.starts_with('-')
        || value.contains(['~', '^', ':', '?', '*', '[', '\\', '@'])
        || value.contains("..")
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
        || value.split('/').any(|part| {
            part.is_empty()
                || part.starts_with('.')
                || part.ends_with('.')
                || part.ends_with(".lock")
        })
    {
        return Err(Error::bad("Invalid Git branch name."));
    }
    Ok(format!("refs/heads/{value}"))
}

async fn initialize(binary: &Path, dir: &Path) -> Result<()> {
    let mut init = command(binary);
    init.args([
        "init",
        "--bare",
        "--initial-branch=main",
        "--object-format=sha1",
        "--template=",
    ])
    .arg(dir);
    output(init, 4096).await?;
    write_config(dir, DEFAULT_REF)?;
    Ok(())
}

async fn fetch(
    binary: &Path,
    dest: &Path,
    source: &Path,
    reference: &str,
    into: &str,
) -> Result<()> {
    let mut command = in_repo(binary, dest);
    command
        .args([
            "-c",
            "protocol.file.allow=always",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-auto-maintenance",
            "--",
        ])
        .arg(source)
        .arg(format!("{reference}:{into}"));
    output(command, 4096).await?;
    Ok(())
}

async fn with_input(
    binary: &Path,
    dir: &Path,
    arguments: &[&str],
    input: String,
) -> Result<Vec<u8>> {
    use tokio::io::AsyncWriteExt;
    let mut command = in_repo(binary, dir);
    command.args(arguments).stdin(Stdio::piped());
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut child = crate::platform::Process::spawn_piped(&mut command)?;
        let mut stdin = child.child.stdin.take().expect("piped input");
        let mut stdout = child.child.stdout.take().expect("piped output");
        stdin.write_all(input.as_bytes()).await?;
        drop(stdin);
        let mut output = Vec::new();
        (&mut stdout).take(4097).read_to_end(&mut output).await?;
        if output.len() > 4096 || !child.child.wait().await?.success() {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "Git could not update the repository.",
            ));
        }
        Ok(output)
    })
    .await
    .map_err(|_| Error::new(StatusCode::GATEWAY_TIMEOUT, "Git operation timed out."))?
}
