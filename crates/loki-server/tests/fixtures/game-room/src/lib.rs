//! Test component for LOKI's game-server features: rooms, calls, posts, timers,
//! batches, broadcasts and text frames, plus appended storage, outbound requests,
//! scheduled jobs, streamed responses and kept instances. Commands arrive as text.
use loki_sdk::realtime::{self, Connection, OpenInfo, RealtimeApp};
use loki_sdk::{Request, Response, Result, Router, Storage};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Requests to `/count` this instance has served, to tell kept instances apart.
static COUNT: AtomicU64 = AtomicU64::new(0);

fn router() -> Router {
    Router::new()
        .get("/health", |_| Ok(Response::text("ready")))
        .get("/call", |r| {
            let room = r.query("room").unwrap_or_default();
            let answer = realtime::call(&room, r.query("q").unwrap_or_default().as_bytes())?;
            Ok(Response::text(String::from_utf8_lossy(&answer).into_owned()))
        })
        .get("/rooms", |_| Response::json(&realtime::rooms()?))
        .get("/append", |r| {
            let path = r.query("path").unwrap_or_default();
            let kib: usize = r.query("kib").unwrap_or_default().parse().unwrap_or(1);
            let info = Storage.append(&path, vec![b'a'; kib * 1024])?;
            Ok(Response::text(info.size.to_string()))
        })
        .get("/rename", |r| {
            let (from, to) = (r.query("from").unwrap_or_default(), r.query("to").unwrap_or_default());
            let file = Storage.rename(&from, &to, r.query("replace").is_some())?;
            Ok(Response::text(file.info()?.size.to_string()))
        })
        .get("/usage", |_| Ok(Response::text(Storage.usage()?.limit_bytes.to_string())))
        .get("/fetch", |r| {
            let url = r.query("url").unwrap_or_default();
            let fetched = loki_sdk::fetch::get(&url)?;
            Ok(Response::text(format!("{} {}", fetched.status, fetched.text()?)))
        })
        .get("/stream", |_| {
            let body: String = (0..300 * 1024).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
            Ok(Response::text(body))
        })
        .get("/count", |_| {
            Ok(Response::text((COUNT.fetch_add(1, Ordering::SeqCst) + 1).to_string()))
        })
        .get("/trap", |_| panic!("deliberate trap"))
        .schedule("tick", |_| {
            let runs: u64 = Storage.get_json("runs")?.unwrap_or(0);
            Storage.set_json("runs", &(runs + 1))?;
            Ok(Response::text("ran"))
        })
        .get("/runs", |_| {
            Ok(Response::text(Storage.get_json::<u64>("runs")?.unwrap_or(0).to_string()))
        })
}
fn handle(request: Request) -> Result<Response> {
    router().handle(request)
}

struct Game {
    room: String,
    connections: BTreeSet<Connection>,
    /// The most messages one `messages` call has brought.
    batch: usize,
}
impl Game {
    fn everyone(&self, text: &str) {
        realtime::broadcast(self.connections.iter().copied(), text.as_bytes());
    }
}
impl RealtimeApp for Game {
    fn start() -> Self {
        Game {
            room: realtime::room(),
            connections: BTreeSet::new(),
            batch: 0,
        }
    }
    fn open(&mut self, connection: Connection, _: OpenInfo) {
        self.connections.insert(connection);
        let _ = connection.send_text(&format!("room {}", self.room));
    }
    fn message(&mut self, connection: Connection, data: Vec<u8>) {
        let text = String::from_utf8_lossy(&data).to_string();
        let (command, rest) = text.split_once(' ').unwrap_or((&text, ""));
        match command {
            "timer" => {
                let ms = rest.parse().unwrap_or(10);
                realtime::set_timer(1, Duration::from_millis(ms)).unwrap();
            }
            "post" => {
                let (room, message) = rest.split_once(' ').unwrap_or((rest, ""));
                if let Err(e) = realtime::post(room, message.as_bytes()) {
                    let _ = connection.send_text(&format!("post failed {}", e.status()));
                }
            }
            "all" => self.everyone(rest),
            "slow" => std::thread::sleep(Duration::from_millis(300)),
            "batch" => {
                let _ = connection.send_text(&format!("batch {}", self.batch));
            }
            _ => {}
        }
    }
    fn messages(&mut self, messages: Vec<(Connection, Vec<u8>)>) {
        self.batch = self.batch.max(messages.len());
        for (connection, data) in messages {
            self.message(connection, data);
        }
    }
    fn close(&mut self, connection: Connection) {
        self.connections.remove(&connection);
    }
    fn timer(&mut self, id: u64) {
        self.everyone(&format!("timer {id} fired"));
    }
    fn call(&mut self, data: Vec<u8>) -> Vec<u8> {
        format!(
            "{}:{}:{}",
            self.room,
            self.connections.len(),
            String::from_utf8_lossy(&data)
        )
        .into_bytes()
    }
    fn post(&mut self, from: String, data: Vec<u8>) {
        self.everyone(&format!("post from '{from}': {}", String::from_utf8_lossy(&data)));
    }
}

loki_sdk::export_realtime!(handle, Game);
