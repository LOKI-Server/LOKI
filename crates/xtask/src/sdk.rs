use crate::common::{self, Browser, Process};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    net::{SocketAddr, TcpStream},
    path::Path,
    process::Command,
    time::Duration,
};

fn wait_log(process: &mut Process, log: &Path, check: impl Fn(&str) -> bool) -> Result<String> {
    common::wait_for(180, || {
        let text = fs::read_to_string(log).unwrap_or_default();
        ensure!(process.running()?, "Preview exited: {text}");
        Ok(check(&text).then_some(text))
    })
    .with_context(|| format!("Preview log: {}", log.display()))
}
pub fn run(tool: &Path, server: &Path, offline: bool) -> Result<()> {
    let project = common::temporary("sdk-smoke")?;
    // The scaffolder requires an absent destination.
    fs::remove_dir(&project)?;
    let target = project.join("target");
    crate::run(
        Command::new(tool)
            .args(["loki", "new", "sdk-smoke", "--path"])
            .arg(&project)
            .arg("--sdk-path")
            .arg(crate::root().join("crates/loki-sdk"))
            .env("CARGO_TARGET_DIR", &target),
    )?;
    let command = |task: &str| {
        let mut command = Command::new(tool);
        command
            .args(["loki", task, "--manifest-path"])
            .arg(project.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &target);
        if offline {
            command.arg("--offline");
        }
        command
    };
    crate::run(&mut command("check"))?;
    crate::run(&mut command("package"))?;
    ensure!(
        target.join("loki/sdk-smoke-0.1.0.loki-app").is_file(),
        "SDK package was not created"
    );
    let log = project.join("preview.log");
    let profile = target.join("loki/dev/sdk-smoke");
    let mut previous = None;
    for run in 1..=2 {
        fs::write(&log, "")?;
        let mut launch = command("dev");
        launch.arg("--server").arg(server).args(["--port", "0"]);
        let mut process = Process::spawn(&mut launch, &log, true)?;
        // Even a failed assertion/interrupt must not leave the dev child alive.
        let _pid_guard = common::PidFileGuard(profile.join("server.pid"));
        let text = wait_log(&mut process, &log, |s| s.contains("Plugin: "))?;
        let dashboard = common::capture(r"Dashboard: (\S+)", &text)?;
        let plugin = common::capture(r"Plugin: (\S+)", &text)?;
        if let Some(previous) = &previous {
            ensure!(
                previous == &dashboard,
                "Restart changed the persisted issuer/port"
            );
        }
        previous = Some(dashboard.clone());
        let url = reqwest::Url::parse(&dashboard)?;
        let port = url.port().context("Preview URL omitted port")?;
        let credentials: serde_json::Value =
            serde_json::from_slice(&fs::read(profile.join("cargo-loki-dev.json"))?)?;
        let mut browser = Browser::new(port)?;
        browser.login(
            &dashboard,
            credentials["username"]
                .as_str()
                .context("Missing dev username")?,
            credentials["password"]
                .as_str()
                .context("Missing dev password")?,
        )?;
        let page = browser.get(&format!("{plugin}/"))?;
        page.ok()?;
        ensure!(
            page.text().contains("LOKI plugin"),
            "Scaffolded app did not render"
        );
        browser.get(&format!("{plugin}/static/style.css"))?.ok()?;
        if run == 1 {
            let source = project.join("src/lib.rs");
            let original = fs::read_to_string(&source)?;
            let updated = original.replace(
                "Your LOKI plugin is running.",
                "Your LOKI plugin rebuilt successfully.",
            );
            ensure!(
                updated != original,
                "Scaffold message changed; update the smoke fixture"
            );
            fs::write(&source, &updated)?;
            wait_log(&mut process, &log, |s| s.contains("Updated "))?;
            ensure!(
                browser
                    .get(&format!("{plugin}/"))?
                    .text()
                    .contains("rebuilt successfully"),
                "Watched rebuild did not update plugin"
            );
            fs::write(&source, format!("{updated}\nthis is invalid Rust\n"))?;
            wait_log(&mut process, &log, |s| s.contains("Update failed"))?;
            ensure!(
                browser
                    .get(&format!("{plugin}/"))?
                    .text()
                    .contains("rebuilt successfully"),
                "Failed build displaced working plugin"
            );
            fs::write(&source, original)?;
            wait_log(&mut process, &log, |s| s.matches("Updated ").count() >= 2)?;
        }
        process.interrupt()?;
        let socket = SocketAddr::from(([127, 0, 0, 1], port));
        ensure!(
            TcpStream::connect_timeout(&socket, Duration::from_secs(1)).is_err(),
            "Preview server remained listening after Ctrl+C"
        );
        println!(
            "PASS SDK component, host SSO, assets, persisted dev profile and Ctrl+C cleanup (run {run})"
        );
    }
    println!(
        "PASS watched rebuild and failed-build preservation\n{}",
        project.display()
    );
    Ok(())
}
