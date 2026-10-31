use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

mod benchmark;
mod bundle;
mod common;
mod sdk;
mod smoke;

#[derive(Parser)]
#[command(
    version,
    about = "LOKI repository build, verification and release tasks"
)]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Assemble a portable bundle with a checksum-verified official Caddy binary.
    Bundle(bundle::Options),
    /// Measure initialized idle resident memory and one-core CPU consumption.
    Benchmark(benchmark::Options),
    /// Verify setup, HTTPS/local cookies, relocation and supervised restart.
    SetupSmoke { binary: PathBuf },
    /// Verify SDK scaffolding, packaging, watched rebuilds and preview cleanup.
    SdkSmoke {
        #[arg(long)]
        tool: PathBuf,
        #[arg(long)]
        server: PathBuf,
        #[arg(long)]
        offline: bool,
    },
    /// Verify Branch Notes storage, identity, rollback and backup/restore.
    StorageSmoke {
        #[arg(long)]
        loki: PathBuf,
        #[arg(long)]
        package: PathBuf,
    },
    /// Verify Bigshot SSO, voting, media streaming, rollback and backup/restore.
    #[command(alias = "smoke")]
    WasiSmoke {
        #[arg(long)]
        loki: PathBuf,
        #[arg(long)]
        package: PathBuf,
        /// Retain the disposable preview after all checks pass.
        #[arg(long)]
        keep_running: bool,
    },
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .into()
}
fn absolute(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("Cannot find {}", path.display()))
}
fn run(command: &mut Command) -> Result<()> {
    let status = command.status().context("Could not start command")?;
    anyhow::ensure!(status.success(), "Command failed ({status}): {command:?}");
    Ok(())
}
fn main() -> Result<()> {
    let args = Cli::parse();
    std::env::set_current_dir(root())?;
    match args.task {
        Task::Bundle(options) => bundle::run(options),
        Task::Benchmark(options) => benchmark::run(options),
        Task::SetupSmoke { binary } => smoke::setup(&absolute(&binary)?),
        Task::SdkSmoke {
            tool,
            server,
            offline,
        } => sdk::run(&absolute(&tool)?, &absolute(&server)?, offline),
        Task::StorageSmoke { loki, package } => {
            smoke::storage(&absolute(&loki)?, &absolute(&package)?)
        }
        Task::WasiSmoke {
            loki,
            package,
            keep_running,
        } => smoke::wasi(&absolute(&loki)?, &absolute(&package)?, keep_running),
    }
}
