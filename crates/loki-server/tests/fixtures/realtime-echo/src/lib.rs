//! Test component for LOKI's realtime instances. Commands arrive as text messages.
use loki_sdk::realtime::{Connection, FileSource, OpenInfo, RealtimeApp};
use loki_sdk::{Request, Response, Result};
use std::collections::BTreeSet;

fn handle(request: Request) -> Result<Response> {
    match request.path() {
        "/health" => Ok(Response::text("ready")),
        "/restart" => {
            loki_sdk::realtime::restart()?;
            Ok(Response::text("restarted"))
        }
        "/size" => Ok(Response::text(
            loki_sdk::realtime::file_size(FileSource::Asset, "blob.bin")?.to_string(),
        )),
        "/escape" => Ok(Response::text(
            loki_sdk::realtime::file_size(FileSource::Asset, "echo.wasm")
                .is_err()
                .to_string(),
        )),
        _ => Ok(Response::empty(404)),
    }
}

/// Every connection, and how many ticks this instance has seen.
struct Echo {
    connections: BTreeSet<Connection>,
    ticks: u64,
}
impl RealtimeApp for Echo {
    fn start() -> Self {
        Echo {
            connections: BTreeSet::new(),
            ticks: 0,
        }
    }
    fn open(&mut self, connection: Connection, info: OpenInfo) {
        self.connections.insert(connection);
        let _ = connection.send(format!("open {}", !info.remote.is_empty()).as_bytes());
    }
    fn message(&mut self, connection: Connection, data: Vec<u8>) {
        let text = String::from_utf8_lossy(&data).to_string();
        match text.as_str() {
            "count" => {
                let _ = connection.send(self.connections.len().to_string().as_bytes());
            }
            "ticks" => {
                let _ = connection.send(format!("ticks {}", self.ticks).as_bytes());
            }
            "file" => {
                let size = connection.send_file(FileSource::Asset, "blob.bin").unwrap();
                let _ = connection.send(format!("sent {size}").as_bytes());
            }
            "close" => connection.close(4000, "asked to close"),
            "panic" => panic!("deliberate realtime trap"),
            "spin" => loop {
                std::hint::black_box(1);
            },
            "flood" => {
                let chunk = vec![7u8; 512 * 1024];
                for _ in 0..40 {
                    let _ = connection.send(&chunk);
                }
            }
            other if other.starts_with("all ") => {
                for peer in &self.connections {
                    let _ = peer.send(other[4..].as_bytes());
                }
            }
            _ => {
                let _ = connection.send(&data);
            }
        }
    }
    fn close(&mut self, connection: Connection) {
        self.connections.remove(&connection);
    }
    fn tick(&mut self) {
        self.ticks += 1;
    }
}

loki_sdk::export_realtime!(handle, Echo);
