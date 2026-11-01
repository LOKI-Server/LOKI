//! Test component for LOKI's realtime instances. Commands arrive as text messages.
use loki_sdk::realtime::{Connection, FileSource, OpenInfo, RealtimeApp};
use loki_sdk::panel::{Action, Panel, PanelResponse, Section};
use loki_sdk::{Request, Response, Result};
use std::collections::BTreeSet;

fn handle(request: Request) -> Result<Response> {
    match request.path() {
        "/health" => Ok(Response::text("ready")),
        "/_loki/panel" => panel(request),
        "/whoami" => Ok(Response::text(
            request
                .context()
                .user
                .as_ref()
                .map_or("anonymous".into(), |u| u.username.clone()),
        )),
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

/// A counter everyone with access can see; admins can bump it and upload a file.
fn panel(request: Request) -> Result<Response> {
    let context = request.context();
    let storage = context.storage();
    let mut count: u64 = storage.get_json("count")?.unwrap_or(0);
    let mut notice = None;
    if let Some(action) = &context.panel_action {
        match action.action.as_str() {
            "bump" => {
                count += action.value.parse::<u64>().unwrap_or(1);
                storage.set_json("count", &count)?;
                notice = Some("Bumped.");
            }
            "upload" => {
                let file = action.files.first().ok_or_else(|| loki_sdk::Error::bad("No file."))?;
                file.retain(format!("panel/{}", file.name))?;
                notice = Some("Stored.");
            }
            "broken" => return Panel::new().section(Section::new("x".repeat(500))).respond(),
            _ => return PanelResponse::error(Panel::new(), "Unknown action.").respond(),
        }
    }
    let mut panel = Panel::new().section(
        Section::new("Echo")
            .stats([("Count", count.to_string())])
            .code("LAN", format!("{} {}", context.lan_bind, context.lan_ips.join(","))),
    );
    if context.user.as_ref().is_some_and(|u| u.has_role("admin")) {
        panel = panel.section(
            Section::new("Admin")
                .buttons([Action::new("bump", "Bump").value("2").primary()])
                .upload("upload", "File", "", "Upload"),
        );
    }
    panel.respond_with(notice)
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
        let user = info.user.map(|u| format!(" as {}", u.username)).unwrap_or_default();
        let _ = connection.send(format!("open {}{user}", !info.remote.is_empty()).as_bytes());
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
