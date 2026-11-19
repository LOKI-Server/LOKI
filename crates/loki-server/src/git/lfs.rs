use super::{
    MIB,
    credentials::{authenticate, authorize, challenge, token},
    in_repo, oid,
};
use crate::{Error, Result, State, auth};
use axum::{
    Json,
    body::Body,
    extract::FromRequest,
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path as DiskPath, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MEDIA: &str = "application/vnd.git-lfs+json";
pub(super) const IDLE: Duration = Duration::from_secs(120);
type Objects = HashMap<String, u64>;

pub(crate) fn valid_oid(oid: &str) -> bool {
    oid.len() == 64
        && oid
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

pub(crate) fn blob_path(data: &DiskPath, oid: &str) -> PathBuf {
    data.join("git-lfs").join(&oid[..2]).join(oid)
}

fn lfs_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, MEDIA),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        Json(body),
    )
        .into_response()
}

#[derive(Deserialize)]
struct Batch {
    operation: String,
    #[serde(default)]
    transfers: Vec<String>,
    #[serde(default)]
    hash_algo: Option<String>,
    objects: Vec<Object>,
}

#[derive(Deserialize)]
struct Object {
    oid: String,
    size: u64,
}

pub(super) async fn route(
    state: State,
    repo: String,
    endpoint: &str,
    request: axum::extract::Request,
) -> Response {
    match serve(state, repo, endpoint, request).await {
        Ok(response) => response,
        Err(error) if error.status == StatusCode::UNAUTHORIZED => challenge(),
        Err(error) => lfs_response(error.status, json!({"message":error.message})),
    }
}

async fn serve(
    state: State,
    id: String,
    endpoint: &str,
    request: axum::extract::Request,
) -> Result<Response> {
    let method = request.method().clone();
    let endpoint = endpoint
        .strip_prefix("info/lfs/")
        .ok_or_else(Error::not_found)?;
    let batch = method == Method::POST && endpoint == "objects/batch";
    let verifying = method == Method::POST && endpoint == "verify";
    let object = endpoint
        .strip_prefix("objects/")
        .filter(|oid| valid_oid(oid));
    let uploading = method == Method::PUT && object.is_some();
    if !batch && !verifying && !uploading && !(method == Method::GET && object.is_some()) {
        return Err(Error::not_found());
    }
    let peer = request.extensions().get::<auth::ClientIp>().cloned();
    let (repo, identity) = match authenticate(
        &state,
        &id,
        request.headers(),
        super::credentials::peer(&request),
        uploading || verifying,
    )
    .await
    {
        Err(error) if error.status == StatusCode::UNAUTHORIZED => {
            if let Some(ip) = peer {
                auth::rate(&state, format!("git-lfs-auth:{}", ip.0), 180, 60).await?;
            }
            return Err(error);
        }
        result => result?,
    };
    if batch {
        let Json(batch) = Json::<Batch>::from_request(request, &state)
            .await
            .map_err(|_| Error::bad("Invalid Git LFS batch request."))?;
        return batch_response(&state, &repo, &identity, batch).await;
    }
    if verifying {
        let Json(object): Json<Object> = Json::<Object>::from_request(request, &state)
            .await
            .map_err(|_| Error::bad("Invalid Git LFS verification request."))?;
        if !valid_oid(&object.oid) {
            return Err(Error::bad("Invalid Git LFS object ID."));
        }
        verify(
            &state.0.db,
            &state.0.data,
            &id,
            &HashMap::from([(object.oid, object.size)]),
        )
        .await?;
        return Ok(lfs_response(StatusCode::OK, json!({})));
    }
    let oid = object.expect("validated object endpoint");
    if uploading {
        let (headers, body) = request.into_parts();
        let length = headers
            .headers
            .get(header::CONTENT_LENGTH)
            .ok_or_else(|| {
                Error::new(
                    StatusCode::LENGTH_REQUIRED,
                    "An LFS object requires a content length.",
                )
            })?
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| Error::bad("Invalid LFS content length."))?;
        upload(&state, &repo, &identity, oid, length, body).await?;
        return Ok(lfs_response(StatusCode::OK, json!({})));
    }
    let guard = state.0.git_files.read().await;
    let current = token(&state.0.db, &id, &identity.id, true)
        .await?
        .ok_or_else(Error::unauthorized)?;
    authorize(&state.0.db, &repo, &current, false).await?;
    known(&state.0.db, &id, oid)
        .await?
        .ok_or_else(Error::not_found)?;
    let file = tokio::fs::File::open(blob_path(&state.0.data, oid))
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::not_found()
            } else {
                error.into()
            }
        })?;
    drop(guard);
    crate::sync::blobs::stream_file(file, request.headers(), None, Some(oid)).await
}

async fn batch_response(
    state: &State,
    repo: &super::Repo,
    identity: &super::credentials::Token,
    batch: Batch,
) -> Result<Response> {
    let write = match batch.operation.as_str() {
        "upload" => true,
        "download" => false,
        _ => {
            return Err(Error::bad(
                "Choose the Git LFS upload or download operation.",
            ));
        }
    };
    if batch.objects.len() > 1000
        || (!batch.transfers.is_empty() && !batch.transfers.iter().any(|t| t == "basic"))
        || batch
            .hash_algo
            .as_deref()
            .is_some_and(|hash| hash != "sha256")
    {
        return Err(Error::bad(
            "Git LFS supports up to 1000 objects per batch and the basic SHA-256 transfer.",
        ));
    }
    authorize(&state.0.db, repo, identity, write).await?;
    let settings = state.0.settings.read().await;
    let root = format!(
        "{}/git-http/{}.git/info/lfs",
        settings.public_url.trim_end_matches('/'),
        repo.id
    );
    let max = settings.git_lfs_object_mib * MIB;
    drop(settings);
    let mut results = Vec::with_capacity(batch.objects.len());
    for Object { oid, size } in batch.objects {
        if !valid_oid(&oid) {
            return Err(Error::bad("Invalid Git LFS object ID."));
        }
        let href = format!("{root}/objects/{oid}");
        let record = known(&state.0.db, &repo.id, &oid).await?;
        let exists = record == Some(size) && has_size(&state.0.data, &oid, size).await;
        let details = if record.is_some_and(|stored| stored != size) {
            json!({"error":{"code":422,"message":"Git LFS size does not match the stored object."}})
        } else if write && exists {
            json!({})
        } else if write && size <= max {
            json!({"actions":{"upload":{"href":href},"verify":{"href":format!("{root}/verify")}}})
        } else if write {
            json!({"error":{"code":413,"message":"Git LFS object exceeds the server size limit."}})
        } else if exists {
            json!({"actions":{"download":{"href":href}}})
        } else {
            json!({"error":{"code":404,"message":"Git LFS object is unavailable in this repository."}})
        };
        let mut result = json!({"oid":oid,"size":size,"authenticated":false});
        result
            .as_object_mut()
            .unwrap()
            .extend(details.as_object().unwrap().clone());
        results.push(result);
    }
    Ok(lfs_response(
        StatusCode::OK,
        json!({"transfer":"basic","hash_algo":"sha256","objects":results}),
    ))
}

async fn known(db: &sqlx::SqlitePool, repo: &str, oid: &str) -> Result<Option<u64>> {
    let count: Option<i64> =
        sqlx::query_scalar("SELECT size FROM git_lfs_objects WHERE repo_id=? AND oid=?")
            .bind(repo)
            .bind(oid)
            .fetch_optional(db)
            .await?;
    Ok(count.map(|size| size as u64))
}

async fn has_size(data: &DiskPath, oid: &str, size: u64) -> bool {
    tokio::fs::symlink_metadata(blob_path(data, oid))
        .await
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == size)
}

fn full() -> Error {
    Error::new(
        StatusCode::INSUFFICIENT_STORAGE,
        "Git LFS storage limit reached.",
    )
}

async fn check_quota(
    state: &State,
    db: &mut sqlx::SqliteConnection,
    repo: &str,
    additional: u64,
    physical: u64,
) -> Result<()> {
    let settings = state.0.settings.read().await;
    let used: i64 =
        sqlx::query_scalar("SELECT coalesce(sum(size),0) FROM git_lfs_objects WHERE repo_id=?")
            .bind(repo)
            .fetch_one(&mut *db)
            .await?;
    let total: i64 = sqlx::query_scalar("SELECT coalesce(sum(size),0) FROM (SELECT max(size) AS size FROM git_lfs_objects GROUP BY oid)")
        .fetch_one(&mut *db).await?;
    if (used as u64).saturating_add(additional) > settings.git_lfs_repo_mib * MIB
        || (total as u64).saturating_add(physical) > settings.git_lfs_total_mib * MIB
    {
        return Err(full());
    }
    Ok(())
}

async fn upload(
    state: &State,
    repo: &super::Repo,
    identity: &super::credentials::Token,
    oid: &str,
    length: u64,
    body: Body,
) -> Result<()> {
    if length > state.0.settings.read().await.git_lfs_object_mib * MIB {
        return Err(Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Git LFS object exceeds the server size limit.",
        ));
    }
    let slot = state
        .0
        .git_lfs_uploads
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| {
            Error::new(
                StatusCode::TOO_MANY_REQUESTS,
                "Other Git uploads are in progress.",
            )
        })?;
    let reservation = super::reserve_transfer(state, length)?;
    let existing = known(&state.0.db, &repo.id, oid).await?;
    let mut tx = state.0.db.acquire().await?;
    if existing == Some(length) && has_size(&state.0.data, oid, length).await {
        return Ok(());
    }
    check_quota(
        state,
        &mut tx,
        &repo.id,
        if existing.is_some() { 0 } else { length },
        0,
    )
    .await?;
    drop(tx);
    let staging = state.0.data.join("git-staging");
    let temporary = tempfile::NamedTempFile::new_in(staging)?;
    let mut file = tokio::fs::File::from_std(temporary.reopen()?);
    let mut stream = body.into_data_stream();
    let (mut digest, mut size) = (Sha256::new(), 0u64);
    while let Some(chunk) = tokio::time::timeout(IDLE, stream.next())
        .await
        .map_err(|_| Error::new(StatusCode::REQUEST_TIMEOUT, "Git LFS upload timed out."))?
    {
        let chunk = chunk.map_err(|_| Error::bad("Git LFS upload was interrupted."))?;
        size += chunk.len() as u64;
        if size > length {
            return Err(Error::bad("Git LFS upload exceeded its declared length."));
        }
        digest.update(&chunk);
        file.write_all(&chunk).await?;
    }
    if size != length || hex::encode(digest.finalize()) != oid {
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Git LFS object size or SHA-256 did not match.",
        ));
    }
    file.sync_all().await?;
    drop(file);
    let _lock = state.0.git_files.write().await;
    let current = token(&state.0.db, &repo.id, &identity.id, true)
        .await?
        .ok_or_else(Error::unauthorized)?;
    let repo = super::find(state, &repo.id).await?;
    authorize(&state.0.db, &repo, &current, true).await?;
    let mut tx = state.0.db.begin().await?;
    let existing: Option<i64> =
        sqlx::query_scalar("SELECT size FROM git_lfs_objects WHERE repo_id=? AND oid=?")
            .bind(&repo.id)
            .bind(oid)
            .fetch_optional(&mut *tx)
            .await?;
    let has_owner: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM git_lfs_objects WHERE oid=?)")
            .bind(oid)
            .fetch_one(&mut *tx)
            .await?;
    check_quota(
        state,
        &mut tx,
        &repo.id,
        if existing.is_some() { 0 } else { length },
        if has_owner { 0 } else { length },
    )
    .await?;
    let dest = blob_path(&state.0.data, oid);
    if !has_size(&state.0.data, oid, length).await {
        tokio::fs::create_dir_all(dest.parent().expect("LFS shard directory")).await?;
        temporary.persist(&dest).map_err(|error| error.error)?;
    }
    sqlx::query("INSERT INTO git_lfs_objects(repo_id,oid,size) VALUES(?,?,?) ON CONFLICT(repo_id,oid) DO UPDATE SET size=excluded.size")
        .bind(&repo.id).bind(oid).bind(length as i64).execute(&mut *tx).await?;
    tx.commit().await?;
    drop((slot, reservation));
    Ok(())
}

pub(super) async fn verify(
    db: &sqlx::SqlitePool,
    data: &DiskPath,
    repo: &str,
    objects: &Objects,
) -> Result<()> {
    for (oid, size) in objects {
        if known(db, repo, oid).await? != Some(*size) || !has_size(data, oid, *size).await {
            return Err(Error::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "Missing Git LFS object {oid}. Run git lfs push --all against this remote, then push again."
                ),
            ));
        }
    }
    Ok(())
}

pub(super) async fn share(
    state: &State,
    tx: &mut sqlx::SqliteTransaction<'_>,
    source: &str,
    target: &str,
    objects: &Objects,
) -> Result<()> {
    let mut added = false;
    for (oid, size) in objects {
        let target_size: Option<i64> =
            sqlx::query_scalar("SELECT size FROM git_lfs_objects WHERE repo_id=? AND oid=?")
                .bind(target)
                .bind(oid)
                .fetch_optional(&mut **tx)
                .await?;
        if target_size == Some(*size as i64) && has_size(&state.0.data, oid, *size).await {
            continue;
        }
        let source_size: Option<i64> =
            sqlx::query_scalar("SELECT size FROM git_lfs_objects WHERE repo_id=? AND oid=?")
                .bind(source)
                .bind(oid)
                .fetch_optional(&mut **tx)
                .await?;
        if source_size != Some(*size as i64) || !has_size(&state.0.data, oid, *size).await {
            return Err(Error::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Source repository is missing Git LFS object {oid}."),
            ));
        }
        added = true;
        sqlx::query("INSERT INTO git_lfs_objects(repo_id,oid,size) VALUES(?,?,?)")
            .bind(target)
            .bind(oid)
            .bind(*size as i64)
            .execute(&mut **tx)
            .await?;
    }
    if added {
        check_quota(state, tx, target, 0, 0).await?;
    }
    Ok(())
}

pub(super) async fn repository_pointers(git: &DiskPath, repo: &DiskPath) -> Result<Objects> {
    pointers(git, repo, &["--branches", "--tags"], false).await
}

pub(super) async fn pointers(
    git: &DiskPath,
    repo: &DiskPath,
    revisions: &[&str],
    quarantine: bool,
) -> Result<Objects> {
    if revisions.is_empty() {
        return Ok(HashMap::new());
    }
    let mut args = vec!["rev-list", "--objects", "--no-object-names", "--stdin"];
    let mut input = Vec::new();
    for revision in revisions {
        match *revision {
            "--branches" | "--tags" | "--all" => args.push(revision),
            "--exclude-existing" => args.extend(["--not", "--branches", "--tags", "--not"]),
            value => {
                input.extend_from_slice(value.as_bytes());
                input.push(b'\n');
            }
        }
    }
    let objects = scan(git, repo, &args, input, quarantine).await?;
    let meta = scan(
        git,
        repo,
        &["cat-file", "--batch-check"],
        objects,
        quarantine,
    )
    .await?;
    let mut names = Vec::new();
    for line in meta.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let line =
            std::str::from_utf8(line).map_err(|_| Error::bad("Invalid Git object listing."))?;
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        let [id, kind, size] = fields.as_slice() else {
            return Err(Error::bad(
                "Git could not read an object while checking LFS pointers.",
            ));
        };
        if oid(id)
            && *kind == "blob"
            && size
                .parse::<usize>()
                .is_ok_and(|size| (100..1024).contains(&size))
        {
            names.extend_from_slice(id.as_bytes());
            names.push(b'\n');
        }
    }
    let content = scan(git, repo, &["cat-file", "--batch"], names, quarantine).await?;
    let mut pending = content.as_slice();
    let mut result = HashMap::new();
    while !pending.is_empty() {
        let newline = pending
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| Error::bad("Invalid Git blob header."))?;
        let head = std::str::from_utf8(&pending[..newline])
            .map_err(|_| Error::bad("Invalid Git blob header."))?;
        let size = head
            .rsplit_once(' ')
            .and_then(|(_, size)| size.parse::<usize>().ok())
            .filter(|size| *size < 1024 && newline + size + 1 < pending.len())
            .ok_or_else(|| Error::bad("Invalid Git blob size."))?;
        let end = newline + 1 + size;
        if let Some((oid, size)) = pointer(&pending[newline + 1..end]) {
            if result
                .insert(oid, size)
                .is_some_and(|previous| previous != size)
            {
                return Err(Error::bad(
                    "Git LFS pointers disagree about an object's size.",
                ));
            }
        }
        pending = &pending[end + 1..];
    }
    Ok(result)
}

fn pointer(blob: &[u8]) -> Option<(String, u64)> {
    let text = std::str::from_utf8(blob).ok()?;
    let mut lines = text.lines();
    match lines.next()? {
        "version https://git-lfs.github.com/spec/v1"
        | "version https://hawser.github.com/spec/v1" => {}
        _ => return None,
    }
    let mut lines = lines.skip_while(|line| line.starts_with("ext-"));
    let oid = lines
        .next()?
        .strip_prefix("oid sha256:")
        .filter(|oid| valid_oid(oid))?;
    let size = lines.next()?.strip_prefix("size ")?.parse().ok()?;
    lines.next().is_none().then(|| (oid.to_owned(), size))
}

async fn scan(
    git: &DiskPath,
    repo: &DiskPath,
    args: &[&str],
    mut input: Vec<u8>,
    quarantine: bool,
) -> Result<Vec<u8>> {
    const LIMIT: u64 = 128 * MIB;
    if !input.is_empty() && !input.ends_with(b"\n") {
        input.push(b'\n');
    }
    let mut command = in_repo(git, repo);
    command.args(args).stdin(Stdio::piped());
    if quarantine {
        for key in [
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_QUARANTINE_PATH",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    }
    let mut child = crate::platform::Process::spawn_piped(&mut command)?;
    let mut stdin = child.child.stdin.take().unwrap();
    let mut output = child.child.stdout.take().unwrap().take(LIMIT + 1);
    let mut bytes = Vec::new();
    tokio::time::timeout(IDLE, async {
        tokio::try_join!(
            async move { stdin.write_all(&input).await },
            output.read_to_end(&mut bytes)
        )?;
        if bytes.len() as u64 > LIMIT || !child.child.wait().await?.success() {
            return Err(Error::bad("Could not finish checking Git LFS pointers."));
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| {
        Error::new(
            StatusCode::GATEWAY_TIMEOUT,
            "Git LFS pointer check timed out.",
        )
    })?
}

pub(crate) async fn verify_backup(data: &DiskPath, db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let objects: Vec<(String, i64)> =
        sqlx::query_as("SELECT oid,max(size) FROM git_lfs_objects GROUP BY oid")
            .fetch_all(db)
            .await?;
    for (oid, size) in objects {
        anyhow::ensure!(
            valid_oid(&oid) && size >= 0,
            "Invalid Git LFS index in backup"
        );
        let mut file = tokio::fs::File::open(blob_path(data, &oid)).await?;
        anyhow::ensure!(
            file.metadata().await?.len() == size as u64,
            "Wrong size for Git LFS object {oid}"
        );
        let mut digest = Sha256::new();
        let mut chunk = vec![0; 64 * 1024];
        loop {
            let count = file.read(&mut chunk).await?;
            if count == 0 {
                break;
            }
            digest.update(&chunk[..count]);
        }
        anyhow::ensure!(
            hex::encode(digest.finalize()) == oid,
            "Corrupt Git LFS object {oid}"
        );
    }
    Ok(())
}

pub(super) async fn collect(data: &DiskPath, db: &sqlx::SqlitePool) -> Result<()> {
    let root = data.join("git-lfs");
    let mut shards = tokio::fs::read_dir(root).await?;
    while let Some(shard) = shards.next_entry().await? {
        if !shard.file_type().await?.is_dir() {
            continue;
        }
        let mut entries = tokio::fs::read_dir(shard.path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            let oid = entry.file_name();
            let Some(oid) = oid.to_str().filter(|oid| valid_oid(oid)) else {
                continue;
            };
            let used: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM git_lfs_objects WHERE oid=?)")
                    .bind(oid)
                    .fetch_one(db)
                    .await?;
            if !used {
                tokio::fs::remove_file(entry.path()).await?;
            }
        }
    }
    Ok(())
}
