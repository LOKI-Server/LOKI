use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
mod dev;
mod package;
mod project;
mod scaffold;

#[derive(Parser)]
#[command(
    name = "cargo loki",
    bin_name = "cargo loki",
    version,
    about = "Develop portable LOKI plugins"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Create a Rust plugin with the SDK and Cargo metadata.
    New {
        name: String,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long, env = "LOKI_SDK_PATH")]
        sdk_path: Option<PathBuf>,
    },
    /// Compile the plugin for wasm32-wasip2.
    Build(Build),
    /// Check plugin code without producing a package.
    Check(Build),
    /// Build a release component and create a checksummed .loki-app.
    Package {
        #[command(flatten)]
        build: Build,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        debug: bool,
        #[arg(long)]
        no_build: bool,
    },
    /// Build, install and watch a plugin in an isolated local LOKI preview.
    Dev {
        #[command(flatten)]
        build: Build,
        #[arg(long, env = "LOKI_SERVER")]
        server: Option<PathBuf>,
        #[arg(long, default_value_t = 7077)]
        port: u16,
        #[arg(long)]
        no_watch: bool,
    },
}
#[derive(Args, Clone, Debug)]
pub struct Build {
    #[arg(long, default_value = "Cargo.toml")]
    pub manifest_path: PathBuf,
    #[arg(short, long)]
    pub package: Option<String>,
    #[arg(long)]
    pub release: bool,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub locked: bool,
}
fn main() -> Result<()> {
    let mut args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|s| s == "loki") {
        args.remove(1);
    }
    let cli = Cli::parse_from(args);
    match cli.command {
        Command::New {
            name,
            path,
            sdk_path,
        } => scaffold::create(&name, path.as_deref(), sdk_path.as_deref()),
        Command::Build(build) => {
            let project = project::Project::load(&build)?;
            println!("{}", project.build(&build)?.display());
            Ok(())
        }
        Command::Check(build) => project::Project::load(&build)?.check(&build),
        Command::Package {
            mut build,
            output,
            debug,
            no_build,
        } => {
            anyhow::ensure!(
                !(build.release && debug),
                "--release and --debug cannot be combined"
            );
            build.release = !debug;
            let project = project::Project::load(&build)?;
            let wasm = if no_build {
                project.artifact(build.release)?
            } else {
                project.build(&build)?
            };
            println!(
                "{}",
                package::create(&project, &wasm, output.as_deref())?.display()
            );
            Ok(())
        }
        Command::Dev {
            build,
            server,
            port,
            no_watch,
        } => dev::run(build, server, port, no_watch),
    }
}
