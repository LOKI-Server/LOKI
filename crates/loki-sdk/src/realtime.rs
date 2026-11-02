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
//!
//! With `limits.realtime.rooms` set, each room (`{path}/{room}`) is a separate
//! instance of the same type, with its own memory and connections, running in
//! parallel with the others. [`room`] says which one is running.
use crate::{Error, Result, User};
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
impl From<FileSource> for bindings::loki::app::types::FileSource {
    fn from(source: FileSource) -> Self {
        match source {
            FileSource::Upload => Self::Upload,
            FileSource::Storage => Self::Storage,
            FileSource::Asset => Self::Asset,
        }
    }
}
#[cfg(target_arch = "wasm32")]
impl From<FileSource> for crate::guest::bindings::loki::app::types::FileSource {
    fn from(source: FileSource) -> Self {
        match source {
            FileSource::Upload => Self::Upload,
            FileSource::Storage => Self::Storage,
            FileSource::Asset => Self::Asset,
        }
    }
}
#[cfg(target_arch = "wasm32")]
impl From<bindings::loki::app::types::Error> for Error {
    fn from(error: bindings::loki::app::types::Error) -> Self {
        Error::new(error.status, error.message)
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
    /// Several messages arrived together, in order. Handles each with
    /// [`RealtimeApp::message`] unless overridden.
    fn messages(&mut self, messages: Vec<(Connection, Vec<u8>)>) {
        for (connection, data) in messages {
            self.message(connection, data);
        }
    }
    /// A timer set with [`set_timer`] ran out.
    fn timer(&mut self, id: u64) {
        let _ = id;
    }
    /// The app's HTTP side, or another instance, asked this one with [`call`]. The
    /// answer goes back to the caller.
    fn call(&mut self, data: Vec<u8>) -> Vec<u8> {
        let _ = data;
        Vec::new()
    }
    /// Another room sent this one data with [`post`]. `from` is empty for the main
    /// instance.
    fn post(&mut self, from: String, data: Vec<u8>) {
        let _ = (from, data);
    }
}

/// One running instance of a realtime app, as [`rooms`] lists it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoomInfo {
    /// The room; empty for the main instance.
    pub id: String,
    pub connections: u32,
}

/// Who opened a connection.
#[derive(Clone, Debug)]
pub struct OpenInfo {
    /// The peer's `IP:port`, as LOKI saw it.
    pub remote: String,
    /// Set only for a same-origin browser signed in to LOKI with access to the app.
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
        return bindings::loki::app::realtime::send(self.0, data).map_err(Into::into);
        #[cfg(not(target_arch = "wasm32"))]
        Err(no_host())
    }
    /// Queues one text message.
    pub fn send_text(self, text: &str) -> Result<()> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(result) = testing::send(self, testing::Sent::Text(text.into())) {
            return result.map(|_| ());
        }
        #[cfg(target_arch = "wasm32")]
        return bindings::loki::app::realtime::send_text(self.0, text).map_err(Into::into);
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
            .map_err(Into::into);
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
/// its settings change. Rooms close. Callable from HTTP handlers too.
pub fn restart() -> Result<()> {
    crate::guest::host::restart_realtime()
}

/// Queues the same binary message for each of these connections that is open in
/// this instance, copying it once. Returns how many it was queued for.
pub fn broadcast(connections: impl IntoIterator<Item = Connection>, data: &[u8]) -> u32 {
    let ids: Vec<u64> = connections.into_iter().map(|c| c.0).collect();
    #[cfg(any(test, feature = "testing"))]
    if let Some(sent) = testing::broadcast(&ids, data) {
        return sent;
    }
    #[cfg(target_arch = "wasm32")]
    return bindings::loki::app::realtime::broadcast(&ids, data);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (ids, data);
        0
    }
}
/// Calls [`RealtimeApp::timer`] with `id` after `delay`, replacing a waiting timer with
/// the same id. At most 256 wait at once. Timers end with the instance.
pub fn set_timer(id: u64, delay: std::time::Duration) -> Result<()> {
    let ms = u32::try_from(delay.as_millis()).unwrap_or(u32::MAX);
    #[cfg(any(test, feature = "testing"))]
    if let Some(result) = testing::timer(id, Some(ms)) {
        return result;
    }
    #[cfg(target_arch = "wasm32")]
    return bindings::loki::app::realtime::set_timer(id, ms).map_err(Into::into);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = ms;
        Err(no_host())
    }
}
pub fn cancel_timer(id: u64) {
    #[cfg(any(test, feature = "testing"))]
    if testing::timer(id, None).is_some() {
        return;
    }
    #[cfg(target_arch = "wasm32")]
    bindings::loki::app::realtime::cancel_timer(id);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = id;
}
/// The room this instance serves; empty for the main instance.
pub fn room() -> String {
    #[cfg(any(test, feature = "testing"))]
    if let Some(room) = testing::room() {
        return room;
    }
    #[cfg(target_arch = "wasm32")]
    return bindings::loki::app::realtime::room();
    #[cfg(not(target_arch = "wasm32"))]
    String::new()
}
/// Starts a room, if it isn't running. Rooms also start when someone connects to
/// `{path}/{room}`, and close once idle.
pub fn open_room(id: &str) -> Result<()> {
    #[cfg(target_arch = "wasm32")]
    return bindings::loki::app::realtime::open_room(id).map_err(Into::into);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = id;
        Err(no_host())
    }
}
/// Closes a room's connections and ends it.
pub fn close_room(id: &str) -> Result<()> {
    #[cfg(target_arch = "wasm32")]
    return bindings::loki::app::realtime::close_room(id).map_err(Into::into);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = id;
        Err(no_host())
    }
}
/// Hands `data` to another running room's [`RealtimeApp::post`]; `""` is the main
/// instance. Only realtime instances can post.
pub fn post(room: &str, data: &[u8]) -> Result<()> {
    #[cfg(any(test, feature = "testing"))]
    if let Some(result) = testing::post(room, data) {
        return result;
    }
    #[cfg(target_arch = "wasm32")]
    return bindings::loki::app::realtime::post(room, data).map_err(Into::into);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (room, data);
        Err(no_host())
    }
}
/// Asks a running instance (`""` for the main one) through its [`RealtimeApp::call`]
/// and returns its answer, for example to show live state in a page or panel.
/// Callable from HTTP handlers and from other rooms.
pub fn call(room: &str, data: &[u8]) -> Result<Vec<u8>> {
    crate::guest::host::call_realtime(room, data)
}
/// The running instances, sorted by room.
pub fn rooms() -> Result<Vec<RoomInfo>> {
    crate::guest::host::list_rooms()
}

/// The size of one of the app's files.
pub fn file_size(source: FileSource, path: &str) -> Result<u64> {
    #[cfg(any(test, feature = "testing"))]
    if let Some(result) = testing::file(source, path) {
        return result.map(|data| data.len() as u64);
    }
    #[cfg(target_arch = "wasm32")]
    return crate::guest::host::file_size(source.into(), path);
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
    return crate::guest::host::read_range(source.into(), path, offset, length);
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
    pub fn info(peer: bindings::exports::loki::app::realtime_handler::Peer) -> OpenInfo {
        OpenInfo {
            remote: peer.remote,
            user: peer.user.map(|user| User {
                id: user.id,
                username: user.username,
                name: user.name,
                email: user.email,
                bio: user.bio,
                avatar: user.avatar,
                roles: user.roles,
            }),
        }
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
            fn on_open(
                connection: u64,
                peer: $crate::realtime::bindings::exports::loki::app::realtime_handler::Peer,
            ) {
                let info = $crate::realtime::__private::info(peer);
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
        impl $crate::realtime::bindings::exports::loki::app::realtime_events::Guest
            for __LokiRealtime
        {
            fn on_call(data: ::std::vec::Vec<u8>) -> ::std::vec::Vec<u8> {
                let mut answer = ::std::vec::Vec::new();
                $crate::realtime::__private::with(&__LOKI_REALTIME, |app: &mut $app| {
                    answer = $crate::realtime::RealtimeApp::call(app, data)
                });
                answer
            }
            fn on_timer(id: u64) {
                $crate::realtime::__private::with(&__LOKI_REALTIME, |app: &mut $app| {
                    $crate::realtime::RealtimeApp::timer(app, id)
                });
            }
            fn on_post(from: ::std::string::String, data: ::std::vec::Vec<u8>) {
                $crate::realtime::__private::with(&__LOKI_REALTIME, |app: &mut $app| {
                    $crate::realtime::RealtimeApp::post(app, from, data)
                });
            }
            fn on_messages(messages: ::std::vec::Vec<(u64, ::std::vec::Vec<u8>)>) {
                let messages = messages
                    .into_iter()
                    .map(|(c, d)| ($crate::realtime::Connection(c), d))
                    .collect();
                $crate::realtime::__private::with(&__LOKI_REALTIME, |app: &mut $app| {
                    $crate::realtime::RealtimeApp::messages(app, messages)
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
        Text(String),
        File(FileSource, String),
        Close(u16, String),
    }
    #[derive(Default)]
    struct Host {
        files: HashMap<(FileSource, String), Vec<u8>>,
        sent: Vec<(Connection, Sent)>,
        timers: Vec<(u64, Option<u32>)>,
        posts: Vec<(String, Vec<u8>)>,
        room: String,
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
            ..Host::default()
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
    /// Pretends the code under test runs in `room`, for the rest of this test.
    pub fn set_room(room: &str) {
        HOST.with(|h| {
            if let Some(host) = h.borrow_mut().as_mut() {
                host.room = room.into();
            }
        });
    }
    /// Timers set (with their delay in milliseconds) or cancelled so far, clearing the record.
    pub fn take_timers() -> Vec<(u64, Option<u32>)> {
        HOST.with(|h| {
            h.borrow_mut()
                .as_mut()
                .map(|h| std::mem::take(&mut h.timers))
                .unwrap_or_default()
        })
    }
    /// Data posted to other rooms so far, clearing the record.
    pub fn take_posts() -> Vec<(String, Vec<u8>)> {
        HOST.with(|h| {
            h.borrow_mut()
                .as_mut()
                .map(|h| std::mem::take(&mut h.posts))
                .unwrap_or_default()
        })
    }
    pub(super) fn broadcast(ids: &[u64], data: &[u8]) -> Option<u32> {
        HOST.with(|h| {
            let mut host = h.borrow_mut();
            let host = host.as_mut()?;
            for id in ids {
                host.sent.push((Connection(*id), Sent::Data(data.to_vec())));
            }
            Some(ids.len() as u32)
        })
    }
    pub(super) fn timer(id: u64, delay: Option<u32>) -> Option<Result<()>> {
        HOST.with(|h| {
            let mut host = h.borrow_mut();
            host.as_mut()?.timers.push((id, delay));
            Some(Ok(()))
        })
    }
    pub(super) fn post(room: &str, data: &[u8]) -> Option<Result<()>> {
        HOST.with(|h| {
            let mut host = h.borrow_mut();
            host.as_mut()?.posts.push((room.into(), data.to_vec()));
            Some(Ok(()))
        })
    }
    pub(super) fn room() -> Option<String> {
        HOST.with(|h| h.borrow().as_ref().map(|h| h.room.clone()))
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
