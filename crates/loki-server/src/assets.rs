//! Dashboard assets, embedded in the binary. Versioned URLs are cached forever;
//! unversioned URLs revalidate by ETag. Text assets are gzipped once on first use.
use axum::{
    body::Body,
    extract::Path,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, io::Write, sync::OnceLock};

const JS: &str = "text/javascript; charset=utf-8";
const FILES: &[(&str, &str, &[u8])] = &[
    ("app.js", JS, include_bytes!("../static/app.js")),
    ("lib/core.js", JS, include_bytes!("../static/lib/core.js")),
    ("lib/icons.js", JS, include_bytes!("../static/lib/icons.js")),
    ("lib/ui.js", JS, include_bytes!("../static/lib/ui.js")),
    (
        "lib/router.js",
        JS,
        include_bytes!("../static/lib/router.js"),
    ),
    (
        "lib/actions.js",
        JS,
        include_bytes!("../static/lib/actions.js"),
    ),
    (
        "pages/auth.js",
        JS,
        include_bytes!("../static/pages/auth.js"),
    ),
    (
        "pages/services.js",
        JS,
        include_bytes!("../static/pages/services.js"),
    ),
    (
        "pages/manage.js",
        JS,
        include_bytes!("../static/pages/manage.js"),
    ),
    (
        "pages/people.js",
        JS,
        include_bytes!("../static/pages/people.js"),
    ),
    (
        "pages/account.js",
        JS,
        include_bytes!("../static/pages/account.js"),
    ),
    (
        "pages/files.js",
        JS,
        include_bytes!("../static/pages/files.js"),
    ),
    (
        "pages/sync.js",
        JS,
        include_bytes!("../static/pages/sync.js"),
    ),
    (
        "pages/panel.js",
        JS,
        include_bytes!("../static/pages/panel.js"),
    ),
    (
        "style.css",
        "text/css; charset=utf-8",
        include_bytes!("../static/style.css"),
    ),
    (
        "icon-32.png",
        "image/png",
        include_bytes!("../static/icon-32.png"),
    ),
    (
        "icon-256.png",
        "image/png",
        include_bytes!("../static/icon-256.png"),
    ),
    (
        "tree.svg",
        "image/svg+xml",
        include_bytes!("../static/tree.svg"),
    ),
];

struct Asset {
    mime: &'static str,
    raw: &'static [u8],
    gzip: Vec<u8>,
    etag: HeaderValue,
}
struct Catalog {
    version: String,
    base: String,
    files: HashMap<&'static str, Asset>,
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut all = Sha256::new();
        let files = FILES
            .iter()
            .map(|&(name, mime, raw)| {
                all.update(name);
                all.update(raw);
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
                encoder.write_all(raw).expect("gzip into memory");
                let gzip = encoder.finish().expect("gzip into memory");
                let etag = format!("\"{}\"", &hex::encode(Sha256::digest(raw))[..20]);
                let etag = HeaderValue::from_str(&etag).expect("hex etag");
                (
                    name,
                    Asset {
                        mime,
                        raw,
                        gzip,
                        etag,
                    },
                )
            })
            .collect();
        let version = hex::encode(all.finalize())[..12].to_string();
        Catalog {
            base: format!("/assets/{version}"),
            version,
            files,
        }
    })
}

/// URL prefix that changes whenever any asset changes, e.g. `/assets/0123456789ab`.
pub fn base() -> &'static str {
    &catalog().base
}
/// JavaScript modules, for `<link rel="modulepreload">` so imports load in parallel.
pub fn modules() -> Vec<&'static str> {
    FILES
        .iter()
        .filter(|(name, mime, _)| *mime == JS && *name != "app.js")
        .map(|(name, ..)| *name)
        .collect()
}

pub async fn serve(Path(path): Path<String>, headers: HeaderMap) -> Response {
    let catalog = catalog();
    let (immutable, name) = match path.split_once('/') {
        Some((version, rest)) if version == catalog.version => (true, rest),
        // A page loaded before an upgrade still finds its files, without caching them.
        Some((version, rest))
            if version.len() == 12 && version.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            (false, rest)
        }
        _ => (false, path.as_str()),
    };
    let Some(asset) = catalog.files.get(name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|tag| tag == asset.etag)
    {
        StatusCode::NOT_MODIFIED.into_response()
    } else if accepts_gzip(&headers) {
        let mut response = Response::new(Body::from(asset.gzip.clone()));
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        response
    } else {
        Response::new(Body::from(asset.raw))
    };
    let out = response.headers_mut();
    out.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
    out.insert(header::ETAG, asset.etag.clone());
    out.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    out.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    response
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',').any(|coding| {
                let mut parts = coding.split(';').map(str::trim);
                parts.next() == Some("gzip") && !parts.any(|p| p == "q=0" || p == "q=0.0")
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_static_file_is_embedded() {
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    let relative = path.strip_prefix(root).unwrap().to_string_lossy();
                    out.push(relative.replace('\\', "/"));
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
        let mut found = Vec::new();
        walk(&root, &root, &mut found);
        found.sort();
        let mut listed: Vec<_> = FILES.iter().map(|(name, ..)| name.to_string()).collect();
        listed.sort();
        assert_eq!(found, listed);
    }

    #[test]
    fn gzip_negotiation_respects_q_zero() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("br, gzip;q=0.8"),
        );
        assert!(accepts_gzip(&headers));
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip;q=0, deflate"),
        );
        assert!(!accepts_gzip(&headers));
    }
}
