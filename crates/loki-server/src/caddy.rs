use crate::{CaddyMode, Error, Result, Settings, State, auth, platform};
use serde_json::{Value, json};
use tokio::process::Command;

pub fn hostname(host: &str) -> Result<()> {
    if host.len() > 253
        || host.is_empty()
        || host.split('.').count() < 2
        || host.split('.').any(|p| {
            p.is_empty()
                || p.len() > 63
                || p.starts_with('-')
                || p.ends_with('-')
                || !p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(Error::bad(
            "Enter a hostname such as app.example.com, without a scheme, port, or path.",
        ));
    }
    Ok(())
}
pub fn upstream(value: &str) -> Result<()> {
    let url = url::Url::parse(value)
        .map_err(|_| Error::bad("Enter an HTTP(S) upstream URL, including its port."))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(Error::bad(
            "Use an HTTP(S) upstream origin without credentials or a path.",
        ));
    }
    Ok(())
}
pub fn admin_origin(settings: &Settings) -> Result<String> {
    let url = url::Url::parse(&settings.caddy_admin)
        .map_err(|_| Error::bad("Invalid Caddy admin address."))?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::bad(
            "Caddy management must use an HTTP loopback origin.",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').into())
}
pub async fn render(state: &State, settings: &Settings) -> Result<String> {
    let public = url::Url::parse(&settings.public_url)?;
    let mut text = String::new();
    let admin = admin_origin(settings)?;
    let admin_url = url::Url::parse(&admin)?;
    text.push_str(&format!(
        "{{\n admin {}:{}\n}}\n",
        admin_url.host_str().unwrap_or("127.0.0.1"),
        admin_url.port_or_known_default().unwrap_or(2019)
    ));
    if !settings.caddy_enabled {
        return Ok(text);
    }
    if public.scheme() == "https" {
        text.push_str(&format!(
            "{} {{\n reverse_proxy {}\n}}\n",
            public
                .host_str()
                .ok_or_else(|| Error::bad("Missing public hostname."))?,
            state.0.bind
        ));
    }
    let routes: Vec<(String, String, String, bool)> =
        sqlx::query_as("SELECT id,hostname,upstream,gated FROM routes WHERE enabled=1")
            .fetch_all(&state.0.db)
            .await?;
    for (id, host, up, gated) in routes {
        hostname(&host)?;
        upstream(&up)?;
        text.push_str(&format!("{host} {{\n"));
        if gated {
            text.push_str(&format!(" handle /_loki/* {{\n  reverse_proxy {} {{\n   header_up X-Loki-Resource {id}\n  }}\n }}\n handle {{\n  forward_auth {} {{\n   uri /gateway/check/{id}\n  }}\n  reverse_proxy {up}\n }}\n",state.0.bind,state.0.bind));
        } else {
            text.push_str(&format!(" reverse_proxy {up}\n"));
        }
        text.push_str("}\n");
    }
    let apps: Vec<(String, String, i64, String)> = sqlx::query_as(
        "SELECT id,hostname,port,manifest FROM apps WHERE enabled=1 AND hostname!=''",
    )
    .fetch_all(&state.0.db)
    .await?;
    let runtimes = state.0.runtimes.lock().await;
    for (id, host, port, manifest) in apps {
        if runtimes.get(&id).is_some_and(|r| r.ready) {
            hostname(&host)?;
            let upstream =
                if serde_json::from_str::<crate::apps::Manifest>(&manifest)?.runtime == "wasi" {
                    state.0.bind.to_string()
                } else {
                    format!("127.0.0.1:{port}")
                };
            text.push_str(&format!("{host} {{\n reverse_proxy {upstream}\n}}\n"));
        }
    }
    Ok(text)
}
async fn current_config(settings: &Settings) -> Result<Value> {
    let admin = admin_origin(settings)?;
    let response = platform::http()
        .get(format!("{admin}/config/"))
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(%error, %admin, "Could not reach Caddy management");
            let guidance = if settings.caddy_mode == CaddyMode::Existing {
                "Existing mode requires an already running Caddy. Start it separately, or disable Caddy management and save, then select LOKI-managed Caddy process and enable it."
            } else {
                "Check the Caddy logs and management address."
            };
            Error::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                format!("Cannot reach Caddy at {admin}. {guidance}"),
            )
        })?;
    if !response.status().is_success() {
        return Err(Error::bad(format!(
            "Caddy management at {admin} returned HTTP {}. Check the management address and Caddy configuration.",
            response.status()
        )));
    }
    response.json().await.map_err(|error| {
        tracing::warn!(%error, %admin, "Invalid Caddy management response");
        Error::bad(format!(
            "Caddy management at {admin} did not return valid configuration JSON. Check that this address belongs to Caddy."
        ))
    })
}
async fn candidate(state: &State, settings: &Settings, live: Option<&Value>) -> Result<Value> {
    let text = render(state, settings).await?;
    let path = state.0.data.join("caddy/candidate.Caddyfile");
    tokio::fs::write(&path, &text).await?;
    let mut command = Command::new(&settings.caddy_binary);
    command
        .args(["adapt", "--config"])
        .arg(&path)
        .args(["--adapter", "caddyfile", "--validate"])
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000);
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), command.output())
        .await
        .map_err(|_| Error::bad("Caddy validation timed out."))??;
    if !output.status.success() {
        return Err(Error::bad(format!(
            "Caddy rejected the configuration: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(3000)
                .collect::<String>()
        )));
    }
    let mut generated: Value = serde_json::from_slice(&output.stdout)?;
    if !settings.caddy_base.trim().is_empty() {
        let mut base: Value = serde_json::from_str(&settings.caddy_base).map_err(|_| {
            Error::bad(
                "The Caddy base must be a JSON configuration (use Adopt existing to capture it).",
            )
        })?;
        let servers = generated
            .pointer("/apps/http/servers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if !base.is_object() {
            return Err(Error::bad("Caddy base must be an object."));
        }
        if base.get("apps").is_none() {
            base["apps"] = json!({});
        }
        if base["apps"].get("http").is_none() {
            base["apps"]["http"] = json!({});
        }
        if base["apps"]["http"].get("servers").is_none() {
            base["apps"]["http"]["servers"] = json!({});
        }
        let target = base["apps"]["http"]["servers"]
            .as_object_mut()
            .ok_or_else(|| Error::bad("Invalid Caddy servers object."))?;
        // Merge shared listeners into existing servers so adoption does not bind 443 twice.
        for (name, server) in servers {
            let matching = target
                .iter()
                .find(|(_, v)| v.get("listen") == server.get("listen"))
                .map(|(k, _)| k.clone());
            if let Some(name) = matching {
                let existing = target.get_mut(&name).unwrap();
                let mut routes = server["routes"].as_array().cloned().unwrap_or_default();
                routes.extend(existing["routes"].as_array().cloned().unwrap_or_default());
                existing["routes"] = json!(routes);
            } else {
                target.insert(format!("loki_{name}"), server);
            }
        }
        base["admin"] = generated["admin"].clone();
        generated = base;
    }
    if let Some(live) = live {
        // The API URL is a connection address, not a request to rebind Caddy.
        // Preserve even an omitted admin block: its effective default may come
        // from CADDY_ADMIN in the existing process. Overlapping localhost/IP
        // listeners cannot be rebound during a Windows reload.
        match live.get("admin") {
            Some(admin) => generated["admin"] = admin.clone(),
            None => {
                generated.as_object_mut().unwrap().remove("admin");
            }
        }
    }
    // Validate the merged document as well as the generated fragment.
    let path = state.0.data.join("caddy/candidate.json");
    tokio::fs::write(&path, serde_json::to_vec_pretty(&generated)?).await?;
    let mut cmd = Command::new(&settings.caddy_binary);
    cmd.arg("validate")
        .arg("--config")
        .arg(path)
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000);
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), cmd.output())
        .await
        .map_err(|_| Error::bad("Caddy validation timed out."))??;
    if !output.status.success() {
        return Err(Error::bad(format!(
            "Caddy rejected the merged configuration: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(3000)
                .collect::<String>()
        )));
    }
    Ok(generated)
}
pub async fn apply(state: &State) -> Result<()> {
    let settings = state.0.settings.read().await.clone();
    apply_settings(state, &settings).await
}
pub async fn apply_settings(state: &State, settings: &Settings) -> Result<()> {
    if !settings.caddy_enabled {
        if let Some(mut process) = state.0.caddy.lock().await.take() {
            process.stop().await?;
            return Ok(());
        }
        if !state.0.settings.read().await.caddy_enabled {
            return Ok(());
        }
    }
    let live = if settings.caddy_mode == CaddyMode::Existing {
        match current_config(settings).await {
            Ok(value) => Some(value),
            Err(error)
                if !settings.caddy_enabled
                    && error.status == axum::http::StatusCode::SERVICE_UNAVAILABLE =>
            {
                // Let the owner leave existing mode when its external process is
                // gone. We cannot update routes, but disabling local management
                // must not require starting that process first.
                tracing::warn!(
                    "Disabling Caddy management while the existing instance is unreachable; its remote configuration was not changed"
                );
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let value = candidate(state, settings, live.as_ref()).await?;
    let admin = admin_origin(settings)?;
    let client = platform::http();
    if settings.caddy_mode == CaddyMode::Managed {
        let mut running = state.0.caddy.lock().await;
        if let Some(p) = running.as_mut()
            && p.child.try_wait()?.is_some()
        {
            *running = None;
        }
        if running.is_none() {
            if client.get(format!("{admin}/config/")).send().await.is_ok() {
                return Err(Error::bad(
                    "Another Caddy already uses this management address. Choose another address or explicitly adopt the existing instance.",
                ));
            }
            let bootstrap = state.0.data.join("caddy/bootstrap.json");
            tokio::fs::write(
                &bootstrap,
                serde_json::to_vec(&json!({"admin":value["admin"]}))?,
            )
            .await?;
            let mut command = Command::new(&settings.caddy_binary);
            command
                .arg("run")
                .arg("--config")
                .arg(bootstrap)
                .env("XDG_DATA_HOME", state.0.data.join("caddy/storage"))
                .env("XDG_CONFIG_HOME", state.0.data.join("caddy/config"));
            command
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let mut process = platform::Process::spawn(&mut command)?;
            if let Some(stream) = process.child.stdout.take() {
                tokio::spawn(crate::apps::capture(
                    stream,
                    state.0.data.join("logs/caddy.log"),
                ));
            }
            if let Some(stream) = process.child.stderr.take() {
                tokio::spawn(crate::apps::capture(
                    stream,
                    state.0.data.join("logs/caddy.stderr.log"),
                ));
            }
            *running = Some(process);
            let mut ready = false;
            for _ in 0..30 {
                if running.as_mut().unwrap().child.try_wait()?.is_some() {
                    break;
                }
                if client.get(format!("{admin}/config/")).send().await.is_ok() {
                    ready = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if !ready {
                return Err(Error::bad(
                    "Caddy management did not become ready. Check for a port conflict.",
                ));
            }
        }
    }
    let previous = match live {
        Some(value) => value,
        None => current_config(settings).await?,
    };
    let backup =
        state
            .0
            .data
            .join("backups")
            .join(format!("caddy-{}-{}.json", auth::now(), auth::id()));
    tokio::fs::write(backup, serde_json::to_vec(&previous)?).await?;
    let response = client
        .post(format!("{admin}/load"))
        .json(&value)
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            tokio::fs::write(
                state.0.data.join("caddy/active.json"),
                serde_json::to_vec_pretty(&value)?,
            )
            .await?;
            Ok(())
        }
        failed => {
            let rollback = client
                .post(format!("{admin}/load"))
                .json(&previous)
                .send()
                .await;
            let restored = rollback.is_ok_and(|r| r.status().is_success());
            let detail = match failed {
                Ok(r) => r.text().await.unwrap_or_default(),
                Err(e) => e.to_string(),
            };
            Err(Error::bad(format!(
                "Caddy apply failed; rollback {}. {}",
                if restored { "succeeded" } else { "also failed" },
                detail.chars().take(2000).collect::<String>()
            )))
        }
    }
}
pub async fn preview(state: &State, settings: &Settings) -> Result<Value> {
    let live = if settings.caddy_mode == CaddyMode::Existing {
        Some(current_config(settings).await?)
    } else {
        None
    };
    candidate(state, settings, live.as_ref()).await
}
pub async fn adopt(state: &State) -> Result<String> {
    let settings = state.0.settings.read().await.clone();
    let value = current_config(&settings).await?;
    let json = serde_json::to_string_pretty(&value)?;
    tokio::fs::write(
        state.0.data.join("backups/caddy-before-adoption.json"),
        &json,
    )
    .await?;
    Ok(json)
}
