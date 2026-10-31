//! A deliberately public lifecycle example. Private apps must implement OIDC:
//! see docs/app-protocol.md and Bigshot's independent reference adapter.
use askama::Template;
use axum::{Router, response::Html, routing::get};
#[derive(Template)]
#[template(
    source = "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Branch</title><h1>{{ message }}</h1><p>A small branch of your LOKI universe.</p></html>",
    ext = "html"
)]
struct Page {
    message: String,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind: std::net::SocketAddr = std::env::var("LOKI_BIND")?.parse()?;
    anyhow::ensure!(bind.ip().is_loopback(), "loopback listener required");
    let config: serde_json::Value = serde_json::from_str(&std::env::var("LOKI_CONFIG")?)?;
    let message = config["message"]
        .as_str()
        .unwrap_or("Hello, universe.")
        .to_string();
    let app = Router::new()
        .route("/health", get(|| async { "ready" }))
        .route(
            "/",
            get(move || {
                let message = message.clone();
                async move {
                    Html(
                        Page { message }
                            .render()
                            .unwrap_or_else(|_| "Could not render this branch.".into()),
                    )
                }
            }),
        );
    axum::serve(tokio::net::TcpListener::bind(bind).await?, app).await?;
    Ok(())
}
