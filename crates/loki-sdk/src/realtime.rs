//! Long-lived realtime apps: WebSocket connections held by LOKI, and ranged file reads.
//!
//! An app with the `realtime` capability keeps one instance alive. LOKI calls it one
//! event at a time, so its state is ordinary Rust data:
//!
//! ```ignore
//! use loki_sdk::realtime::{Connection, OpenInfo, RealtimeApp};
//! #[derive(Default)]
//! struct Echo;
//! impl RealtimeApp for Echo {
//!     fn start() -> Self { Echo }
//!     fn message(&mut self, connection: Connection, data: Vec<u8>) {
//!         let _ = connection.send(&data);
//!     }
//! }
//! loki_sdk::export_realtime!(handle, Echo);
//! ```
use crate::{Error, Result, User};
use serde::Deserialize;
use std::io::{Read, Seek, SeekFrom};

#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "realtime-app",
        pub_export_macro: true,
        export_macro_name: "__loki_export_realtime",
        default_bindings_module: "loki_sdk::realtime::bindings",
    });
}

/// Where one of the app's own files lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileSource {
    /// A retained upload (`uploads` capability).
    Upload,
    /// A storage file (`storage` capability).
    Storage,
    /// A file declared in the app package, other than the component itself.
    Asset,
}
#[cfg(target_arch = "wasm32")]
impl From<FileSource> for bindings::loki::app::files::FileSource {
    fn from(source: FileSource) -> Self {
        match source {
            FileSource::Upload => Self::Upload,
            FileSource::Storage => Self::Storage,
            FileSource::Asset => Self::Asset,
        }
    }
}

/// An app's long-lived state. LOKI builds it with [`RealtimeApp::start`] and then
/// calls one method at a time. A failure (a panic, or running out of fuel or time)
/// closes every connection and starts afresh.
pub trait RealtimeApp: Sized {
    /// Builds the state when the instance starts or restarts.
    fn start() -> Self;
    /// A connection opened. Close it here to turn it away.
    fn open(&mut self, connection: Connection, info: OpenInfo) {
        let _ = (connection, info);
    }
    /// A binary (or text) message arrived.
    fn message(&mut self, connection: Connection, data: Vec<u8>);
    /// A connection closed, from either side.
    fn close(&mut self, connection: Connection) {
        let _ = connection;
    }
    /// Runs every `limits.realtime.tick_ms` milliseconds, when set.
    fn tick(&mut self) {}
}

/// Who opened a connection.
#[derive(Clone, Debug, Deserialize)]
pub struct OpenInfo {
    /// The peer's `IP:port`, as LOKI saw it.
    #[serde(default)]
    pub remote: String,
    /// Set only for a same-origin browser signed in to LOKI with access to the app.
    #[serde(default)]
    pub user: Option<User>,
}

/// One WebSocket connection to the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Connection(pub u64);
impl Connection {
    /// Queues one binary message. A connection that falls too far behind is closed.
    pub fn send(self, data: &[u8]) -> Result<()> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(result) = testing::send(self, testing::Sent::Data(data.to_vec())) {
            return result.map(|_| ());
        }
        #[cfg(target_arch = "wasm32")]
        return bindings::loki::app::realtime::send(self.0, data).map_err(Error::bad);
        #[cfg(not(target_arch = "wasm32"))]
        Err(no_host())
    }
    /// Streams one of the app's files as consecutive 64 KiB binary messages, in order
    /// with other sends. Returns the file's size.
    pub fn send_file(self, source: FileSource, path: &str) -> Result<u64> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(result) = testing::send(self, testing::Sent::File(source, path.into())) {
            return result;
        }
        #[cfg(target_arch = "wasm32")]
        return bindings::loki::app::realtime::send_file(self.0, source.into(), path)
            .map_err(Error::bad);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (source, path);
            Err(no_host())
        }
    }
    /// Closes the connection after its queued messages. Use code 1000 or 3000–4999.
    pub fn close(self, code: u16, reason: &str) {
        #[cfg(any(test, feature = "testing"))]
        if testing::send(self, testing::Sent::Close(code, reason.into())).is_some() {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        bindings::loki::app::realtime::close(self.0, code, reason);
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (code, reason);
    }
}

/// Closes every connection and starts the realtime instance afresh, for example after
/// its settings change. Callable from HTTP handlers too.
pub fn restart() -> Result<()> {
    crate::guest::call::<serde_json::Value>("realtime-restart", serde_json::Value::Null).map(|_| ())
}

/// The size of one of the app's files.
pub fn file_size(source: FileSource, path: &str) -> Result<u64> {
    #[cfg(any(test, feature = "testing"))]
    if let Some(result) = testing::file(source, path) {
        return result.map(|data| data.len() as u64);
    }
    #[cfg(target_arch = "wasm32")]
    return bindings::loki::app::files::size(source.into(), path).map_err(Error::bad);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (source, path);
        Err(no_host())
    }
}
/// Reads up to `length` bytes (at most 1 MiB) of one of the app's files from `offset`.
pub fn read_file(source: FileSource, path: &str, offset: u64, length: u32) -> Result<Vec<u8>> {
    #[cfg(any(test, feature = "testing"))]
    if let Some(result) = testing::file(source, path) {
        return result.map(|data| {
            let start = (offset as usize).min(data.len());
            let end = (start + length.min(1024 * 1024) as usize).min(data.len());
            data[start..end].to_vec()
        });
    }
    #[cfg(target_arch = "wasm32")]
    return bindings::loki::app::files::read(source.into(), path, offset, length)
        .map_err(Error::bad);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (source, path, offset, length);
        Err(no_host())
    }
}
#[cfg(not(target_arch = "wasm32"))]
fn no_host() -> Error {
    Error::internal(
        "LOKI services require a WASI host. Use loki_sdk::testing for native unit tests.",
    )
}

/// Reads one of the app's files on demand, so large files never sit in memory.
/// It implements `Read` and `Seek`, which is enough for formats like ZIP.
pub struct FileReader {
    source: FileSource,
    path: String,
    size: u64,
    position: u64,
}
impl FileReader {
    pub fn open(source: FileSource, path: &str) -> Result<Self> {
        Ok(Self {
            source,
            path: path.into(),
            size: file_size(source, path)?,
            position: 0,
        })
    }
    pub fn size(&self) -> u64 {
        self.size
    }
}
impl Read for FileReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.size || buffer.is_empty() {
            return Ok(0);
        }
        let length = buffer.len().min(1024 * 1024) as u32;
        let data = read_file(self.source, &self.path, self.position, length)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        buffer[..data.len()].copy_from_slice(&data);
        self.position += data.len() as u64;
        Ok(data.len())
    }
}
impl Seek for FileReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let next = match position {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(n) => self.size.checked_add_signed(n),
            SeekFrom::Current(n) => self.position.checked_add_signed(n),
        };
        self.position = next.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Seek before the start of the file.",
            )
        })?;
        Ok(self.position)
    }
}

#[doc(hidden)]
pub mod __private {
    use super::*;
    use std::{cell::RefCell, thread::LocalKey};
    pub type Slot<A> = RefCell<Option<A>>;
    pub fn start<A: RealtimeApp>(slot: &'static LocalKey<Slot<A>>) {
        let app = A::start();
        slot.with(|s| *s.borrow_mut() = Some(app));
    }
    pub fn with<A: RealtimeApp>(slot: &'static LocalKey<Slot<A>>, f: impl FnOnce(&mut A)) {
        slot.with(|s| {
            if let Some(app) = s.borrow_mut().as_mut() {
                f(app);
            }
        });
    }
    pub fn info(info: &str) -> OpenInfo {
        serde_json::from_str(info).unwrap_or(OpenInfo {
            remote: String::new(),
            user: None,
        })
    }
}

/// Export an HTTP handler and a [`RealtimeApp`] together, as one component.
#[macro_export]
macro_rules! export_realtime {
    ($handler:path, $app:ty) => {
        $crate::export!($handler);
        ::std::thread_local! {
            static __LOKI_REALTIME: $crate::realtime::__private::Slot<$app> =
                const { ::core::cell::RefCell::new(::core::option::Option::None) };
        }
        struct __LokiRealtime;
        impl $crate::realtime::bindings::exports::loki::app::realtime_handler::Guest
            for __LokiRealtime
        {
            fn on_start() {
                $crate::realtime::__private::start(&__LOKI_REALTIME);
            }
            fn on_open(connection: u64, info: ::std::string::String) {
                let info = $crate::realtime::__private::info(&info);
                $crate::realtime::__private::with(&__LOKI_REALTIME, |app: &mut $app| {
                    $crate::realtime::RealtimeApp::open(
                        app,
                        $crate::realtime::Connection(connection),
                        info,
                    )
                });
            }
            fn on_message(connection: u64, data: ::std::vec::Vec<u8>) {
                $crate::realtime::__private::with(&__LOKI_REALTIME, |app: &mut $app| {
                    $crate::realtime::RealtimeApp::message(
                        app,
                        $crate::realtime::Connection(connection),
                        data,
                    )
                });
            }
            fn on_close(connection: u64) {
                $crate::realtime::__private::with(&__LOKI_REALTIME, |app: &mut $app| {
                    $crate::realtime::RealtimeApp::close(
                        app,
                        $crate::realtime::Connection(connection),
                    )
                });
            }
            fn on_tick() {
                $crate::realtime::__private::with(&__LOKI_REALTIME, |app: &mut $app| {
                    $crate::realtime::RealtimeApp::tick(app)
                });
            }
        }
        $crate::realtime::bindings::__loki_export_realtime!(__LokiRealtime with_types_in $crate::realtime::bindings);
    };
}

/// A fake realtime host for native unit tests.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use std::{cell::RefCell, collections::HashMap};

    /// Something the app asked LOKI to send.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Sent {
        Data(Vec<u8>),
        File(FileSource, String),
        Close(u16, String),
    }
    #[derive(Default)]
    struct Host {
        files: HashMap<(FileSource, String), Vec<u8>>,
        sent: Vec<(Connection, Sent)>,
    }
    thread_local! {static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };}

    /// Runs a test against a fake host holding `files`, and returns what was sent.
    pub fn with_host<T>(
        files: impl IntoIterator<Item = (FileSource, String, Vec<u8>)>,
        test: impl FnOnce() -> T,
    ) -> (T, Vec<(Connection, Sent)>) {
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                HOST.with(|h| *h.borrow_mut() = None);
            }
        }
        let host = Host {
            files: files.into_iter().map(|(s, p, d)| ((s, p), d)).collect(),
            sent: Vec::new(),
        };
        HOST.with(|h| *h.borrow_mut() = Some(host));
        let _restore = Restore;
        let result = test();
        let sent = HOST.with(|h| {
            h.borrow_mut()
                .as_mut()
                .map(|h| std::mem::take(&mut h.sent))
                .unwrap_or_default()
        });
        (result, sent)
    }
    /// What the app has sent so far in this test, clearing the record.
    pub fn take_sent() -> Vec<(Connection, Sent)> {
        HOST.with(|h| {
            h.borrow_mut()
                .as_mut()
                .map(|h| std::mem::take(&mut h.sent))
                .unwrap_or_default()
        })
    }
    pub(super) fn send(connection: Connection, item: Sent) -> Option<Result<u64>> {
        HOST.with(|h| {
            let mut host = h.borrow_mut();
            let host = host.as_mut()?;
            let size = match &item {
                Sent::File(source, path) => match host.files.get(&(*source, path.clone())) {
                    Some(data) => data.len() as u64,
                    None => return Some(Err(Error::new(404, "Not found."))),
                },
                _ => 0,
            };
            host.sent.push((connection, item));
            Some(Ok(size))
        })
    }
    pub(super) fn file(source: FileSource, path: &str) -> Option<Result<Vec<u8>>> {
        HOST.with(|h| {
            let host = h.borrow();
            let host = host.as_ref()?;
            Some(
                host.files
                    .get(&(source, path.to_string()))
                    .cloned()
                    .ok_or_else(|| Error::new(404, "Not found.")),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testing::Sent;
    #[test]
    fn file_reader_reads_and_seeks_in_ranges() {
        let data: Vec<u8> = (0..=255).cycle().take(3 * 1024 * 1024 + 7).collect();
        let ((), _) = testing::with_host(
            [(FileSource::Asset, "big.bin".into(), data.clone())],
            || {
                let mut reader = FileReader::open(FileSource::Asset, "big.bin").unwrap();
                assert_eq!(reader.size(), data.len() as u64);
                let mut all = Vec::new();
                reader.read_to_end(&mut all).unwrap();
                assert_eq!(all, data);
                reader.seek(SeekFrom::End(-7)).unwrap();
                let mut tail = Vec::new();
                reader.read_to_end(&mut tail).unwrap();
                assert_eq!(tail, data[data.len() - 7..]);
                assert!(reader.seek(SeekFrom::Current(-100_000_000)).is_err());
            },
        );
    }
    #[test]
    fn connections_record_what_they_send() {
        let ((), sent) = testing::with_host(
            [(FileSource::Upload, "a.dream".into(), vec![1; 10])],
            || {
                let c = Connection(7);
                c.send(b"hi").unwrap();
                assert_eq!(c.send_file(FileSource::Upload, "a.dream").unwrap(), 10);
                assert!(c.send_file(FileSource::Upload, "missing").is_err());
                c.close(1000, "bye");
            },
        );
        assert_eq!(
            sent,
            vec![
                (Connection(7), Sent::Data(b"hi".to_vec())),
                (
                    Connection(7),
                    Sent::File(FileSource::Upload, "a.dream".into())
                ),
                (Connection(7), Sent::Close(1000, "bye".into())),
            ]
        );
    }
}
