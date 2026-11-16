use super::{
    Access, DEFAULT_REF, PUSH_BYTES, REPO_BYTES, TOTAL_BYTES, ZERO, access, checked_binary, find,
    in_repo, oid, repo_dir, reserves, size, valid_id,
};
use crate::{Error, Result, State, auth};
use axum::{
    Extension,
    body::{Body, Bytes, to_bytes},
    extract::{Path, RawQuery, State as ExtractState},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    io::{BufRead, Read},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const IO_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, sqlx::FromRow)]
struct Token {
    id: String,
    user_id: String,
    write: bool,
}

async fn token(
    db: &sqlx::SqlitePool,
    id: &str,
    secret: &str,
    by_id: bool,
) -> Result<Option<Token>> {
    Ok(sqlx::query_as("SELECT t.id,t.user_id,t.write FROM git_tokens t JOIN users u ON u.id=t.user_id WHERE t.repo_id=?1 AND ((?2 AND t.id=?3) OR (NOT ?2 AND t.digest=?3)) AND t.expires>?4 AND t.auth_version=u.auth_version AND u.status='approved'")
        .bind(id).bind(by_id).bind(secret).bind(auth::now()).fetch_optional(db).await?)
}

fn challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, r#"Basic realm="LOKI Git""#),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "Use a repository token as your password.",
    )
        .into_response()
}

fn credential(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    if raw.len() > 1024 {
        return None;
    }
    let text = String::from_utf8(STANDARD.decode(raw).ok()?).ok()?;
    let (_, password) = text.split_once(':')?;
    if !password.starts_with("loki_git_") {
        return None;
    }
    Some(auth::digest(password))
}

pub(super) async fn route(
    ExtractState(state): ExtractState<State>,
    Path((id, endpoint)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    Extension(ip): Extension<auth::ClientIp>,
    request: axum::extract::Request,
) -> Response {
    match serve(state, id, endpoint, query, ip, request).await {
        Ok(response) => response,
        Err(error) if error.status == StatusCode::UNAUTHORIZED => challenge(),
        Err(error) => error.into_response(),
    }
}

async fn serve(
    state: State,
    id: String,
    endpoint: String,
    query: Option<String>,
    ip: auth::ClientIp,
    request: axum::extract::Request,
) -> Result<Response> {
    let id = id
        .strip_suffix(".git")
        .filter(|id| valid_id(id))
        .ok_or_else(Error::not_found)?
        .to_owned();
    let query = query.as_deref();
    let (service, advertise) = match (request.method(), endpoint.as_str(), query) {
        (&Method::GET, "info/refs", Some("service=git-upload-pack")) => ("upload-pack", true),
        (&Method::GET, "info/refs", Some("service=git-receive-pack")) => ("receive-pack", true),
        (&Method::POST, "git-upload-pack", None) => ("upload-pack", false),
        (&Method::POST, "git-receive-pack", None) => ("receive-pack", false),
        _ => return Err(Error::not_found()),
    };
    auth::rate(&state, format!("git-http:{}", ip.0), 180, 60).await?;
    // Public tokens go through LOKI-managed HTTPS or a TLS proxy on loopback.
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|p| p.0);
    if peer.is_some_and(|addr| !addr.ip().is_loopback()) {
        return Err(Error::forbidden());
    }
    if request.headers().contains_key(header::ORIGIN)
        || request.headers().contains_key("sec-fetch-site")
    {
        return Err(Error::forbidden());
    }
    let public = state.0.settings.read().await.public_url.clone();
    if public.starts_with("https://")
        && request
            .headers()
            .get("x-forwarded-proto")
            .is_none_or(|h| h != "https")
    {
        return Err(Error::forbidden());
    }
    let key = credential(request.headers()).ok_or_else(Error::unauthorized)?;
    let auth = token(&state.0.db, &id, &key, false)
        .await?
        .ok_or_else(Error::unauthorized)?;
    let repo = find(&state, &id).await?;
    let receive = service == "receive-pack";
    authorize(&state.0.db, &repo, &auth, receive).await?;
    let slots = state
        .0
        .git_processes
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            Error::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Git is busy. Try again shortly.",
            )
        })?;
    let git = checked_binary(&state).await?;
    let dir = repo_dir(&state.0.data, &id);
    let protocol = request
        .headers()
        .get("git-protocol")
        .filter(|v| matches!(v.as_bytes(), b"version=1" | b"version=2"))
        .cloned();
    let preamble =
        advertise && (receive || protocol.as_ref().is_none_or(|value| value != "version=2"));
    let writing = receive && !advertise;
    let input_slot = if writing {
        Some(
            state
                .0
                .git_uploads
                .clone()
                .try_acquire_owned()
                .map_err(|_| {
                    Error::new(
                        StatusCode::TOO_MANY_REQUESTS,
                        "Other Git pushes are in progress. Try again shortly.",
                    )
                })?,
        )
    } else {
        None
    };
    let input = if advertise {
        Bytes::new()
    } else {
        request_body(request, service).await?
    };
    let (read_guard, write_guard) = if writing {
        (None, Some(state.0.git_files.clone().write_owned().await))
    } else {
        (Some(state.0.git_files.clone().read_owned().await), None)
    };
    let auth = token(&state.0.db, &id, &auth.id, true)
        .await?
        .ok_or_else(Error::unauthorized)?;
    authorize(&state.0.db, &repo, &auth, receive).await?;
    if writing {
        let data = state.0.data.clone();
        let directory = dir.clone();
        let bytes = input.len() as u64;
        tokio::task::spawn_blocking(move || reserves(&data, Some(&directory), bytes)).await??;
    }
    let mut command = in_repo(&git, &dir);
    if writing {
        // Git 2.54 supports config-defined hooks with stdin on all three hosts.
        command
            .args([
                "-c",
                "hook.loki-policy.event=pre-receive",
                "-c",
                "hook.loki-policy.command=\"$LOKI_GIT_HOOK_EXE\" git-receive-hook",
            ])
            .env("LOKI_GIT_HOOK_EXE", hook_path()?)
            .env("LOKI_GIT_HOOK_DATA", &state.0.data)
            .env("LOKI_GIT_HOOK_REPO", &id)
            .env("LOKI_GIT_HOOK_TOKEN", &auth.id);
    }
    if let Some(protocol) = protocol {
        command.env(
            "GIT_PROTOCOL",
            protocol
                .to_str()
                .map_err(|_| Error::bad("Invalid Git protocol."))?,
        );
    }
    command.args([service, "--stateless-rpc"]);
    if advertise {
        command.arg("--advertise-refs");
    }
    command.arg(&dir).stdin(Stdio::piped());
    let mut child = crate::platform::Process::spawn_piped(&mut command)?;
    let mut stdin = child.child.stdin.take().expect("piped Git stdin");
    let mut stdout = child.child.stdout.take().expect("piped Git stdout");
    let (mut writer, reader) = tokio::io::duplex(128 * 1024);
    let advertisement = format!("# service=git-{service}\n");
    tokio::spawn(async move {
        let _guards = (read_guard, write_guard, slots, input_slot);
        let operation = async {
            if preamble {
                writer
                    .write_all(
                        format!("{:04x}{advertisement}0000", advertisement.len() + 4).as_bytes(),
                    )
                    .await?;
            }
            let send = async move {
                if let Err(error) = stdin.write_all(&input).await
                    && error.kind() != std::io::ErrorKind::BrokenPipe
                {
                    return Err(error);
                }
                Ok(())
            };
            let copy = async { tokio::io::copy(&mut stdout, &mut writer).await.map(|_| ()) };
            tokio::try_join!(send, copy)?;
            child.child.wait().await?;
            Ok::<_, std::io::Error>(())
        };
        tokio::select! {
            _ = tokio::time::timeout(IO_TIMEOUT,operation) => {},
            _ = writer_closed(state.clone()) => {},
        }
    });
    Ok(Response::builder()
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::CONTENT_TYPE,
            format!(
                "application/x-git-{service}-{}",
                if advertise { "advertisement" } else { "result" }
            ),
        )
        .body(Body::from_stream(tokio_util::io::ReaderStream::new(reader)))?)
}

fn hook_path() -> Result<PathBuf> {
    let exe = dunce::canonicalize(std::env::current_exe()?)?;
    #[cfg(windows)]
    return Ok(PathBuf::from(exe.to_string_lossy().replace('\\', "/")));
    #[cfg(not(windows))]
    Ok(exe)
}

async fn writer_closed(state: State) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if state
            .0
            .maintenance
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
    }
}

async fn request_body(request: axum::extract::Request, service: &str) -> Result<Bytes> {
    let expected = format!("application/x-git-{service}-request");
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .is_none_or(|v| v != &expected)
    {
        return Err(Error::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Invalid Git request type.",
        ));
    }
    let compressed = match request
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
    {
        None | Some("identity") => false,
        Some("gzip") => true,
        _ => return Err(Error::bad("Unsupported Git request encoding.")),
    };
    let max = if service == "receive-pack" {
        PUSH_BYTES
    } else {
        1024 * 1024
    };
    let bytes = tokio::time::timeout(IO_TIMEOUT, to_bytes(request.into_body(), max))
        .await
        .map_err(|_| Error::new(StatusCode::REQUEST_TIMEOUT, "Git request timed out."))?
        .map_err(|_| {
            Error::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Git request exceeds its size limit.",
            )
        })?;
    if !compressed {
        return Ok(bytes);
    }
    tokio::task::spawn_blocking(move || {
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_ref())
            .take(max as u64 + 1)
            .read_to_end(&mut plain)
            .map_err(|_| Error::bad("Invalid Git compression."))?;
        if plain.len() > max {
            return Err(Error::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Git request exceeds its size limit.",
            ));
        }
        Ok(Bytes::from(plain))
    })
    .await?
}

async fn authorize(
    db: &sqlx::SqlitePool,
    repo: &super::Repo,
    token: &Token,
    receive: bool,
) -> Result<Access> {
    let role = access(db, repo, &token.user_id)
        .await?
        .ok_or_else(Error::not_found)?;
    if receive && (!token.write || role < Access::Write) {
        return Err(Error::forbidden());
    }
    Ok(role)
}

/// Invoked by Git only after its incoming objects pass verification.
pub async fn receive_hook() -> anyhow::Result<()> {
    let data = PathBuf::from(
        std::env::var_os("LOKI_GIT_HOOK_DATA")
            .ok_or_else(|| anyhow::anyhow!("Missing LOKI data path"))?,
    );
    let id = std::env::var("LOKI_GIT_HOOK_REPO")?;
    let token_id = std::env::var("LOKI_GIT_HOOK_TOKEN")?;
    anyhow::ensure!(
        valid_id(&id) && valid_id(&token_id),
        "Invalid LOKI Git request"
    );
    let db = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(data.join("loki.db"))
            .busy_timeout(Duration::from_secs(5)),
    )
    .await?;
    let repo: super::Repo = sqlx::query_as("SELECT * FROM git_repos WHERE id=?")
        .bind(&id)
        .fetch_one(&db)
        .await?;
    let auth = token(&db, &id, &token_id, true)
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?
        .ok_or_else(|| anyhow::anyhow!("LOKI Git credential expired or was revoked"))?;
    let role = authorize(&db, &repo, &auth, true)
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?;
    let lines = std::io::stdin().lock().take(256 * 1024 + 1);
    let mut received = 0usize;
    for line in std::io::BufReader::new(lines).lines() {
        let line = line?;
        received += line.len() + 1;
        anyhow::ensure!(received <= 256 * 1024, "Too many Git ref changes");
        let parts: Vec<_> = line.split(' ').collect();
        let [old, new, reference] = parts.as_slice() else {
            anyhow::bail!("Invalid ref change");
        };
        anyhow::ensure!(oid(old) && oid(new), "Invalid ref object");
        anyhow::ensure!(
            reference.starts_with("refs/heads/") || reference.starts_with("refs/tags/"),
            "Only branch and tag updates are permitted"
        );
        if *reference == DEFAULT_REF {
            anyhow::ensure!(
                role == Access::Manage && *old == ZERO && *new != ZERO,
                "Open a pull request to change the default branch"
            );
        }
    }
    size(&repo_dir(&data, &id), REPO_BYTES)?;
    size(&data.join("git-repos"), TOTAL_BYTES)?;
    Ok(())
}
