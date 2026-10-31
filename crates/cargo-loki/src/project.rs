use crate::Build;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
pub struct CargoPackage {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub manifest_path: PathBuf,
    pub metadata: Value,
    pub targets: Vec<Target>,
}
#[derive(Deserialize)]
pub struct Target {
    pub name: String,
    pub crate_types: Vec<String>,
}
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<CargoPackage>,
    target_directory: PathBuf,
    workspace_default_members: Vec<String>,
}
pub struct Project {
    pub package: CargoPackage,
    pub root: PathBuf,
    pub target: PathBuf,
}
pub fn cargo() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}
fn flags(command: &mut Command, options: &Build) {
    if options.offline {
        command.arg("--offline");
    }
    if options.locked {
        command.arg("--locked");
    }
}
impl Project {
    pub fn load(options: &Build) -> Result<Self> {
        let manifest =
            std::fs::canonicalize(&options.manifest_path).context("Cannot locate Cargo.toml")?;
        let mut command = cargo();
        command
            .args([
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--manifest-path",
            ])
            .arg(&manifest);
        flags(&mut command, options);
        let output = command
            .output()
            .context("Could not run Cargo. Install Rust and Cargo first.")?;
        ensure!(
            output.status.success(),
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
        let current = metadata
            .packages
            .iter()
            .find(|package| {
                std::fs::canonicalize(&package.manifest_path).is_ok_and(|path| path == manifest)
            })
            .map(|package| package.id.clone());
        let mut candidates: Vec<_> = metadata
            .packages
            .into_iter()
            .filter(|p| p.metadata.get("loki").is_some())
            .filter(|p| {
                if let Some(name) = &options.package {
                    &p.name == name || &p.id == name
                } else if let Some(current) = &current {
                    &p.id == current
                } else {
                    metadata.workspace_default_members.contains(&p.id)
                }
            })
            .collect();
        ensure!(
            candidates.len() == 1,
            "Select one plugin containing [package.metadata.loki]. Run inside its directory or use --manifest-path and -p NAME."
        );
        let package = candidates.remove(0);
        ensure!(
            package
                .targets
                .iter()
                .any(|t| t.crate_types.iter().any(|k| k == "cdylib")),
            "The plugin must declare [lib] crate-type = [\"cdylib\"]."
        );
        let root = package
            .manifest_path
            .parent()
            .context("Invalid manifest path")?
            .to_path_buf();
        Ok(Self {
            package,
            root,
            target: metadata.target_directory,
        })
    }
    pub fn command(&self, options: &Build, operation: &str) -> Command {
        let mut command = cargo();
        command
            .arg(operation)
            .arg("--manifest-path")
            .arg(&options.manifest_path)
            .args([
                "--package",
                &self.package.id,
                "--target",
                "wasm32-wasip2",
                "--lib",
            ]);
        if options.release {
            command.arg("--release");
        }
        flags(&mut command, options);
        command
    }
    pub fn check(&self, options: &Build) -> Result<()> {
        ensure!(
            self.command(options, "check").status()?.success(),
            "Plugin check failed. If the target is missing, run rustup target add wasm32-wasip2."
        );
        Ok(())
    }
    pub fn build(&self, options: &Build) -> Result<PathBuf> {
        let output = self
            .command(options, "build")
            .arg("--message-format=json-render-diagnostics")
            .output()?;
        std::io::Write::write_all(&mut std::io::stderr(), &output.stderr)?;
        let mut artifact = None;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(message) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if message["reason"] == "compiler-message"
                && let Some(text) = message["message"]["rendered"].as_str()
            {
                eprint!("{text}");
            }
            if message["reason"] == "compiler-artifact"
                && message["package_id"] == self.package.id
                && let Some(files) = message["filenames"].as_array()
            {
                for file in files {
                    if let Some(path) = file.as_str().filter(|s| s.ends_with(".wasm")) {
                        artifact = Some(PathBuf::from(path));
                    }
                }
            }
        }
        ensure!(
            output.status.success(),
            "Plugin build failed. If needed, run rustup target add wasm32-wasip2."
        );
        artifact.context("Cargo did not report a .wasm component artifact")
    }
    pub fn artifact(&self, release: bool) -> Result<PathBuf> {
        let target = self
            .package
            .targets
            .iter()
            .find(|t| t.crate_types.iter().any(|t| t == "cdylib"))
            .context("No component library")?;
        let path = self
            .target
            .join("wasm32-wasip2")
            .join(if release { "release" } else { "debug" })
            .join(format!("{}.wasm", target.name.replace('-', "_")));
        if !path.is_file() {
            bail!(
                "No built component at {}; run cargo loki build{} first",
                path.display(),
                if release { " --release" } else { "" }
            );
        }
        Ok(path)
    }
    pub fn config(&self) -> Result<Value> {
        let value = self.package.metadata["loki"].clone();
        ensure!(value.is_object(), "[package.metadata.loki] must be a table");
        Ok(value)
    }
    pub fn app_id(&self) -> Result<String> {
        match self.config()?.get("id") {
            Some(value) => value
                .as_str()
                .map(str::to_owned)
                .context("Plugin ID must be a string"),
            None => Ok(self.package.name.replace('_', "-")),
        }
    }
}
pub fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_member_takes_precedence_over_workspace_defaults() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Cargo.toml"),"[workspace]\nmembers=[\"first\",\"second\"]\ndefault-members=[\"first\"]\nresolver=\"3\"\n").unwrap();
        for name in ["first", "second"] {
            let path = temp.path().join(name);
            std::fs::create_dir_all(path.join("src")).unwrap();
            std::fs::write(path.join("src/lib.rs"), "").unwrap();
            std::fs::write(path.join("Cargo.toml"),format!("[package]\nname={name:?}\nversion=\"0.1.0\"\nedition=\"2024\"\n[lib]\ncrate-type=[\"cdylib\"]\n[package.metadata.loki]\nid={name:?}\n")).unwrap();
        }
        let mut options = Build {
            manifest_path: temp.path().join("second/Cargo.toml"),
            package: None,
            release: false,
            offline: true,
            locked: false,
        };
        assert_eq!(Project::load(&options).unwrap().package.name, "second");
        options.manifest_path = temp.path().join("Cargo.toml");
        assert_eq!(Project::load(&options).unwrap().package.name, "first");
        options.package = Some("second".into());
        assert_eq!(Project::load(&options).unwrap().package.name, "second");
    }
}
