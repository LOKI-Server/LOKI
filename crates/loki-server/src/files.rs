use crate::{Error, Result, State, auth};
use axum::{
    Json,
    body::Body,
    extract::{Path as RoutePath, Query, State as ExtractState},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

pub mod api;

pub fn parse_excluded_extensions(input: &str) -> Result<Vec<String>> {
    if input.len() > 4096 {
        return Err(Error::bad(
            "Excluded file extensions must fit within 4,096 bytes.",
        ));
    }
    let mut extensions = std::collections::BTreeSet::new();
    for value in input.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';')) {
        if value.is_empty() {
            continue;
        }
        let value = value
            .strip_prefix("*.")
            .or_else(|| value.strip_prefix('.'))
            .unwrap_or(value);
        if value.is_empty()
            || value.len() > 32
            || value.split('.').any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-+".contains(&c))
            })
        {
            return Err(Error::bad(
                "Enter file extensions such as .tmp, .bak or .tar.gz, separated by commas or spaces.",
            ));
        }
        extensions.insert(value.to_ascii_lowercase());
        if extensions.len() > 64 {
            return Err(Error::bad(
                "Use at most 64 excluded file extensions per share.",
            ));
        }
    }
    Ok(extensions.into_iter().collect())
}

fn excluded(path: &Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return false;
    }
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy().to_ascii_lowercase();
    // Win32 can treat trailing spaces/dots as aliases for the same filename.
    #[cfg(windows)]
    let name = name.trim_end_matches([' ', '.']);
    extensions.iter().any(|extension| {
        name.strip_suffix(extension)
            .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

fn check_opened_file(file: &std::fs::File, path: &Path, extensions: &[String]) -> Result<()> {
    if excluded(path, extensions) {
        return Err(Error::not_found());
    }
    #[cfg(not(windows))]
    let _ = file;
    #[cfg(windows)]
    if !extensions.is_empty() {
        use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_NT,
        };
        // Check the opened handle's long filename as well: an 8.3 alias can
        // have a different extension, especially for compound suffixes.
        let mut buffer = vec![0u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle().cast(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_NT,
            )
        } as usize;
        if length == 0 || length >= buffer.len() {
            return Err(Error::not_found());
        }
        let resolved = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
        if excluded(&resolved, extensions) {
            return Err(Error::not_found());
        }
    }
    Ok(())
}

#[derive(Deserialize, Default)]
pub struct Browse {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub page: usize,
    #[serde(default)]
    pub sort: String,
    #[serde(default)]
    pub download: bool,
}
#[derive(Serialize)]
pub struct Entry {
    pub name: String,
    pub directory: bool,
    pub size: u64,
    pub modified: u64,
}
pub fn relative(value: &str) -> Result<PathBuf> {
    if value.contains(['\\', ':', '\0'])
        || value.starts_with('/')
        || value.split('/').any(|s| matches!(s, ".." | "."))
        || Path::new(value)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(Error::bad("Invalid file path."));
    }
    Ok(PathBuf::from(value))
}
pub(crate) fn confined(root: &str, path: &str) -> Result<(Dir, PathBuf)> {
    let path = relative(path)?;
    let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        let meta = dir
            .symlink_metadata(&prefix)
            .map_err(|_| Error::not_found())?;
        if meta.file_type().is_symlink() {
            return Err(Error::forbidden());
        }
        #[cfg(windows)]
        {
            use cap_std::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err(Error::forbidden());
            }
        }
    }
    Ok((dir, path))
}
pub struct Share {
    pub root: String,
    pub excluded_extensions: Vec<String>,
}
pub async fn share(state: &State, headers: &HeaderMap, id: &str) -> Result<Share> {
    let ctx = auth::context(state, headers).await.ok();
    let user = ctx.as_ref().and_then(|c| c.user.as_ref());
    if !auth::allowed(state, user, id, "access").await? {
        return Err(if user.is_none() {
            Error::unauthorized()
        } else {
            Error::forbidden()
        });
    }
    let (root, extensions): (String, String) =
        sqlx::query_as("SELECT root,excluded_extensions FROM shares WHERE id=?")
            .bind(id)
            .fetch_optional(&state.0.db)
            .await?
            .ok_or_else(Error::not_found)?;
    Ok(Share {
        root,
        excluded_extensions: serde_json::from_str(&extensions)?,
    })
}
pub async fn info(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>> {
    share(&state, &headers, &id).await?;
    let (name, visibility): (String, String) =
        sqlx::query_as("SELECT name,visibility FROM resources WHERE id=? AND kind='share'")
            .bind(&id)
            .fetch_one(&state.0.db)
            .await?;
    Ok(Json(
        serde_json::json!({"id":id,"name":name,"visibility":visibility,"kind":"share"}),
    ))
}
pub async fn list(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(query): Query<Browse>,
) -> Result<Json<serde_json::Value>> {
    let share = share(&state, &headers, &id).await?;
    list_share(share, query).await
}

pub(super) async fn list_share(share: Share, query: Browse) -> Result<Json<serde_json::Value>> {
    let path = query.path.clone();
    let (entries, total) = tokio::task::spawn_blocking(move || -> Result<(Vec<Entry>, usize)> {
        let (dir, path) = confined(&share.root, &path)?;
        let current = if path.as_os_str().is_empty() {
            dir
        } else {
            dir.open_dir(path).map_err(|_| Error::not_found())?
        };
        // Retain only the requested sorted prefix. This bounds memory even for very large folders.
        let limit = query.page.min(999).saturating_add(1) * 100;
        let mut entries = Vec::new();
        let mut total = 0;
        for item in current.entries()? {
            let Ok(item) = item else { continue };
            let name = item.file_name().to_string_lossy().to_string();
            let Ok(meta) = current.symlink_metadata(&name) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            #[cfg(windows)]
            {
                use cap_std::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    continue;
                }
            }
            if !meta.is_dir() && !meta.is_file() {
                continue;
            }
            if meta.is_file() && excluded(Path::new(&name), &share.excluded_extensions) {
                continue;
            }
            total += 1;
            entries.push(Entry {
                name,
                directory: meta.is_dir(),
                size: meta.len(),
                modified: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.into_std().duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            });
            if entries.len() > limit + 256 {
                sort_entries(&mut entries, &query.sort);
                entries.truncate(limit);
            }
        }
        sort_entries(&mut entries, &query.sort);
        entries.truncate(limit);
        let skip = query.page.min(999) * 100;
        Ok((entries.into_iter().skip(skip).collect(), total))
    })
    .await??;
    Ok(Json(
        serde_json::json!({"entries":entries,"total":total,"page":query.page.min(999),"path":query.path}),
    ))
}
fn sort_entries(entries: &mut [Entry], sort: &str) {
    entries.sort_unstable_by(|a, b| {
        b.directory
            .cmp(&a.directory)
            .then_with(|| match sort {
                "size" => b.size.cmp(&a.size),
                "modified" => b.modified.cmp(&a.modified),
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            })
            .then_with(|| a.name.cmp(&b.name))
    });
}
pub fn range(value: &str, length: u64) -> Option<(u64, u64)> {
    if length == 0 {
        return None;
    }
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let suffix: u64 = end.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        return Some((length.saturating_sub(suffix), length - 1));
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().ok()?.min(length - 1)
    };
    if start >= length || end < start {
        return None;
    }
    Some((start, end))
}
pub fn preview_type(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            "avif" => "image/avif",
            "mp3" => "audio/mpeg",
            "ogg" => "audio/ogg",
            "wav" => "audio/wav",
            "flac" => "audio/flac",
            "mp4" => "video/mp4",
            "webm" => "video/webm",
            "pdf" => "application/pdf",
            "txt" | "md" | "rs" | "json" | "toml" | "yaml" | "yml" | "csv" | "log" => {
                "text/plain; charset=utf-8"
            }
            _ => return None,
        }
        .into(),
    )
}
pub async fn download(
    ExtractState(state): ExtractState<State>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(query): Query<Browse>,
) -> Result<Response> {
    let share = share(&state, &headers, &id).await?;
    serve_file(share, &headers, query, false).await
}

pub(super) async fn open_file(share: Share, path: String) -> Result<(std::fs::File, PathBuf)> {
    tokio::task::spawn_blocking(move || -> Result<_> {
        let (dir, path) = confined(&share.root, &path)?;
        if excluded(&path, &share.excluded_extensions) {
            return Err(Error::not_found());
        }
        let file = dir.open(&path).map_err(|_| Error::not_found())?;
        if !file.metadata()?.is_file() {
            return Err(Error::not_found());
        }
        let file = file.into_std();
        check_opened_file(&file, &path, &share.excluded_extensions)?;
        Ok((file, path))
    })
    .await?
}

/// Filesystem timestamps can predate Unix time or exceed HTTP's year range.
/// `httpdate` panics for these, so omit the validator when it cannot be encoded.
fn http_date(time: std::time::SystemTime) -> Option<String> {
    let seconds = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    (seconds < 253402300800).then(|| httpdate::fmt_http_date(time))
}

pub(super) async fn serve_file(
    share: Share,
    headers: &HeaderMap,
    query: Browse,
    head: bool,
) -> Result<Response> {
    let (file, path) = open_file(share, query.path).await?;
    let metadata = file.metadata()?;
    let length = metadata.len();
    let modified = metadata.modified().ok().and_then(http_date);
    let mut file = tokio::fs::File::from_std(file);
    let mut status = StatusCode::OK;
    // A stale validator must restart the transfer, not splice two versions.
    // Weak ETags (and unrecognized validators) never satisfy If-Range.
    let use_range = !head
        && headers.get(header::IF_RANGE).is_none_or(|value| {
            modified
                .as_deref()
                .is_some_and(|date| value.as_bytes() == date.as_bytes())
        });
    let (start, end) = if let Some(value) = headers.get(header::RANGE).filter(|_| use_range) {
        match value.to_str().ok().and_then(|s| range(s, length)) {
            Some(r) => {
                status = StatusCode::PARTIAL_CONTENT;
                r
            }
            None => {
                return Ok((
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{length}"))],
                )
                    .into_response());
            }
        }
    } else {
        (0, length.saturating_sub(1))
    };
    let count = if length == 0 { 0 } else { end - start + 1 };
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let preview = preview_type(&path);
    let attachment = query.download || preview.is_none();
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let encoded = url::form_urlencoded::byte_serialize(name.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    let mut response = Response::builder()
        .status(status)
        .header(
            header::CONTENT_TYPE,
            preview.unwrap_or_else(|| "application/octet-stream".into()),
        )
        .header(header::CONTENT_LENGTH, count)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "{}; filename=\"download\"; filename*=UTF-8''{encoded}",
                if attachment { "attachment" } else { "inline" }
            ),
        )
        .header(
            "Content-Security-Policy",
            "sandbox; default-src 'none'; style-src 'unsafe-inline'; media-src 'self'",
        );
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{length}"),
        );
    }
    if let Some(modified) = modified {
        response = response.header(header::LAST_MODIFIED, modified);
    }
    if head {
        return Ok(response.body(Body::empty())?);
    }
    Ok(response.body(Body::from_stream(ReaderStream::with_capacity(
        file.take(count),
        65536,
    )))?)
}
pub async fn folders(
    ExtractState(state): ExtractState<State>,
    headers: HeaderMap,
    Query(query): Query<Browse>,
) -> Result<Json<serde_json::Value>> {
    auth::context(&state, &headers).await?.owner()?;
    if query.path.is_empty() {
        #[cfg(windows)]
        let roots: Vec<String> = (b'A'..=b'Z')
            .map(|c| format!("{}:\\", c as char))
            .filter(|p| Path::new(p).is_dir())
            .collect();
        #[cfg(not(windows))]
        let roots = vec!["/".to_string()];
        return Ok(Json(serde_json::json!({"path":"","folders":roots})));
    }
    let path = canonical(query.path).await?;
    let display = path.to_string_lossy().to_string();
    let folders = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        let mut dirs = Vec::new();
        for item in std::fs::read_dir(path)? {
            let Ok(item) = item else { continue };
            let Ok(meta) = item.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() && !meta.file_type().is_symlink() {
                dirs.push(item.path().to_string_lossy().to_string());
            }
            if dirs.len() >= 2000 {
                break;
            }
        }
        dirs.sort();
        Ok(dirs)
    })
    .await??;
    Ok(Json(
        serde_json::json!({"path":display,"parent":Path::new(&display).parent().map(|p|p.to_string_lossy().to_string()),"folders":folders}),
    ))
}
/// The absolute path, as people write it. On Windows `canonicalize` returns the
/// `\\?\` form, which still works but reads badly wherever a path is shown or saved.
pub async fn canonical(path: impl AsRef<Path>) -> std::io::Result<std::path::PathBuf> {
    let path = tokio::fs::canonicalize(path).await?;
    Ok(dunce::simplified(&path).to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn exclusions_check_the_actual_opened_filename_not_only_a_short_alias() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("日本語 archive.tar.gz");
        std::fs::write(&path, b"private").unwrap();
        let file = std::fs::File::open(path).unwrap();
        let extensions = parse_excluded_extensions("tar.gz").unwrap();
        assert_eq!(
            check_opened_file(&file, Path::new("ARCHIV~1.GZ"), &extensions)
                .unwrap_err()
                .status,
            StatusCode::NOT_FOUND
        );
        assert!(check_opened_file(&file, Path::new("ARCHIV~1.GZ"), &[]).is_ok());
    }
    #[test]
    fn excluded_extensions_are_normalized_and_match_file_suffixes() {
        let extensions = parse_excluded_extensions(".TMP, bak; *.JPG\n.tar.gz tmp").unwrap();
        assert_eq!(extensions, ["bak", "jpg", "tar.gz", "tmp"]);
        for name in [
            "image.JPG",
            "日本語.Bak",
            "archive.TAR.GZ",
            ".tmp",
            "folder/photo.jpg",
        ] {
            assert!(excluded(Path::new(name), &extensions), "{name}");
        }
        for name in [
            "notmp",
            "photo.jpg.txt",
            "archive.gz",
            "README",
            "folder.tmp/readme.txt",
        ] {
            assert!(!excluded(Path::new(name), &extensions), "{name}");
        }
        assert!(parse_excluded_extensions(" , ; \n").unwrap().is_empty());
        for input in [
            "*",
            "*.*",
            "../txt",
            "a/b",
            "a\\b",
            ".",
            "..tmp",
            "*..tmp",
            "a..b",
            "exe:stream",
            "<script>",
        ] {
            assert!(parse_excluded_extensions(input).is_err(), "{input}");
        }
        assert!(parse_excluded_extensions(&"x".repeat(33)).is_err());
        assert!(parse_excluded_extensions(&" ".repeat(4097)).is_err());
        let too_many = (0..65)
            .map(|n| format!("ext{n}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_excluded_extensions(&too_many).is_err());
        #[cfg(windows)]
        assert!(excluded(Path::new("photo.JPG. "), &extensions));
    }
    #[test]
    fn ranges() {
        assert_eq!(range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(range("bytes=-20", 100), Some((80, 99)));
        assert_eq!(range("bytes=50-", 100), Some((50, 99)));
        for s in ["bytes=100-", "bytes=9-1", "bytes=0-2,4-6", "bytes=-0"] {
            assert!(range(s, 100).is_none());
        }
    }
    #[test]
    fn traversal() {
        for s in [
            "../secret",
            "a/../../x",
            "C:\\secret",
            "/etc/passwd",
            "a\\b",
            "./file",
        ] {
            assert!(relative(s).is_err());
        }
        assert!(relative("日本語/file.txt").is_ok());
    }
    #[test]
    fn active_formats_download() {
        for name in ["evil.html", "evil.svg", "page.xhtml", "script.js"] {
            assert!(preview_type(Path::new(name)).is_none());
        }
    }
    #[cfg(windows)]
    #[test]
    fn junction_escape_is_denied() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let junction = root.path().join("escape");
        std::fs::write(outside.path().join("secret.txt"), b"outside").unwrap();
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(confined(root.path().to_str().unwrap(), "escape/secret.txt").is_err());
        let snapshot = tempfile::tempdir().unwrap();
        assert!(crate::apps::copy_tree(root.path(), &snapshot.path().join("snapshot")).is_err());
        assert!(root.path().starts_with(std::env::temp_dir()));
        drop(root);
        assert_eq!(
            std::fs::read(outside.path().join("secret.txt")).unwrap(),
            b"outside"
        );
    }
    #[cfg(unix)]
    #[test]
    fn symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(confined(root.path().to_str().unwrap(), "escape").is_err());
    }
}
