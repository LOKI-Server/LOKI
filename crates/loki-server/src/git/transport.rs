use super::{
    Access, MIB, ZERO, checked_binary,
    credentials::{authenticate, authorize, challenge, peer, token},
    in_repo, lfs, oid, repo_dir, reserve_transfer, reserves, size, valid_id,
};
use crate::{Error, Result, State, auth};
use axum::{
    Extension,
    body::{Body, Bytes, to_bytes},
    extract::{Path, RawQuery, State as ExtractState},
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use std::{
    io::{BufRead, Read},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const IO_TIMEOUT: Duration = lfs::IDLE;

pub(super) async fn route(
    ExtractState(state): ExtractState<State>,
    Path((id, endpoint)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    Extension(ip): Extension<auth::ClientIp>,
    request: axum::extract::Request,
) -> Response {
    if endpoint.starts_with("info/lfs/") {
        let Some(id) = id.strip_suffix(".git").filter(|id| valid_id(id)) else {
            return Error::not_found().into_response();
        };
        return lfs::route(state, id.to_owned(), &endpoint, request).await;
    }
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
    let receive = service == "receive-pack";
    let (_repo, auth) =
        authenticate(&state, &id, request.headers(), peer(&request), receive).await?;
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
    let limits = state.0.settings.read().await.clone();
    let (push, input) = if writing {
        (
            Some(stage_push(&state, request, limits.git_push_mib * MIB).await?),
            Bytes::new(),
        )
    } else if advertise {
        (None, Bytes::new())
    } else {
        (None, request_body(request, service).await?)
    };
    let (read_guard, write_guard) = if writing {
        (None, Some(state.0.git_files.clone().write_owned().await))
    } else {
        (Some(state.0.git_files.clone().read_owned().await), None)
    };
    let auth = token(&state.0.db, &id, &auth.id, true)
        .await?
        .ok_or_else(Error::unauthorized)?;
    let repo = super::find(&state, &id).await?;
    authorize(&state.0.db, &repo, &auth, receive).await?;
    if let Some((_, bytes, _)) = &push {
        let data = state.0.data.clone();
        let directory = dir.clone();
        let bytes = *bytes;
        let limits = limits.clone();
        tokio::task::spawn_blocking(move || reserves(&data, Some(&directory), bytes, &limits))
            .await??;
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
            .args([
                "-c",
                &format!("receive.maxInputSize={}", limits.git_push_mib * MIB),
            ])
            .env("LOKI_GIT_EXECUTABLE", &git)
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
    command.arg(&dir);
    let reservation = if let Some((file, _, reservation)) = push {
        command.stdin(Stdio::from(file));
        Some(reservation)
    } else {
        command.stdin(Stdio::piped());
        None
    };
    let mut child = crate::platform::Process::spawn_piped(&mut command)?;
    let stdin = child.child.stdin.take();
    let mut stdout = child.child.stdout.take().expect("piped Git stdout");
    let (mut writer, reader) = tokio::io::duplex(128 * 1024);
    let advertisement = format!("# service=git-{service}\n");
    tokio::spawn(async move {
        let _guards = (read_guard, write_guard, slots, input_slot, reservation);
        let operation = async {
            if preamble {
                writer
                    .write_all(
                        format!("{:04x}{advertisement}0000", advertisement.len() + 4).as_bytes(),
                    )
                    .await?;
            }
            let send = async move {
                if let Some(mut stdin) = stdin
                    && let Err(error) =
                        tokio::time::timeout(IO_TIMEOUT, stdin.write_all(&input)).await?
                    && error.kind() != std::io::ErrorKind::BrokenPipe
                {
                    return Err(error);
                }
                Ok::<_, std::io::Error>(())
            };
            let copy = async {
                let mut buffer = vec![0; 64 * 1024];
                loop {
                    let count =
                        tokio::time::timeout(IO_TIMEOUT, stdout.read(&mut buffer)).await??;
                    if count == 0 {
                        break;
                    }
                    tokio::time::timeout(IO_TIMEOUT, writer.write_all(&buffer[..count])).await??;
                }
                Ok::<_, std::io::Error>(())
            };
            tokio::try_join!(send, copy)?;
            let status = tokio::time::timeout(IO_TIMEOUT, child.child.wait()).await??;
            if !status.success() {
                tracing::warn!(repo=%id, %status, "Git transport process exited unsuccessfully");
            }
            Ok::<_, std::io::Error>(())
        };
        tokio::select! {
            result = operation => if let Err(error) = result {
                tracing::warn!(repo=%id, %error, "Git transport interrupted");
            },
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

fn request_encoding(request: &axum::extract::Request, service: &str) -> Result<bool> {
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
    match request
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
    {
        None | Some("identity") => Ok(false),
        Some("gzip") => Ok(true),
        _ => Err(Error::bad("Unsupported Git request encoding.")),
    }
}

async fn stage_push(
    state: &State,
    request: axum::extract::Request,
    max: u64,
) -> Result<(std::fs::File, u64, super::TransferReservation)> {
    use std::io::{Seek, SeekFrom};
    let compressed = request_encoding(&request, "receive-pack")?;
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let too_large = || {
        Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Git push exceeds the configured size limit.",
        )
    };
    if declared.is_some_and(|length| length > max) {
        return Err(too_large());
    }
    let reservation = reserve_transfer(
        state,
        declared.unwrap_or(max) + if compressed { max } else { 0 },
    )?;
    let staging = state.0.data.join("git-staging");
    let mut file = tokio::fs::File::from_std(tempfile::tempfile_in(&staging)?);
    let mut stream = request.into_body().into_data_stream();
    let mut length = 0;
    while let Some(chunk) = tokio::time::timeout(IO_TIMEOUT, stream.next())
        .await
        .map_err(|_| Error::new(StatusCode::REQUEST_TIMEOUT, "Git upload timed out."))?
    {
        let chunk = chunk.map_err(|_| Error::bad("Git upload was interrupted."))?;
        length += chunk.len() as u64;
        if length > max || declared.is_some_and(|size| length > size) {
            return Err(too_large());
        }
        file.write_all(&chunk).await?;
    }
    if declared.is_some_and(|size| length != size) {
        return Err(Error::bad("Git upload ended before its declared size."));
    }
    file.flush().await?;
    let mut file = file.into_std().await;
    let (file, length) = tokio::task::spawn_blocking(move || {
        file.seek(SeekFrom::Start(0))?;
        if !compressed {
            return Ok((file, length));
        }
        let mut plain = tempfile::tempfile_in(&staging)?;
        let length = std::io::copy(
            &mut flate2::read::GzDecoder::new(file).take(max + 1),
            &mut plain,
        )
        .map_err(|_| Error::bad("Invalid Git compression."))?;
        if length > max {
            return Err(too_large());
        }
        plain.seek(SeekFrom::Start(0))?;
        Ok::<_, Error>((plain, length))
    })
    .await??;
    Ok((file, length, reservation))
}

async fn request_body(request: axum::extract::Request, service: &str) -> Result<Bytes> {
    let compressed = request_encoding(&request, service)?;
    let max = MIB as usize;
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
    let mut tips = Vec::new();
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
        if *new != ZERO {
            tips.push(new.to_string());
        }
        if *reference == repo.default_ref() {
            anyhow::ensure!(
                *new != ZERO && (!repo.protect_default || (role == Access::Manage && *old == ZERO)),
                "Open a pull request to change the default branch"
            );
        }
    }
    let limits = crate::db::settings(&db).await?;
    let repository = repo_dir(&data, &id);
    size(&repository, limits.git_repo_mib * MIB)?;
    size(&data.join("git-repos"), limits.git_total_mib * MIB)?;
    if !tips.is_empty() {
        let git = super::executable().map_err(|error| anyhow::anyhow!(error.message))?;
        let mut revisions = vec!["--exclude-existing"];
        revisions.extend(tips.iter().map(String::as_str));
        let objects = lfs::pointers(&git, &repository, &revisions, true)
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        lfs::verify(&db, &data, &id, &objects)
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
    }
    Ok(())
}
