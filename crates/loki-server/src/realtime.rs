//! Long-lived instances for WASI apps with the `realtime` capability.
//!
//! Each such app gets a main instance that LOKI keeps alive, so its memory persists
//! between calls, and optionally rooms: more instances of the same app, each with its
//! own memory and connections, running in parallel. LOKI holds every WebSocket
//! connection and hands each instance one event at a time: a connection opened,
//! messages arrived, a connection closed, a tick or timer, a call from the app's HTTP
//! side, or a post from another room. Instances answer through the `realtime` host
//! interface.
use crate::{
    Error, Result, State, apps,
    components::{self, ConnectionPeer, FileSource, Host, Runtime, WitError},
};
use axum::{
    body::Body,
    extract::{
        ConnectInfo, FromRequestParts,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::time::{DelayQueue, delay_queue};
use wasmtime::{
    Store,
    component::{ComponentNamedList, ComponentType, Lift, Linker, Lower, TypedFunc},
};

/// Bytes a connection may have waiting to be sent before it counts as fallen behind.
const QUEUE_LIMIT: usize = 4 * 1024 * 1024;
/// The longest one event may run, apart from `on-start`.
const EVENT_TIME: Duration = Duration::from_secs(5);
/// Restarts after a failure wait this long at first, doubling up to `MAX_BACKOFF`.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Timers one instance may have waiting.
const MAX_TIMERS: usize = 256;
/// Messages handed to an instance in one `on-messages` call.
const BATCH: usize = 64;
/// The main instance's room name.
const MAIN: &str = "";
const NOT_REALTIME: &str = "Only the app's realtime instance can use connections.";
/// The host interfaces' names, newest first. Apps built with an older SDK export the
/// older names; LOKI accepts every version it supports.
const HANDLER: [&str; 2] = [
    "loki:app/realtime-handler@0.3.1",
    "loki:app/realtime-handler@0.3.0",
];
const EVENTS: &str = "loki:app/realtime-events@0.3.1";

/// What `list-rooms` says about one room. Matches `room-info` in the WIT's `types`.
#[derive(Clone, Debug, ComponentType, Lower)]
#[component(record)]
pub(crate) struct RoomInfo {
    id: String,
    connections: u32,
}

/// The realtime side of one app: its main instance and its rooms.
pub struct Handle {
    runtime: Weak<Runtime>,
    issuer: String,
    rooms: std::sync::Mutex<HashMap<String, Arc<Room>>>,
    /// Held while a room starts, so two connections never start the same room twice.
    starting: tokio::sync::Mutex<()>,
    next: AtomicU64,
    /// Open connections across every room.
    count: AtomicUsize,
    max_connections: usize,
    max_message_bytes: usize,
    room_limits: Option<loki_sdk::manifest::Rooms>,
    lan: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}
/// One running instance: the main one, or a room.
struct Room {
    events: mpsc::Sender<Event>,
    instance: Arc<Instance>,
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}
enum Event {
    Open(u64, ConnectionPeer),
    Message(u64, Vec<u8>),
    Close(u64),
    Call(Vec<u8>, oneshot::Sender<Option<Vec<u8>>>),
    Post(String, Vec<u8>),
    Restart,
    /// Closes every connection with this code and ends the instance.
    Stop(u16, &'static str),
}
impl Handle {
    /// Closes every connection and ends every instance. Waits for them to finish.
    pub async fn stop(&self) {
        if let Some(lan) = self.lan.lock().unwrap().take() {
            let _ = lan.send(());
        }
        let rooms: Vec<_> = self.rooms.lock().unwrap().drain().map(|(_, r)| r).collect();
        for room in &rooms {
            let _ = room.events.send(Event::Stop(1001, "The app stopped.")).await;
        }
        for room in rooms {
            if let Some(task) = room.task.lock().await.take() {
                let _ = task.await;
            }
        }
    }
    /// Closes every connection and starts the main instance afresh. Rooms close; the
    /// next connection to one starts it again.
    pub async fn restart(&self) {
        let rooms: Vec<_> = self.rooms.lock().unwrap().values().cloned().collect();
        for room in rooms {
            let event = if room.instance.room == MAIN {
                Event::Restart
            } else {
                Event::Stop(1012, "The app restarted.")
            };
            let _ = room.events.send(event).await;
        }
    }
    fn room(&self, id: &str) -> Option<Arc<Room>> {
        self.rooms.lock().unwrap().get(id).cloned()
    }
    /// The room, started if it isn't running yet.
    async fn open_room(self: &Arc<Self>, id: &str) -> Result<Arc<Room>> {
        if let Some(room) = self.room(id) {
            return Ok(room);
        }
        let limits = self
            .room_limits
            .clone()
            .ok_or_else(Error::not_found)?;
        if !apps::valid_id(id) {
            return Err(Error::bad(
                "Room names are 1–64 lowercase letters, digits and hyphens.",
            ));
        }
        let _starting = self.starting.lock().await;
        if let Some(room) = self.room(id) {
            return Ok(room);
        }
        if self.rooms.lock().unwrap().len() > limits.max {
            return Err(busy("This app has as many rooms open as it may."));
        }
        let runtime = self
            .runtime
            .upgrade()
            .ok_or_else(|| busy("The app is stopped."))?;
        let room = self
            .launch(runtime, id, Some(Duration::from_secs(limits.idle_seconds)))
            .await
            .map_err(|e| {
                Error::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("The room could not start: {e:#}"),
                )
            })?;
        Ok(room)
    }
    /// Starts an instance for `id` and records it. Rooms other than the main one end
    /// after `idle` with no connections.
    async fn launch(
        self: &Arc<Self>,
        runtime: Arc<Runtime>,
        id: &str,
        idle: Option<Duration>,
    ) -> anyhow::Result<Arc<Room>> {
        let instance = Arc::new(Instance {
            room: id.to_string(),
            peers: std::sync::Mutex::new(HashMap::new()),
            timers: std::sync::Mutex::new(Timers::default()),
            app: Arc::downgrade(self),
        });
        let live = Live::start(&runtime, &instance, &self.issuer).await?;
        let (events, receiver) = mpsc::channel(1024);
        let room = Arc::new(Room {
            events,
            instance: instance.clone(),
            task: tokio::sync::Mutex::new(None),
        });
        *room.task.lock().await = Some(tokio::spawn(run(
            runtime,
            Arc::downgrade(self),
            instance,
            receiver,
            live,
            self.issuer.clone(),
            idle,
        )));
        self.rooms
            .lock()
            .unwrap()
            .insert(id.to_string(), room.clone());
        Ok(room)
    }
    /// Ends a room other than the main one.
    async fn close_room(&self, id: &str) -> Result<()> {
        if id == MAIN {
            return Err(Error::bad("The main instance is not a room."));
        }
        let room = self
            .rooms
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(Error::not_found)?;
        let _ = room.events.send(Event::Stop(1000, "The room closed.")).await;
        Ok(())
    }
    /// Asks a room's instance through its `on-call` export and returns its answer.
    pub(crate) async fn call(&self, room: &str, data: Vec<u8>) -> Result<Vec<u8>> {
        let room = self.room(room).ok_or_else(Error::not_found)?;
        let (reply, answer) = oneshot::channel();
        room.events
            .send(Event::Call(data, reply))
            .await
            .map_err(|_| busy("The realtime instance is not running."))?;
        tokio::time::timeout(EVENT_TIME * 2, answer)
            .await
            .map_err(|_| busy("The realtime instance did not answer in time."))?
            .ok()
            .flatten()
            .ok_or_else(|| {
                busy("The realtime instance could not answer; it may not take calls.")
            })
    }
    pub(crate) fn list(&self) -> Vec<RoomInfo> {
        let mut rooms: Vec<_> = self
            .rooms
            .lock()
            .unwrap()
            .values()
            .map(|r| RoomInfo {
                id: r.instance.room.clone(),
                connections: r.instance.peers.lock().unwrap().len() as u32,
            })
            .collect();
        rooms.sort_by(|a, b| a.id.cmp(&b.id));
        rooms
    }
    /// Claims a connection slot, if one is free.
    fn reserve(&self) -> bool {
        self.count
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < self.max_connections).then_some(n + 1)
            })
            .is_ok()
    }
    fn release(&self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
    async fn accept(
        self: Arc<Self>,
        room: Arc<Room>,
        mut socket: WebSocket,
        peer: ConnectionPeer,
        permit: OwnedSemaphorePermit,
    ) {
        let (out, queue) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        room.instance.peers.lock().unwrap().insert(
            id,
            Peer {
                out,
                queued: queued.clone(),
            },
        );
        // The instance hears about the connection before any of its messages.
        if room.events.send(Event::Open(id, peer)).await.is_ok() {
            connection(socket, id, queue, queued, &room.events).await;
            let _ = room.events.send(Event::Close(id)).await;
        } else {
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: 1013,
                    reason: "The room closed; reconnect.".into(),
                })))
                .await;
        }
        room.instance.peers.lock().unwrap().remove(&id);
        self.release();
        drop(permit);
    }
}

/// What one instance shares with LOKI: its room, connections and timers.
pub(crate) struct Instance {
    room: String,
    peers: std::sync::Mutex<HashMap<u64, Peer>>,
    timers: std::sync::Mutex<Timers>,
    app: Weak<Handle>,
}
#[derive(Default)]
struct Timers {
    active: HashSet<u64>,
    /// Set (with a delay) or cancelled since the event loop last looked.
    changes: Vec<(u64, Option<Duration>)>,
}
struct Peer {
    out: mpsc::UnboundedSender<Out>,
    queued: Arc<AtomicUsize>,
}
enum Out {
    Data(Bytes),
    Text(String),
    File(std::fs::File),
    Close(u16, String),
}
impl Instance {
    /// Whether this is the instance for `room`.
    pub(crate) fn is(&self, room: &str) -> bool {
        self.room == room
    }
    fn app(&self) -> Result<Arc<Handle>> {
        self.app.upgrade().ok_or_else(|| busy("The app is stopped."))
    }
    fn queue(peer: &Peer, size: usize, item: Out) {
        let total = peer.queued.fetch_add(size, Ordering::SeqCst) + size;
        let item = if total > QUEUE_LIMIT {
            Out::Close(1008, "The connection fell behind.".into())
        } else {
            item
        };
        let _ = peer.out.send(item);
    }
    fn send(&self, id: u64, data: Bytes) -> std::result::Result<(), String> {
        let peers = self.peers.lock().unwrap();
        let peer = peers.get(&id).ok_or("Unknown connection.")?;
        Self::queue(peer, data.len(), Out::Data(data));
        Ok(())
    }
    fn send_text(&self, id: u64, text: String) -> std::result::Result<(), String> {
        let peers = self.peers.lock().unwrap();
        let peer = peers.get(&id).ok_or("Unknown connection.")?;
        Self::queue(peer, text.len(), Out::Text(text));
        Ok(())
    }
    /// Queues one message for each of `ids` that is open here; returns how many.
    fn broadcast(&self, ids: &[u64], data: Bytes) -> u32 {
        let peers = self.peers.lock().unwrap();
        let mut sent = 0;
        for peer in ids.iter().filter_map(|id| peers.get(id)) {
            Self::queue(peer, data.len(), Out::Data(data.clone()));
            sent += 1;
        }
        sent
    }
    fn send_file(&self, id: u64, file: std::fs::File) -> std::result::Result<(), String> {
        let peers = self.peers.lock().unwrap();
        let peer = peers.get(&id).ok_or("Unknown connection.")?;
        let _ = peer.out.send(Out::File(file));
        Ok(())
    }
    fn close(&self, id: u64, code: u16, reason: String) {
        // Only codes an endpoint may send; anything else becomes a normal close.
        let code = if code == 1000 || (3000..=4999).contains(&code) {
            code
        } else {
            1000
        };
        let reason = reason.chars().take(100).collect();
        if let Some(peer) = self.peers.lock().unwrap().get(&id) {
            let _ = peer.out.send(Out::Close(code, reason));
        }
    }
    fn close_all(&self, code: u16, reason: &str) {
        for peer in self.peers.lock().unwrap().values() {
            let _ = peer.out.send(Out::Close(code, reason.into()));
        }
    }
    fn set_timer(&self, id: u64, delay: Duration) -> Result<()> {
        let mut timers = self.timers.lock().unwrap();
        if !timers.active.contains(&id) && timers.active.len() >= MAX_TIMERS {
            return Err(Error::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("An instance may have at most {MAX_TIMERS} timers waiting."),
            ));
        }
        timers.active.insert(id);
        timers.changes.push((id, Some(delay)));
        Ok(())
    }
    fn cancel_timer(&self, id: u64) {
        let mut timers = self.timers.lock().unwrap();
        if timers.active.remove(&id) {
            timers.changes.push((id, None));
        }
    }
}

/// Moves one connection's messages until either side closes it.
async fn connection(
    socket: WebSocket,
    id: u64,
    mut queue: mpsc::UnboundedReceiver<Out>,
    queued: Arc<AtomicUsize>,
    events: &mpsc::Sender<Event>,
) {
    let (mut sink, mut stream) = socket.split();
    let writer = async {
        while let Some(item) = queue.recv().await {
            match item {
                Out::Data(data) => {
                    let size = data.len();
                    let sent = sink.send(Message::Binary(data)).await;
                    queued.fetch_sub(size, Ordering::SeqCst);
                    if sent.is_err() {
                        return;
                    }
                }
                Out::Text(text) => {
                    let size = text.len();
                    let sent = sink.send(Message::Text(text.into())).await;
                    queued.fetch_sub(size, Ordering::SeqCst);
                    if sent.is_err() {
                        return;
                    }
                }
                Out::File(file) => {
                    let mut file = tokio::fs::File::from_std(file);
                    let mut buffer = vec![0; 65536];
                    loop {
                        match file.read(&mut buffer).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if sink
                                    .send(Message::Binary(buffer[..n].to_vec().into()))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(_) => {
                                let _ = sink
                                    .send(Message::Close(Some(CloseFrame {
                                        code: 1011,
                                        reason: "A file could not be read.".into(),
                                    })))
                                    .await;
                                return;
                            }
                        }
                    }
                }
                Out::Close(code, reason) => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code,
                            reason: reason.into(),
                        })))
                        .await;
                    return;
                }
            }
        }
    };
    // Ends with the close to send when the peer broke the protocol.
    let reader = async {
        while let Some(message) = stream.next().await {
            let data = match message {
                Ok(Message::Binary(data)) => data.to_vec(),
                Ok(Message::Text(text)) => text.as_bytes().to_vec(),
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Err(error) if error.to_string().contains("too long") => {
                    return Some((1009, "Message too big."));
                }
                Err(_) => return Some((1002, "Protocol error.")),
            };
            if events.send(Event::Message(id, data)).await.is_err() {
                break;
            }
        }
        None
    };
    let broken = tokio::select! {
        () = writer => None,
        broken = reader => broken,
    };
    if let Some((code, reason)) = broken {
        let _ = sink
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            })))
            .await;
    }
}

/// Messages handed over together: each connection and its message.
type Batch = Vec<(u64, Vec<u8>)>;
/// The `realtime-events` exports of an app built with SDK 0.5 or later.
struct Events {
    call: TypedFunc<(Vec<u8>,), (Vec<u8>,)>,
    timer: TypedFunc<(u64,), ()>,
    post: TypedFunc<(String, Vec<u8>), ()>,
    messages: TypedFunc<(Batch,), ()>,
}
/// The instance itself: one store and the handler functions it exports.
struct Live {
    store: Store<Host>,
    start: TypedFunc<(), ()>,
    open: TypedFunc<(u64, ConnectionPeer), ()>,
    message: TypedFunc<(u64, Vec<u8>), ()>,
    close: TypedFunc<(u64,), ()>,
    tick: TypedFunc<(), ()>,
    events: Option<Events>,
    fuel: u64,
    /// This instance's place among the realtime instances LOKI runs at once.
    _slot: OwnedSemaphorePermit,
}
impl Live {
    async fn start(
        runtime: &Arc<Runtime>,
        instance: &Arc<Instance>,
        issuer: &str,
    ) -> anyhow::Result<Self> {
        let slot = runtime
            .realtime_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("LOKI is running as many realtime instances as it can."))?;
        let mut store = components::new_store(
            runtime,
            json!({"user":null,"csrf":"","issuer":issuer}),
            Value::Null,
            Vec::new(),
            Some(instance.clone()),
        )
        .map_err(|e| anyhow::anyhow!(e.message))?;
        store.set_fuel(runtime.manifest().limits.fuel)?;
        let component = runtime
            .pre()
            .instance_pre()
            .instantiate_async(&mut store)
            .await?;
        let interface = HANDLER
            .iter()
            .find_map(|name| component.get_export_index(&mut store, None, name))
            .ok_or_else(|| anyhow::anyhow!("The component does not export realtime-handler."))?;
        let events = component.get_export_index(&mut store, None, EVENTS);
        let mut index = |interface, name: &str| {
            component
                .get_export_index(&mut store, Some(interface), name)
                .ok_or_else(|| anyhow::anyhow!("The component does not export {name}."))
        };
        let (start, open, message, close, tick) = (
            index(&interface, "on-start")?,
            index(&interface, "on-open")?,
            index(&interface, "on-message")?,
            index(&interface, "on-close")?,
            index(&interface, "on-tick")?,
        );
        let events = match events {
            Some(events) => {
                let (call, timer, post, messages) = (
                    index(&events, "on-call")?,
                    index(&events, "on-timer")?,
                    index(&events, "on-post")?,
                    index(&events, "on-messages")?,
                );
                Some(Events {
                    call: component.get_typed_func(&mut store, call)?,
                    timer: component.get_typed_func(&mut store, timer)?,
                    post: component.get_typed_func(&mut store, post)?,
                    messages: component.get_typed_func(&mut store, messages)?,
                })
            }
            None => None,
        };
        let mut live = Self {
            start: component.get_typed_func(&mut store, start)?,
            open: component.get_typed_func(&mut store, open)?,
            message: component.get_typed_func(&mut store, message)?,
            close: component.get_typed_func(&mut store, close)?,
            tick: component.get_typed_func(&mut store, tick)?,
            events,
            fuel: runtime.manifest().realtime().unwrap_or_default().event_fuel,
            _slot: slot,
            store,
        };
        let limits = &runtime.manifest().limits;
        let func = live.start;
        live.call(
            func,
            (),
            limits.fuel,
            Duration::from_secs(limits.timeout_seconds),
        )
        .await?;
        Ok(live)
    }
    async fn call<P, R>(
        &mut self,
        func: TypedFunc<P, R>,
        params: P,
        fuel: u64,
        time: Duration,
    ) -> anyhow::Result<R>
    where
        P: ComponentNamedList + Lower + Send + Sync + 'static,
        R: ComponentNamedList + Lift + Send + Sync + 'static,
    {
        self.store.set_fuel(fuel)?;
        let result = tokio::time::timeout(time, func.call_async(&mut self.store, params))
            .await
            .map_err(|_| anyhow::anyhow!("A realtime event exceeded its time limit."))??;
        Ok(result)
    }
    async fn event(&mut self, event: Call) -> anyhow::Result<()> {
        let fuel = self.fuel;
        match event {
            Call::Open(id, peer) => self.call(self.open, (id, peer), fuel, EVENT_TIME).await,
            Call::Message(id, data) => self.call(self.message, (id, data), fuel, EVENT_TIME).await,
            Call::Messages(batch) => match &self.events {
                Some(events) => {
                    let func = events.messages;
                    self.call(func, (batch,), fuel, EVENT_TIME).await
                }
                None => {
                    for (id, data) in batch {
                        self.call(self.message, (id, data), fuel, EVENT_TIME).await?;
                    }
                    Ok(())
                }
            },
            Call::Close(id) => self.call(self.close, (id,), fuel, EVENT_TIME).await,
            Call::Tick => self.call(self.tick, (), fuel, EVENT_TIME).await,
            Call::Timer(id) => match &self.events {
                Some(events) => {
                    let func = events.timer;
                    self.call(func, (id,), fuel, EVENT_TIME).await
                }
                None => Ok(()),
            },
            Call::Post(from, data) => match &self.events {
                Some(events) => {
                    let func = events.post;
                    self.call(func, (from, data), fuel, EVENT_TIME).await
                }
                None => Ok(()),
            },
            Call::Ask(data, reply) => match &self.events {
                Some(events) => {
                    let func = events.call;
                    let (answer,) = self.call(func, (data,), fuel, EVENT_TIME).await?;
                    let _ = reply.send(Some(answer));
                    Ok(())
                }
                None => {
                    let _ = reply.send(None);
                    Ok(())
                }
            },
        }
    }
}
enum Call {
    Open(u64, ConnectionPeer),
    Message(u64, Vec<u8>),
    Messages(Batch),
    Close(u64),
    Tick,
    Timer(u64),
    Post(String, Vec<u8>),
    Ask(Vec<u8>, oneshot::Sender<Option<Vec<u8>>>),
}
enum Outcome {
    Stop(u16, &'static str),
    Restart,
    Idle,
    Failed(anyhow::Error),
}

/// What the event loop keeps between turns.
struct Loop {
    /// Connections this instance has seen open. Others belong to an earlier instance.
    open: HashSet<u64>,
    /// An event read while gathering a batch, handled next.
    held: Option<Event>,
    timers: DelayQueue<u64>,
    keys: HashMap<u64, delay_queue::Key>,
}
impl Loop {
    fn new() -> Self {
        Self {
            open: HashSet::new(),
            held: None,
            timers: DelayQueue::new(),
            keys: HashMap::new(),
        }
    }
    /// Applies the timers the instance set or cancelled during its last event.
    fn sync_timers(&mut self, instance: &Instance) {
        let changes = std::mem::take(&mut instance.timers.lock().unwrap().changes);
        for (id, delay) in changes {
            if let Some(key) = self.keys.remove(&id) {
                self.timers.remove(&key);
            }
            if let Some(delay) = delay {
                self.keys.insert(id, self.timers.insert(id, delay));
            }
        }
    }
}

/// Feeds events to one instance until it stops, restarts, idles or fails.
async fn serve(
    live: &mut Live,
    instance: &Instance,
    events: &mut mpsc::Receiver<Event>,
    tick: &mut Option<tokio::time::Interval>,
    idle: Option<Duration>,
) -> Outcome {
    let mut state = Loop::new();
    instance.timers.lock().unwrap().active.clear();
    let batches = live.events.is_some();
    let mut idle_at = idle.map(|i| tokio::time::Instant::now() + i);
    loop {
        let event = match state.held.take() {
            Some(event) => Some(event),
            None => tokio::select! {
                event = events.recv() => event,
                expired = state.timers.next(), if !state.timers.is_empty() => {
                    let id = expired.expect("a non-empty queue").into_inner();
                    state.keys.remove(&id);
                    instance.timers.lock().unwrap().active.remove(&id);
                    if let Err(error) = live.event(Call::Timer(id)).await {
                        return Outcome::Failed(error);
                    }
                    state.sync_timers(instance);
                    continue;
                }
                _ = async { tick.as_mut().unwrap().tick().await }, if tick.is_some() => {
                    if let Err(error) = live.event(Call::Tick).await {
                        return Outcome::Failed(error);
                    }
                    state.sync_timers(instance);
                    continue;
                }
                () = async { tokio::time::sleep_until(idle_at.unwrap()).await }, if idle_at.is_some() => {
                    return Outcome::Idle;
                }
            },
        };
        let call = match event {
            None => return Outcome::Stop(1001, "The app stopped."),
            Some(Event::Stop(code, reason)) => return Outcome::Stop(code, reason),
            Some(Event::Restart) => return Outcome::Restart,
            Some(Event::Open(id, info)) => {
                state.open.insert(id);
                idle_at = None;
                Call::Open(id, info)
            }
            Some(Event::Message(id, data)) if state.open.contains(&id) => {
                let mut batch = vec![(id, data)];
                // Messages already waiting go to the instance together.
                while batches && batch.len() < BATCH {
                    match events.try_recv() {
                        Ok(Event::Message(id, data)) if state.open.contains(&id) => {
                            batch.push((id, data));
                        }
                        Ok(Event::Message(..)) => {}
                        Ok(other) => {
                            state.held = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if batch.len() == 1 {
                    let (id, data) = batch.pop().unwrap();
                    Call::Message(id, data)
                } else {
                    Call::Messages(batch)
                }
            }
            Some(Event::Close(id)) if state.open.remove(&id) => {
                if state.open.is_empty() {
                    idle_at = idle.map(|i| tokio::time::Instant::now() + i);
                }
                Call::Close(id)
            }
            Some(Event::Call(data, reply)) => Call::Ask(data, reply),
            Some(Event::Post(from, data)) => Call::Post(from, data),
            Some(_) => continue,
        };
        if let Err(error) = live.event(call).await {
            return Outcome::Failed(error);
        }
        state.sync_timers(instance);
    }
}

/// Waits out a restart delay. Connections that open meanwhile are turned away.
/// Returns true when the app is stopping.
async fn pause(events: &mut mpsc::Receiver<Event>, instance: &Instance, delay: Duration) -> bool {
    let until = tokio::time::Instant::now() + delay;
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(until) => return false,
            event = events.recv() => match event {
                None | Some(Event::Stop(..)) => return true,
                Some(Event::Open(id, _)) => instance.close(id, 1013, "The app is restarting.".into()),
                Some(Event::Call(_, reply)) => { let _ = reply.send(None); }
                Some(_) => {}
            },
        }
    }
}

/// Runs one instance's event loop. The main instance restarts after a failure, backing
/// off; a room ends instead, and the next connection to it starts it afresh.
async fn run(
    runtime: Arc<Runtime>,
    app: Weak<Handle>,
    instance: Arc<Instance>,
    mut events: mpsc::Receiver<Event>,
    mut live: Live,
    issuer: String,
    idle: Option<Duration>,
) {
    let main = instance.room == MAIN;
    let mut tick = runtime
        .manifest()
        .realtime()
        .and_then(|r| r.tick_ms)
        .map(|ms| {
            let mut interval = tokio::time::interval(Duration::from_millis(ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval
        });
    let mut backoff = FIRST_BACKOFF;
    let mut started = Instant::now();
    loop {
        match serve(&mut live, &instance, &mut events, &mut tick, idle).await {
            Outcome::Stop(code, reason) => {
                instance.close_all(code, reason);
                break;
            }
            Outcome::Idle => break,
            Outcome::Restart => {
                instance.close_all(1012, "The app restarted.");
                backoff = FIRST_BACKOFF;
            }
            Outcome::Failed(error) => {
                let place = if main {
                    String::new()
                } else {
                    format!(" (room {})", instance.room)
                };
                runtime.log(&format!("Realtime instance{place} failed: {error:#}"));
                instance.close_all(1011, "The app failed.");
                if !main {
                    break;
                }
                if started.elapsed() > MAX_BACKOFF {
                    backoff = FIRST_BACKOFF;
                }
                if pause(&mut events, &instance, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
        loop {
            match Live::start(&runtime, &instance, &issuer).await {
                Ok(next) => {
                    live = next;
                    started = Instant::now();
                    break;
                }
                Err(error) => {
                    runtime.log(&format!("Realtime instance could not start: {error:#}"));
                    if pause(&mut events, &instance, backoff).await {
                        instance.close_all(1001, "The app stopped.");
                        return;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
    // A room that ended on its own leaves the app's list, unless it was replaced.
    if !main && let Some(app) = app.upgrade() {
        let mut rooms = app.rooms.lock().unwrap();
        if rooms
            .get(&instance.room)
            .is_some_and(|r| Arc::ptr_eq(&r.instance, &instance))
        {
            rooms.remove(&instance.room);
        }
    }
}

/// Starts an app's main realtime instance, and its LAN listener when one is set.
pub(crate) async fn start(
    state: &State,
    runtime: Arc<Runtime>,
    app: &apps::App,
) -> Result<Arc<Handle>> {
    let settings = runtime.manifest().realtime().unwrap_or_default();
    let issuer = state.0.settings.read().await.public_url.clone();
    let listener = if app.lan_bind.is_empty() {
        None
    } else {
        Some(
            tokio::net::TcpListener::bind(&app.lan_bind)
                .await
                .map_err(|e| Error::bad(format!("Could not listen on {}: {e}", app.lan_bind)))?,
        )
    };
    let handle = Arc::new(Handle {
        runtime: Arc::downgrade(&runtime),
        issuer,
        rooms: std::sync::Mutex::new(HashMap::new()),
        starting: tokio::sync::Mutex::new(()),
        next: AtomicU64::new(1),
        count: AtomicUsize::new(0),
        max_connections: settings.max_connections,
        max_message_bytes: settings.max_message_bytes,
        room_limits: settings.rooms.clone(),
        lan: std::sync::Mutex::new(None),
    });
    handle
        .launch(runtime.clone(), MAIN, None)
        .await
        .map_err(|e| {
            let detail = format!("{e:#}").chars().take(300).collect::<String>();
            runtime.log(&format!("Realtime instance could not start: {detail}"));
            Error::bad(format!(
                "The app's realtime instance could not start: {detail}"
            ))
        })?;
    if let Some(listener) = listener {
        let (stop, stopped) = oneshot::channel();
        *handle.lan.lock().unwrap() = Some(stop);
        let router = axum::Router::new().fallback(lan).with_state(Lan {
            state: state.clone(),
            app: app.id.clone(),
        });
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await;
        });
    }
    Ok(handle)
}

pub(crate) fn is_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| h.eq_ignore_ascii_case("websocket"))
}
/// The room a request path reaches: `Some("")` for the realtime path itself and
/// `Some(room)` for `{path}/{room}` when the app has rooms.
pub(crate) fn room_of<'a>(settings: &loki_sdk::manifest::Realtime, path: &'a str) -> Option<&'a str> {
    if path == settings.path {
        return Some(MAIN);
    }
    settings.rooms.as_ref()?;
    path.strip_prefix(settings.path.as_str())?
        .strip_prefix('/')
        .filter(|room| apps::valid_id(room))
}

/// Accepts a WebSocket connection to an app's realtime endpoint, starting its room
/// when needed. `proxied` is true on LOKI's own listener, where Caddy may forward
/// connections; never on an app's LAN listener.
pub(crate) async fn upgrade(
    state: &State,
    app: &str,
    room: &str,
    request: Request<Body>,
    user: Value,
    proxied: bool,
) -> Result<Response> {
    let runtime = components::running(state, app).await?;
    let handle = runtime
        .realtime
        .get()
        .cloned()
        .ok_or_else(|| busy("The app's realtime instance is not running."))?;
    // Behind Caddy every connection comes from loopback; tell the app who it really is.
    let behind_caddy = proxied && state.0.settings.read().await.caddy_enabled;
    let remote = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| crate::auth::client_addr(c.0, request.headers(), behind_caddy).to_string())
        .unwrap_or_default();
    let (mut parts, _) = request.into_parts();
    let socket = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .map_err(|_| Error::bad("Expected a WebSocket upgrade."))?;
    let permit = state
        .0
        .sockets
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy("LOKI has too many open connections."))?;
    if !handle.reserve() {
        return Err(busy("This app has too many open connections."));
    }
    let room = match handle.open_room(room).await {
        Ok(room) => room,
        Err(error) => {
            handle.release();
            return Err(error);
        }
    };
    let peer = ConnectionPeer {
        remote,
        user: serde_json::from_value(user)?,
    };
    let size = handle.max_message_bytes;
    let failed = handle.clone();
    Ok(socket
        .max_message_size(size)
        .max_frame_size(size)
        .on_failed_upgrade(move |_| failed.release())
        .on_upgrade(move |socket| handle.accept(room, socket, peer, permit)))
}
fn busy(message: &str) -> Error {
    Error::new(StatusCode::SERVICE_UNAVAILABLE, message)
}

/// The app's own LAN port: its realtime endpoint and health check, nothing else.
#[derive(Clone)]
struct Lan {
    state: State,
    app: String,
}
async fn lan(
    axum::extract::State(lan): axum::extract::State<Lan>,
    request: Request<Body>,
) -> Response {
    let result = async {
        if lan.state.0.maintenance.load(Ordering::SeqCst) {
            return Err(busy("LOKI is restarting. Reconnect shortly."));
        }
        let runtime = components::running(&lan.state, &lan.app).await?;
        let path = request.uri().path().to_string();
        let realtime = runtime.manifest().realtime().unwrap_or_default();
        if let Some(room) = room_of(&realtime, &path)
            && is_upgrade(request.headers())
        {
            return upgrade(&lan.state, &lan.app, room, request, Value::Null, false).await;
        }
        if path == runtime.manifest().health && request.method() == Method::GET {
            return components::health(&lan.state, runtime, request).await;
        }
        Err(Error::not_found())
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

/// The `realtime` host functions. They fail outside a realtime instance.
pub(crate) fn link(linker: &mut Linker<Host>) -> Result<()> {
    fn instance(host: &Host) -> Result<&Arc<Instance>> {
        host.instance().ok_or_else(|| Error::bad(NOT_REALTIME))
    }
    let mut realtime = linker.instance("loki:app/realtime@0.3.1")?;
    realtime.func_wrap("send", |context, (id, data): (u64, Vec<u8>)| {
        let result =
            instance(context.data()).and_then(|i| i.send(id, data.into()).map_err(Error::bad));
        Ok((result.map_err(WitError::from),))
    })?;
    realtime.func_wrap("send-text", |context, (id, text): (u64, String)| {
        let result =
            instance(context.data()).and_then(|i| i.send_text(id, text).map_err(Error::bad));
        Ok((result.map_err(WitError::from),))
    })?;
    realtime.func_wrap(
        "broadcast",
        |context, (ids, data): (Vec<u64>, Vec<u8>)| {
            let sent = instance(context.data()).map_or(0, |i| i.broadcast(&ids, data.into()));
            Ok((sent,))
        },
    )?;
    realtime.func_wrap_async(
        "send-file",
        |mut context, (id, source, path): (u64, FileSource, String)| {
            Box::new(async move {
                let host = context.data_mut();
                let result = match host.instance().cloned() {
                    None => Err(Error::bad(NOT_REALTIME)),
                    Some(instance) => match host.open(source, &path).await {
                        Ok(file) => match file.metadata() {
                            Ok(metadata) => instance
                                .send_file(id, file)
                                .map(|()| metadata.len())
                                .map_err(Error::bad),
                            Err(e) => Err(e.into()),
                        },
                        Err(e) => Err(e),
                    },
                };
                Ok((result.map_err(WitError::from),))
            })
        },
    )?;
    realtime.func_wrap(
        "close",
        |context, (id, code, reason): (u64, u16, String)| {
            if let Ok(instance) = instance(context.data()) {
                instance.close(id, code, reason);
            }
            Ok(())
        },
    )?;
    realtime.func_wrap("set-timer", |context, (id, delay): (u64, u32)| {
        let result = instance(context.data())
            .and_then(|i| i.set_timer(id, Duration::from_millis(u64::from(delay))));
        Ok((result.map_err(WitError::from),))
    })?;
    realtime.func_wrap("cancel-timer", |context, (id,): (u64,)| {
        if let Ok(instance) = instance(context.data()) {
            instance.cancel_timer(id);
        }
        Ok(())
    })?;
    realtime.func_wrap("room", |context, ()| {
        Ok((instance(context.data()).map_or_else(|_| String::new(), |i| i.room.clone()),))
    })?;
    realtime.func_wrap_async("open-room", |context, (id,): (String,)| {
        Box::new(async move {
            let result = match instance(context.data()).and_then(|i| i.app()) {
                Ok(app) => app.open_room(&id).await.map(|_| ()),
                Err(e) => Err(e),
            };
            Ok((result.map_err(WitError::from),))
        })
    })?;
    realtime.func_wrap_async("close-room", |context, (id,): (String,)| {
        Box::new(async move {
            let result = match instance(context.data()).and_then(|i| i.app()) {
                Ok(app) => app.close_room(&id).await,
                Err(e) => Err(e),
            };
            Ok((result.map_err(WitError::from),))
        })
    })?;
    realtime.func_wrap(
        "post",
        |context, (room, data): (String, Vec<u8>)| {
            let result = instance(context.data()).and_then(|i| {
                if i.room == room {
                    return Err(Error::bad("A room cannot post to itself."));
                }
                i.app()?.post_now(&i.room, &room, data)
            });
            Ok((result.map_err(WitError::from),))
        },
    )?;
    realtime.func_wrap("list-rooms", |context, ()| {
        let rooms = instance(context.data())
            .and_then(|i| i.app())
            .map(|app| app.list())
            .unwrap_or_default();
        Ok((rooms,))
    })?;
    Ok(())
}
impl Handle {
    /// [`Handle::post`] without waiting, for the synchronous host function.
    fn post_now(&self, from: &str, to: &str, data: Vec<u8>) -> Result<()> {
        let room = self.room(to).ok_or_else(Error::not_found)?;
        room.events
            .try_send(Event::Post(from.to_string(), data))
            .map_err(|_| busy("That room is not keeping up."))
    }
}
