use super::{Share, check_opened_file, excluded, file_changed};
use crate::{Error, Result};
use axum::{
    body::Body,
    http::{StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
};
use tokio::sync::{Semaphore, mpsc};

static SCANS: Semaphore = Semaphore::const_new(4);
static ARCHIVES: Semaphore = Semaphore::const_new(4);
const MAX_ENTRIES: usize = 100_000;
const MAX_PATH_BYTES: usize = 16 * 1024 * 1024;
const MIN_CHUNK: usize = 256 * 1024;
const MAX_CHUNK: usize = 4 * 1024 * 1024;
const CHUNK_PAGE: usize = 4096;

#[derive(Serialize)]
pub(super) struct Manifest {
    directory: bool,
    revision: String,
    entries: Vec<Entry>,
}

#[derive(Serialize)]
struct Entry {
    path: String,
    directory: bool,
    size: u64,
    sha256: Option<String>,
    last_modified: Option<String>,
}

#[derive(Serialize)]
pub(super) struct ChunkPage {
    chunks: Vec<Chunk>,
    next_offset: Option<u64>,
}

#[derive(Serialize)]
struct Chunk {
    offset: u64,
    size: usize,
    sha256: String,
}

fn busy() -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "File server is busy. Try again shortly.",
    )
}

fn too_large() -> Error {
    Error::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "This folder has too many entries. Download a smaller folder.",
    )
}

fn unsafe_path() -> Error {
    Error::bad("A folder entry has a name that cannot be downloaded safely.")
}

fn safe_leaf(name: &str) -> Result<()> {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || device
        || name.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
        || name.chars().any(char::is_control)
    {
        return Err(unsafe_path());
    }
    Ok(())
}

pub(super) async fn is_directory(share: &Share, path: &str) -> Result<bool> {
    let share = share.clone();
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || directory(&share, &path)).await?
}

fn directory(share: &Share, path: &str) -> Result<bool> {
    let (dir, relative) = share.open(path)?;
    Ok(relative.as_os_str().is_empty() || dir.metadata(relative)?.is_dir())
}

fn open(share: &Share, path: &str) -> Result<File> {
    let (dir, relative) = share.open(path)?;
    if !dir.symlink_metadata(&relative)?.is_file() {
        return Err(Error::not_found());
    }
    let file = dir.open(&relative)?.into_std();
    if !file.metadata()?.is_file() {
        return Err(Error::not_found());
    }
    check_opened_file(&file, &relative, &share.excluded_extensions)?;
    Ok(file)
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else if name.is_empty() {
        parent.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn tree(share: &Share, root: &str) -> Result<Vec<(String, bool)>> {
    let data = dunce::canonicalize(&share.data)?;
    let absolute_root = dunce::canonicalize(&share.root)?;
    let mut entries = Vec::new();
    let mut pending = vec![String::new()];
    let mut path_bytes = 0;
    while let Some(parent) = pending.pop() {
        let path = join(root, &parent);
        let (dir, path) = share.open(&path)?;
        let absolute = absolute_root.join(&path);
        let current = if path.as_os_str().is_empty() {
            dir
        } else {
            dir.open_dir(path)?
        };
        for item in current.entries()? {
            let item = item?;
            let kind = item.file_type()?;
            if !kind.is_dir() && !kind.is_file() {
                continue;
            }
            #[cfg(windows)]
            {
                use cap_std::fs::MetadataExt;
                if item.metadata()?.file_attributes() & 0x400 != 0 {
                    continue;
                }
            }
            let name = item.file_name().into_string().map_err(|_| unsafe_path())?;
            if kind.is_file() && excluded(Path::new(&name), &share.excluded_extensions) {
                continue;
            }
            if super::inside_data(&data, &absolute.join(&name)) {
                continue;
            }
            safe_leaf(&name)?;
            let child = join(&parent, &name);
            let full = join(root, &child);
            match share.open(&full) {
                Ok(_) => {}
                Err(error)
                    if matches!(error.status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
            path_bytes += child.len();
            if entries.len() >= MAX_ENTRIES || path_bytes > MAX_PATH_BYTES {
                return Err(too_large());
            }
            if kind.is_dir() {
                pending.push(child.clone());
            }
            entries.push((child, kind.is_dir()));
        }
    }
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

fn checked_read(
    file: &mut File,
    mut consume: impl FnMut(&[u8]) -> Result<()>,
) -> Result<(u64, Option<String>)> {
    let before = file.metadata()?;
    let modified = before.modified().ok();
    let mut remaining = before.len();
    let mut buffer = vec![0; 64 * 1024];
    while remaining > 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        let count = file.read(&mut buffer[..limit])?;
        if count == 0 {
            return Err(file_changed());
        }
        consume(&buffer[..count])?;
        remaining -= count as u64;
    }
    let after = file.metadata()?;
    if after.len() != before.len() || after.modified().ok() != modified {
        return Err(file_changed());
    }
    Ok((before.len(), modified.and_then(super::http_date)))
}

pub(super) async fn manifest(share: Share, path: String) -> Result<Manifest> {
    let permit = SCANS.try_acquire().map_err(|_| busy())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let directory = directory(&share, &path)?;
        let members = if directory {
            tree(&share, &path)?
        } else {
            vec![(String::new(), false)]
        };
        let mut entries = Vec::with_capacity(members.len());
        let mut revision = Sha256::new();
        revision.update([u8::from(directory)]);
        for (member, directory) in members {
            let (size, sha256, last_modified) = if directory {
                (0, None, None)
            } else {
                let mut file = open(&share, &join(&path, &member))?;
                let mut hash = Sha256::new();
                let (size, modified) = checked_read(&mut file, |bytes| {
                    hash.update(bytes);
                    Ok(())
                })?;
                (size, Some(hex::encode(hash.finalize())), modified)
            };
            revision.update((member.len() as u64).to_le_bytes());
            revision.update(member.as_bytes());
            revision.update([u8::from(directory)]);
            revision.update(size.to_le_bytes());
            if let Some(hash) = &sha256 {
                revision.update(hash.as_bytes());
            }
            entries.push(Entry {
                path: member,
                directory,
                size,
                sha256,
                last_modified,
            });
        }
        Ok(Manifest {
            directory,
            revision: hex::encode(revision.finalize()),
            entries,
        })
    })
    .await?
}

// The same deterministic table is generated by Desktop. A boundary survives
// insertions because bytes shift out of the rolling state after 64 steps.
const fn gear(byte: u8) -> u64 {
    let mut value = (byte as u64).wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

const GEAR: [u64; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < table.len() {
        table[i] = gear(i as u8);
        i += 1;
    }
    table
};

pub(super) async fn chunks(
    share: Share,
    path: String,
    sha256: String,
    start: u64,
) -> Result<ChunkPage> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(Error::bad("Provide the current file SHA-256."));
    }
    let permit = SCANS.try_acquire().map_err(|_| busy())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut file = open(&share, &path)?;
        let mut whole = Sha256::new();
        let mut chunks = Vec::new();
        let mut next_offset = None;
        let mut state = 0u64;
        let mut chunk = Vec::with_capacity(MAX_CHUNK);
        let mut offset = 0;
        let mut emit = |chunk: &[u8]| {
            if offset >= start && chunks.len() < CHUNK_PAGE {
                chunks.push(Chunk {
                    offset,
                    size: chunk.len(),
                    sha256: hex::encode(Sha256::digest(chunk)),
                });
            } else if offset >= start && next_offset.is_none() {
                next_offset = Some(offset);
            }
            offset += chunk.len() as u64;
        };
        checked_read(&mut file, |bytes| {
            whole.update(bytes);
            for &byte in bytes {
                chunk.push(byte);
                state = (state << 1).wrapping_add(GEAR[byte as usize]);
                if chunk.len() == MAX_CHUNK
                    || (chunk.len() >= MIN_CHUNK && state & ((1 << 20) - 1) == 0)
                {
                    emit(&chunk);
                    chunk.clear();
                    state = 0;
                }
            }
            Ok(())
        })?;
        if !chunk.is_empty() {
            emit(&chunk);
        }
        if hex::encode(whole.finalize()) != sha256 {
            return Err(file_changed());
        }
        if start != 0 && chunks.first().is_none_or(|chunk| chunk.offset != start) {
            return Err(Error::bad("Invalid chunk page offset."));
        }
        Ok(ChunkPage {
            chunks,
            next_offset,
        })
    })
    .await?
}

struct ArchiveWriter(mpsc::Sender<io::Result<Bytes>>);

impl Write for ArchiveWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = bytes.len().min(64 * 1024);
        self.0
            .blocking_send(Ok(Bytes::copy_from_slice(&bytes[..length])))
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) async fn archive(share: Share, path: String) -> Result<Response> {
    let permit = ARCHIVES.try_acquire().map_err(|_| busy())?;
    let (share, path, name, entries) = tokio::task::spawn_blocking(move || -> Result<_> {
        if !directory(&share, &path)? {
            return Err(Error::not_found());
        }
        let name = Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Shared files")
            .to_owned();
        safe_leaf(&name)?;
        let entries = tree(&share, &path)?;
        Ok((share, path, name, entries))
    })
    .await??;
    let (sender, receiver) = mpsc::channel(16);
    let worker = sender.clone();
    let folder = name.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let write = || -> Result<()> {
            let writer = io::BufWriter::with_capacity(64 * 1024, ArchiveWriter(worker.clone()));
            let mut zip = zip::ZipWriter::new_stream(writer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .large_file(true);
            zip.add_directory(format!("{folder}/"), options)?;
            for (relative, directory) in entries {
                if worker.is_closed() {
                    return Ok(());
                }
                let name = format!("{folder}/{relative}");
                if directory {
                    zip.add_directory(format!("{name}/"), options)?;
                } else {
                    let mut file = open(&share, &join(&path, &relative))?;
                    zip.start_file(name, options)?;
                    checked_read(&mut file, |bytes| Ok(zip.write_all(bytes)?))?;
                }
            }
            zip.finish()?.flush()?;
            Ok(())
        };
        if let Err(error) = write()
            && !worker.is_closed()
        {
            tracing::warn!(message = %error.message, "folder download failed");
            let _ = worker.blocking_send(Err(io::Error::other(error.message)));
        }
    });
    drop(sender);
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    let filename =
        url::form_urlencoded::byte_serialize(format!("{name}.zip").as_bytes()).collect::<String>();
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"folder.zip\"; filename*=UTF-8''{filename}"),
        )
        .body(Body::from_stream(stream))?)
}
