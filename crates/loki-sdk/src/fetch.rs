//! Outgoing HTTP(S) requests, for apps with the `outbound` capability. LOKI sends
//! them only to the hosts listed under `outbound` in the app's metadata:
//!
//! ```toml
//! [package.metadata.loki]
//! capabilities = ["outbound"]
//! outbound = ["api.example.com", "*.discord.com"]
//! ```
//!
//! ```ignore
//! let response = loki_sdk::fetch::get("https://api.example.com/servers")?;
//! let servers: Vec<Server> = response.json()?;
//! ```
use crate::{Error, Result};
use serde::{Serialize, de::DeserializeOwned};

/// The largest response body [`send`] reads.
pub const RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// A request to send with [`send`].
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl Outgoing {
    pub fn new(method: &str, url: &str) -> Self {
        Self {
            method: method.to_ascii_uppercase(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
    /// A JSON body, with its content type.
    pub fn json(self, value: &impl Serialize) -> Result<Self> {
        Ok(self
            .header("content-type", "application/json")
            .body(serde_json::to_vec(value)?))
    }
}
/// What the server answered.
#[derive(Clone, Debug)]
pub struct Fetched {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl Fetched {
    /// The first header with this name, ignoring case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn text(&self) -> Result<String> {
        String::from_utf8(self.body.clone()).map_err(|_| Error::bad("The response is not UTF-8 text."))
    }
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(&self.body)?)
    }
    /// Turns a status other than 2xx into an error.
    pub fn ok(self) -> Result<Self> {
        if (200..300).contains(&self.status) {
            Ok(self)
        } else {
            Err(Error::new(
                502,
                format!("The server answered {}.", self.status),
            ))
        }
    }
}

pub fn get(url: &str) -> Result<Fetched> {
    send(Outgoing::new("GET", url))
}
pub fn post_json(url: &str, value: &impl Serialize) -> Result<Fetched> {
    send(Outgoing::new("POST", url).json(value)?)
}

/// Sends the request and waits for the whole response. A host the app may not reach
/// fails with 403.
pub fn send(request: Outgoing) -> Result<Fetched> {
    #[cfg(any(test, feature = "testing"))]
    if let Some(result) = testing::send(&request) {
        return result;
    }
    #[cfg(target_arch = "wasm32")]
    return wasi_send(request);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = request;
        Err(Error::internal(
            "LOKI services require a WASI host. Use loki_sdk::fetch::testing for native unit tests.",
        ))
    }
}

#[cfg(target_arch = "wasm32")]
fn wasi_send(request: Outgoing) -> Result<Fetched> {
    use wasi::http::{
        outgoing_handler,
        types::{ErrorCode, Fields, Method, OutgoingBody, OutgoingRequest, Scheme},
    };
    use wasi::io::streams::StreamError;
    let (scheme, rest) = request
        .url
        .split_once("://")
        .ok_or_else(|| Error::bad("Use an absolute http:// or https:// URL."))?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        _ => return Err(Error::bad("Use an http:// or https:// URL.")),
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    let method = match request.method.as_str() {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "DELETE" => Method::Delete,
        "PATCH" => Method::Patch,
        "OPTIONS" => Method::Options,
        other => Method::Other(other.into()),
    };
    let headers: Vec<(String, Vec<u8>)> = request
        .headers
        .iter()
        .map(|(n, v)| (n.to_ascii_lowercase(), v.as_bytes().to_vec()))
        .collect();
    let fields = Fields::from_list(&headers).map_err(|_| Error::bad("Invalid request header."))?;
    let outgoing = OutgoingRequest::new(fields);
    let invalid = |_| Error::bad("Invalid request URL.");
    outgoing.set_method(&method).map_err(|()| Error::bad("Invalid request method."))?;
    outgoing.set_scheme(Some(&scheme)).map_err(invalid)?;
    outgoing.set_authority(Some(authority)).map_err(invalid)?;
    outgoing.set_path_with_query(Some(&path)).map_err(invalid)?;
    let body = outgoing.body().map_err(|()| Error::internal("No request body."))?;
    let future = outgoing_handler::handle(outgoing, None).map_err(|e| failed(&e))?;
    {
        let stream = body.write().map_err(|()| Error::internal("No request body."))?;
        for chunk in request.body.chunks(4096) {
            stream
                .blocking_write_and_flush(chunk)
                .map_err(|_| Error::new(502, "The request body could not be sent."))?;
        }
    }
    OutgoingBody::finish(body, None).map_err(|e| failed(&e))?;
    future.subscribe().block();
    let response = future
        .get()
        .ok_or_else(|| Error::internal("No response."))?
        .map_err(|()| Error::internal("The response was already taken."))?
        .map_err(|e: ErrorCode| failed(&e))?;
    let status = response.status();
    let headers = response
        .headers()
        .entries()
        .into_iter()
        .map(|(n, v)| (n, String::from_utf8_lossy(&v).into_owned()))
        .collect();
    let incoming = response
        .consume()
        .map_err(|()| Error::internal("No response body."))?;
    let mut bytes = Vec::new();
    {
        let stream = incoming
            .stream()
            .map_err(|()| Error::internal("No response body."))?;
        loop {
            match stream.blocking_read(65536) {
                Ok(chunk) => {
                    bytes.extend_from_slice(&chunk);
                    if bytes.len() > RESPONSE_BYTES {
                        return Err(Error::new(413, "The response is larger than 16 MiB."));
                    }
                }
                Err(StreamError::Closed) => break,
                Err(_) => return Err(Error::new(502, "The response body could not be read.")),
            }
        }
    }
    Ok(Fetched {
        status,
        headers,
        body: bytes,
    })
}
#[cfg(target_arch = "wasm32")]
fn failed(error: &wasi::http::types::ErrorCode) -> Error {
    use wasi::http::types::ErrorCode;
    match error {
        ErrorCode::HttpRequestDenied => Error::new(
            403,
            "LOKI did not allow this request; list the host under outbound.",
        ),
        ErrorCode::ConnectionTimeout | ErrorCode::ConnectionReadTimeout | ErrorCode::DnsTimeout => {
            Error::new(504, "The server did not answer in time.")
        }
        other => Error::new(502, format!("The request failed: {other:?}")),
    }
}

/// A fake network for native unit tests.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use std::cell::RefCell;
    type Fake = Box<dyn FnMut(&Outgoing) -> Result<Fetched>>;
    thread_local! {static NETWORK: RefCell<Option<Fake>> = RefCell::new(None);}
    /// Answers every [`send`] in `test` with `network`.
    pub fn with_network<T>(
        network: impl FnMut(&Outgoing) -> Result<Fetched> + 'static,
        test: impl FnOnce() -> T,
    ) -> T {
        struct Restore(Option<Fake>);
        impl Drop for Restore {
            fn drop(&mut self) {
                NETWORK.with(|n| *n.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(NETWORK.with(|n| n.borrow_mut().replace(Box::new(network))));
        test()
    }
    pub(super) fn send(request: &Outgoing) -> Option<Result<Fetched>> {
        NETWORK.with(|n| n.borrow_mut().as_mut().map(|f| f(request)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requests_go_to_the_fake_network() {
        let fetched = testing::with_network(
            |request| {
                assert_eq!(request.method, "POST");
                assert_eq!(request.url, "https://api.example.com/x");
                assert_eq!(request.body, br#"{"a":1}"#);
                Ok(Fetched {
                    status: 201,
                    headers: vec![("Content-Type".into(), "text/plain".into())],
                    body: b"made".to_vec(),
                })
            },
            || post_json("https://api.example.com/x", &serde_json::json!({"a":1})),
        )
        .unwrap();
        assert_eq!(fetched.header("content-type"), Some("text/plain"));
        assert_eq!(fetched.clone().ok().unwrap().text().unwrap(), "made");
        assert!(
            Fetched {
                status: 404,
                ..fetched
            }
            .ok()
            .is_err()
        );
    }
}
