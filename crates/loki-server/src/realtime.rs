//! Long-lived instances for WASI apps with the `realtime` capability.
//!
//! Each such app gets one instance that LOKI keeps alive, so its memory persists
//! between calls. LOKI holds the app's WebSocket connections and hands the instance
//! one event at a time: a connection opened, a message arrived, a connection closed,
//! or a tick. The instance answers through the `realtime` host interface.
use crate::{
    Error, Result, State, apps,
    components::{self, FileSource, Host, Runtime},
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
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, mpsc, oneshot},
    task::JoinHandle,
};
use wasmtime::{
    Store,
    component::{ComponentNamedList, Linker, Lower, TypedFunc},
};

/// Bytes a connection may have waiting to be sent before it counts as fallen behind.
const QUEUE_LIMIT: usize = 4 * 1024 * 1024;
/// The longest one event may run, apart from `on-start`.
const EVENT_TIME: Duration = Duration::from_secs(5);
/// Restarts after a failure wait this long at first, doubling up to `MAX_BACKOFF`.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const NOT_REALTIME: &str = "Only the app's realtime instance can use connections.";

/// The running realtime instance of one app, and its connections.
pub struct Handle {
    events: mpsc::Sender<Event>,
    connections: Arc<Connections>,
    max_connections: usize,
    max_message_bytes: usize,
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    lan: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}
enum Event {
    Open(u64, String),
    Message(u64, Vec<u8>),
    Close(u64),
    Restart,
    Stop,
}
impl Handle {
    /// Closes every connection and ends the instance. Waits for it to finish.
    pub async fn stop(&self) {
        if let Some(lan) = self.lan.lock().unwrap().take() {
            let _ = lan.send(());
        }
        let _ = self.events.send(Event::Stop).await;
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
    }
    /// Closes every connection and starts a fresh instance.
    pub async fn restart(&self) {
        let _ = self.events.send(Event::Restart).await;
    }
    async fn accept(
        self: Arc<Self>,
        socket: WebSocket,
        info: String,
        permit: OwnedSemaphorePermit,
    ) {
        let (out, queue) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let id = self.connections.add(Peer {
            out,
            queued: queued.clone(),
        });
        // The instance hears about the connection before any of its messages.
        if self.events.send(Event::Open(id, info)).await.is_ok() {
            connection(socket, id, queue, queued, &self.events).await;
            let _ = self.events.send(Event::Close(id)).await;
        }
        self.connections.remove(id);
        drop(permit);
    }
}

/// The app's open connections, shared between the instance and the socket tasks.
pub(crate) struct Connections {
    next: AtomicU64,
    count: AtomicUsize,
    peers: std::sync::Mutex<HashMap<u64, Peer>>,
}
struct Peer {
    out: mpsc::UnboundedSender<Out>,
    queued: Arc<AtomicUsize>,
}
enum Out {
    Data(Vec<u8>),
    File(std::fs::File),
    Close(u16, String),
}
impl Connections {
    fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            count: AtomicUsize::new(0),
            peers: std::sync::Mutex::new(HashMap::new()),
        }
    }
    /// Claims a connection slot, if one is free.
    fn reserve(&self, max: usize) -> bool {
        self.count
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < max).then_some(n + 1)
            })
            .is_ok()
    }
    fn release(&self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
    fn add(&self, peer: Peer) -> u64 {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.peers.lock().unwrap().insert(id, peer);
        id
    }
    fn remove(&self, id: u64) {
        if self.peers.lock().unwrap().remove(&id).is_some() {
            self.release();
        }
    }
    fn send(&self, id: u64, data: Vec<u8>) -> std::result::Result<(), String> {
        let peers = self.peers.lock().unwrap();
        let peer = peers.get(&id).ok_or("Unknown connection.")?;
        let total = peer.queued.fetch_add(data.len(), Ordering::SeqCst) + data.len();
        let item = if total > QUEUE_LIMIT {
            Out::Close(1008, "The connection fell behind.".into())
        } else {
            Out::Data(data)
        };
        let _ = peer.out.send(item);
        Ok(())
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
                    let sent = sink.send(Message::Binary(data.into())).await;
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

/// The instance itself: one store and the handler functions it exports.
struct Live {
    store: Store<Host>,
    start: TypedFunc<(), ()>,
    open: TypedFunc<(u64, String), ()>,
    message: TypedFunc<(u64, Vec<u8>), ()>,
    close: TypedFunc<(u64,), ()>,
    tick: TypedFunc<(), ()>,
    fuel: u64,
}
impl Live {
    async fn start(
        runtime: &Arc<Runtime>,
        connections: &Arc<Connections>,
        issuer: &str,
    ) -> anyhow::Result<Self> {
        let mut store = components::new_store(
            runtime,
            json!({"user":null,"csrf":"","issuer":issuer}),
            Value::Null,
            Vec::new(),
            Some(connections.clone()),
        )
        .map_err(|e| anyhow::anyhow!(e.message))?;
        store.set_fuel(runtime.manifest().limits.fuel)?;
        let instance = runtime
            .pre()
            .instance_pre()
            .instantiate_async(&mut store)
            .await?;
        let interface = instance
            .get_export_index(&mut store, None, "loki:app/realtime-handler@0.1.0")
            .ok_or_else(|| anyhow::anyhow!("The component does not export realtime-handler."))?;
        let mut index = |name: &str| {
            instance
                .get_export_index(&mut store, Some(&interface), name)
                .ok_or_else(|| anyhow::anyhow!("The component does not export {name}."))
        };
        let (start, open, message, close, tick) = (
            index("on-start")?,
            index("on-open")?,
            index("on-message")?,
            index("on-close")?,
            index("on-tick")?,
        );
        let mut live = Self {
            start: instance.get_typed_func(&mut store, start)?,
            open: instance.get_typed_func(&mut store, open)?,
            message: instance.get_typed_func(&mut store, message)?,
            close: instance.get_typed_func(&mut store, close)?,
            tick: instance.get_typed_func(&mut store, tick)?,
            fuel: runtime.manifest().realtime().unwrap_or_default().event_fuel,
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
    async fn call<P>(
        &mut self,
        func: TypedFunc<P, ()>,
        params: P,
        fuel: u64,
        time: Duration,
    ) -> anyhow::Result<()>
    where
        P: ComponentNamedList + Lower + Send + Sync + 'static,
    {
        self.store.set_fuel(fuel)?;
        tokio::time::timeout(time, func.call_async(&mut self.store, params))
            .await
            .map_err(|_| anyhow::anyhow!("A realtime event exceeded its time limit."))??;
        Ok(())
    }
    async fn event(&mut self, event: Call) -> anyhow::Result<()> {
        let fuel = self.fuel;
        match event {
            Call::Open(id, info) => self.call(self.open, (id, info), fuel, EVENT_TIME).await,
            Call::Message(id, data) => self.call(self.message, (id, data), fuel, EVENT_TIME).await,
            Call::Close(id) => self.call(self.close, (id,), fuel, EVENT_TIME).await,
            Call::Tick => self.call(self.tick, (), fuel, EVENT_TIME).await,
        }
    }
}
enum Call {
    Open(u64, String),
    Message(u64, Vec<u8>),
    Close(u64),
    Tick,
}
enum Outcome {
    Stop,
    Restart,
    Failed(anyhow::Error),
}

/// Feeds events to one instance until it stops, restarts or fails.
async fn serve(
    live: &mut Live,
    events: &mut mpsc::Receiver<Event>,
    tick: &mut Option<tokio::time::Interval>,
) -> Outcome {
    // Connections this instance has seen open. Others belong to an earlier instance.
    let mut open = HashSet::new();
    loop {
        let call = tokio::select! {
            event = events.recv() => match event {
                None | Some(Event::Stop) => return Outcome::Stop,
                Some(Event::Restart) => return Outcome::Restart,
                Some(Event::Open(id, info)) => {
                    open.insert(id);
                    Call::Open(id, info)
                }
                Some(Event::Message(id, data)) if open.contains(&id) => Call::Message(id, data),
                Some(Event::Close(id)) if open.remove(&id) => Call::Close(id),
                Some(_) => continue,
            },
            _ = async { tick.as_mut().unwrap().tick().await }, if tick.is_some() => Call::Tick,
        };
        if let Err(error) = live.event(call).await {
            return Outcome::Failed(error);
        }
    }
}

/// Waits out a restart delay. Connections that open meanwhile are turned away.
/// Returns true when the app is stopping.
async fn pause(
    events: &mut mpsc::Receiver<Event>,
    connections: &Connections,
    delay: Duration,
) -> bool {
    let until = tokio::time::Instant::now() + delay;
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(until) => return false,
            event = events.recv() => match event {
                None | Some(Event::Stop) => return true,
                Some(Event::Open(id, _)) => connections.close(id, 1013, "The app is restarting.".into()),
                Some(_) => {}
            },
        }
    }
}

async fn run(
    runtime: Arc<Runtime>,
    connections: Arc<Connections>,
    mut events: mpsc::Receiver<Event>,
    mut live: Live,
    issuer: String,
) {
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
        match serve(&mut live, &mut events, &mut tick).await {
            Outcome::Stop => {
                connections.close_all(1001, "The app stopped.");
                return;
            }
            Outcome::Restart => {
                connections.close_all(1012, "The app restarted.");
                backoff = FIRST_BACKOFF;
            }
            Outcome::Failed(error) => {
                runtime.log(&format!("Realtime instance failed: {error:#}"));
                connections.close_all(1011, "The app failed.");
                if started.elapsed() > MAX_BACKOFF {
                    backoff = FIRST_BACKOFF;
                }
                if pause(&mut events, &connections, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
        loop {
            match Live::start(&runtime, &connections, &issuer).await {
                Ok(next) => {
                    live = next;
                    started = Instant::now();
                    break;
                }
                Err(error) => {
                    runtime.log(&format!("Realtime instance could not start: {error:#}"));
                    if pause(&mut events, &connections, backoff).await {
                        connections.close_all(1001, "The app stopped.");
                        return;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
}

/// Starts an app's realtime instance, and its LAN listener when one is set.
pub(crate) async fn start(
    state: &State,
    runtime: Arc<Runtime>,
    app: &apps::App,
) -> Result<Arc<Handle>> {
    let settings = runtime.manifest().realtime().unwrap_or_default();
    let issuer = state.0.settings.read().await.public_url.clone();
    let connections = Arc::new(Connections::new());
    let live = Live::start(&runtime, &connections, &issuer)
        .await
        .map_err(|e| {
            let detail = format!("{e:#}").chars().take(300).collect::<String>();
            runtime.log(&format!("Realtime instance could not start: {detail}"));
            Error::bad(format!(
                "The app's realtime instance could not start: {detail}"
            ))
        })?;
    let listener = if app.lan_bind.is_empty() {
        None
    } else {
        Some(
            tokio::net::TcpListener::bind(&app.lan_bind)
                .await
                .map_err(|e| Error::bad(format!("Could not listen on {}: {e}", app.lan_bind)))?,
        )
    };
    let (events, receiver) = mpsc::channel(1024);
    let handle = Arc::new(Handle {
        events,
        connections: connections.clone(),
        max_connections: settings.max_connections,
        max_message_bytes: settings.max_message_bytes,
        task: tokio::sync::Mutex::new(Some(tokio::spawn(run(
            runtime,
            connections,
            receiver,
            live,
            issuer,
        )))),
        lan: std::sync::Mutex::new(None),
    });
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

/// Accepts a WebSocket connection to an app's realtime endpoint.
pub(crate) async fn upgrade(
    state: &State,
    app: &str,
    request: Request<Body>,
    user: Value,
) -> Result<Response> {
    let runtime = components::running(state, app).await?;
    let handle = runtime
        .realtime
        .get()
        .cloned()
        .ok_or_else(|| busy("The app's realtime instance is not running."))?;
    let remote = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.to_string())
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
    if !handle.connections.reserve(handle.max_connections) {
        return Err(busy("This app has too many open connections."));
    }
    let info = json!({"remote":remote,"user":user}).to_string();
    let size = handle.max_message_bytes;
    let failed = handle.connections.clone();
    Ok(socket
        .max_message_size(size)
        .max_frame_size(size)
        .on_failed_upgrade(move |_| failed.release())
        .on_upgrade(move |socket| handle.accept(socket, info, permit)))
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
        if path == realtime.path && is_upgrade(request.headers()) {
            return upgrade(&lan.state, &lan.app, request, Value::Null).await;
        }
        if path == runtime.manifest().health && request.method() == Method::GET {
            return components::health(&lan.state, runtime, request).await;
        }
        Err(Error::not_found())
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

/// The `realtime` host functions. They fail outside the realtime instance.
pub(crate) fn link(linker: &mut Linker<Host>) -> Result<()> {
    let mut realtime = linker.instance("loki:app/realtime@0.1.0")?;
    realtime.func_wrap("send", |context, (id, data): (u64, Vec<u8>)| {
        let result = match context.data().connections() {
            Some(connections) => connections.send(id, data),
            None => Err(NOT_REALTIME.into()),
        };
        Ok((result,))
    })?;
    realtime.func_wrap_async(
        "send-file",
        |mut context, (id, source, path): (u64, FileSource, String)| {
            Box::new(async move {
                let host = context.data_mut();
                let result = match host.connections().cloned() {
                    None => Err(NOT_REALTIME.to_string()),
                    Some(connections) => match host.open(source, &path).await {
                        Ok(file) => match file.metadata() {
                            Ok(metadata) => {
                                connections.send_file(id, file).map(|()| metadata.len())
                            }
                            Err(e) => Err(e.to_string()),
                        },
                        Err(e) => Err(e.message),
                    },
                };
                Ok((result,))
            })
        },
    )?;
    realtime.func_wrap(
        "close",
        |context, (id, code, reason): (u64, u16, String)| {
            if let Some(connections) = context.data().connections() {
                connections.close(id, code, reason);
            }
            Ok(())
        },
    )?;
    Ok(())
}
