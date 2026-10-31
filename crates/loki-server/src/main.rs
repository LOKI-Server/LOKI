#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
use clap::{Parser, Subcommand};
use fs2::FileExt;
use loki_server::{State, apps, auth, caddy, platform, web};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Parser)]
#[command(version, about = "LOKI — your services, held together")]
struct Cli {
    #[arg(long, env = "LOKI_HOME")]
    data_dir: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1:7070")]
    bind: std::net::SocketAddr,
    #[arg(long, env = "LOKI_NO_TRAY")]
    no_tray: bool,
    #[arg(long, hide = true)]
    wait_for_pid: Option<u32>,
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Subcommand)]
enum Action {
    /// Inspect local state, DNS and listeners.
    Doctor,
    /// Issue a local owner recovery link; invalidates all owner sessions and MFA.
    RecoverOwner,
    /// Restore a LOKI backup into an empty data directory while the server is stopped.
    Restore { backup: PathBuf },
}
#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::*;
        if GetConsoleWindow().is_null() {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
    let cli = Cli::parse();
    if let Some(pid) = cli.wait_for_pid {
        loki_server::storage::wait_for_pid(pid).await?;
    }
    let data = cli
        .data_dir
        .unwrap_or_else(loki_server::storage::default_home);
    std::fs::create_dir_all(&data)?;
    let data = std::fs::canonicalize(data)?;
    platform::private_directory(&data)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data.join("server.lock"))?;
    lock.try_lock_exclusive().map_err(|_| {
        anyhow::anyhow!(
            "LOKI is already running with this data directory. Stop it before recovery or restore."
        )
    })?;
    if let Some(Action::Restore { backup }) = &cli.command {
        loki_server::storage::restore(backup, &data)?;
        println!("Restored into {}", data.display());
        return Ok(());
    }
    let log_path = data.join("server.log");
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "loki_server=info".into()),
        )
        .with_writer(move || platform::RollingLog(log_path.clone()))
        .init();
    let state = State::open(data, cli.bind).await?;
    std::fs::write(
        state.0.data.join("server.pid"),
        std::process::id().to_string(),
    )?;
    match cli.command {
        Some(Action::Doctor) => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &web::diagnostics(&state)
                        .await
                        .map_err(|e| anyhow::anyhow!(e.1))?
                )?
            );
            return Ok(());
        }
        Some(Action::RecoverOwner) => {
            let uid: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE owner=1")
                .fetch_optional(&state.0.db)
                .await?;
            let uid = uid
                .ok_or_else(|| anyhow::anyhow!("No owner exists. Start LOKI and finish setup."))?;
            let raw = auth::random();
            let mut tx = state.0.db.begin().await?;
            for table in [
                "sessions",
                "tokens",
                "codes",
                "gateway_sessions",
                "recovery_codes",
                "resets",
            ] {
                sqlx::query(&format!("DELETE FROM {table} WHERE user_id=?"))
                    .bind(&uid)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query(
                "UPDATE users SET totp=NULL,totp_pending=NULL,status='approved',auth_version=auth_version+1 WHERE id=?",
            )
            .bind(&uid)
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO resets(id,user_id,expires) VALUES(?,?,?)")
                .bind(auth::digest(&raw))
                .bind(uid)
                .bind(auth::now() + 3600)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            println!(
                "Start LOKI, then open this one-time link within one hour:\n{}/reset#{}",
                state.0.settings.read().await.public_url,
                raw
            );
            return Ok(());
        }
        _ => {}
    }
    if let Some(token) = state.0.setup_token.lock().await.as_ref() {
        println!(
            "LOKI setup: http://{}/setup#{}\nData: {}",
            cli.bind,
            token,
            state.0.data.display()
        );
        platform::write_private(&state.0.data.join("setup-token.txt"), token.as_bytes())
            .or_else(|_| std::fs::write(state.0.data.join("setup-token.txt"), token))?;
        if state.0.settings.read().await.public_url == "http://127.0.0.1:7070" {
            state.0.settings.write().await.public_url = format!("http://{}", cli.bind);
        }
    } else {
        let _ = std::fs::remove_file(state.0.data.join("setup-token.txt"));
    }
    let listener = tokio::net::TcpListener::bind(cli.bind).await?;
    if let Err(e) = caddy::apply(&state).await {
        tracing::error!(error=%e.1,"Caddy reconciliation failed; dashboard remains available locally");
    }
    let monitor = tokio::spawn(apps::monitor(state.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let tray = if cli.no_tray {
        None
    } else {
        tray(state.clone(), stop.clone())
    };
    println!("LOKI is listening on http://{}", cli.bind);
    let shutdown = state.clone();
    axum::serve(listener,web::router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(async move{
        #[cfg(unix)]let terminate=async{if let Ok(mut signal)=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()){signal.recv().await;}};
        #[cfg(not(unix))]let terminate=std::future::pending::<()>();
        tokio::select!{_=tokio::signal::ctrl_c()=>{},_=shutdown.0.shutdown.notified()=>{},_=terminate=>{}}
    }).await?;
    monitor.abort();
    stop.store(true, Ordering::Relaxed);
    if let Some(tray) = tray {
        let _ = tray.join();
    }
    let _guard = state.0.operations.lock().await;
    let ids = state
        .0
        .runtimes
        .lock()
        .await
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for id in ids {
        let _ = apps::stop_inner(&state, &id).await;
    }
    if let Some(mut caddy) = state.0.caddy.lock().await.take() {
        let _ = caddy.stop().await;
    }
    state.0.db.close().await;
    drop(lock);
    Ok(())
}
#[cfg(not(windows))]
fn tray(_: State, _: Arc<AtomicBool>) -> Option<std::thread::JoinHandle<()>> {
    None
}
#[cfg(windows)]
fn tray(state: State, stop: Arc<AtomicBool>) -> Option<std::thread::JoinHandle<()>> {
    Some(std::thread::spawn(move || {
        use tray_icon::{
            Icon, TrayIconBuilder,
            menu::{Menu, MenuEvent, MenuItem},
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let menu = Menu::new();
        let open = MenuItem::new("Open LOKI", true, None);
        let exit = MenuItem::new("Exit", true, None);
        if menu.append_items(&[&open, &exit]).is_err() {
            return;
        }
        let mut rgba = vec![0u8; 32 * 32 * 4];
        for y in 0..32usize {
            for x in 0..32usize {
                let at = (y * 32 + x) * 4;
                let rune = (x.abs_diff(15) <= 1 && y > 5 && y < 27)
                    || ((y as isize - x as isize).abs() <= 1 && x > 7 && x < 16)
                    || ((y + x).abs_diff(30) <= 1 && x > 14 && x < 24);
                rgba[at..at + 4].copy_from_slice(if rune {
                    &[95, 235, 161, 255]
                } else {
                    &[10, 27, 20, 255]
                });
            }
        }
        let Ok(icon) = Icon::from_rgba(rgba, 32, 32) else {
            return;
        };
        let Ok(_tray) = TrayIconBuilder::new()
            .with_tooltip("LOKI — your services, held together")
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .build()
        else {
            return;
        };
        while !stop.load(Ordering::Relaxed) {
            unsafe {
                let mut message = std::mem::zeroed();
                while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == exit.id() {
                    state.0.shutdown.notify_one();
                } else if event.id == open.id() {
                    let _ = std::process::Command::new("explorer.exe")
                        .arg(if let Ok(token) = state.0.setup_token.try_lock() {
                            match token.as_ref() {
                                Some(token) => format!("http://{}/setup#{}", state.0.bind, token),
                                None => format!("http://{}", state.0.bind),
                            }
                        } else {
                            format!("http://{}", state.0.bind)
                        })
                        .spawn();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }))
}
