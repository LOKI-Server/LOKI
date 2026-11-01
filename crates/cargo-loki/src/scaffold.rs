use anyhow::{Result, ensure};
use std::path::Path;
pub fn create(name: &str, path: Option<&Path>, sdk: Option<&Path>) -> Result<()> {
    ensure!(
        loki_sdk::manifest::valid_id(name)
            && !name.starts_with("loki-")
            && name.len() <= 63
            && !name.ends_with('-'),
        "Use a lowercase plugin name of at most 63 letters, digits or hyphens; the loki- prefix is reserved."
    );
    let path = super::project::absolute(path.unwrap_or_else(|| Path::new(name)))?;
    ensure!(
        !path.exists(),
        "{} already exists; choose a new directory",
        path.display()
    );
    // cargo-loki and loki-sdk share the workspace version.
    let sdk_version = env!("CARGO_PKG_VERSION");
    let dependency = if let Some(sdk) = sdk {
        format!(
            "{{ version = \"{sdk_version}\", path = {} }}",
            toml::Value::String(std::fs::canonicalize(sdk)?.to_string_lossy().into_owned())
        )
    } else {
        format!("\"{sdk_version}\"")
    };
    let cargo = format!(
        "[package]\nname = {name:?}\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\ncrate-type = [\"cdylib\", \"rlib\"]\n\n[dependencies]\nloki-sdk = {dependency}\n\n[package.metadata.loki]\nid = {name:?}\nname = {name:?}\nhealth = \"/health\"\nassets = [\"static\"]\n\n[profile.release]\nlto = true\nopt-level = \"s\"\nstrip = true\npanic = \"abort\"\n"
    );
    let source = format!(
        r####"use loki_sdk::{{Request, Response, Result, Router}};

fn home(_request: Request) -> Result<Response> {{
    Ok(Response::html(r##"<!doctype html><html lang="en"><meta charset="utf-8"><title>{name}</title><link rel="stylesheet" href="/static/style.css"><main><h1>{name}</h1><p>Your LOKI plugin is running.</p></main></html>"##))
}}
fn health(_request: Request) -> Result<Response> {{
    Ok(Response::text("ready"))
}}
fn stylesheet(_request: Request) -> Result<Response> {{
    loki_sdk::asset("static/style.css")
}}
fn handle(request: Request) -> Result<Response> {{
    Router::new()
        .get("/", home)
        .get("/health", health)
        .get("/static/style.css", stylesheet)
        .handle(request)
}}
loki_sdk::export!(handle);
"####
    );
    std::fs::create_dir_all(path.join("src"))?;
    std::fs::create_dir(path.join("static"))?;
    // A generated plugin is independent, including when created under another workspace.
    std::fs::write(path.join("Cargo.toml"), format!("{cargo}\n[workspace]\n"))?;
    std::fs::write(path.join("src/lib.rs"), source)?;
    std::fs::write(
        path.join("static/style.css"),
        "body{font:18px system-ui;background:#0a1510;color:#d8efdf;margin:4rem}main{max-width:48rem;margin:auto}h1{color:#7de7a7}",
    )?;
    std::fs::write(path.join(".gitignore"), "/target/\n*.loki-app\n")?;
    std::fs::write(
        path.join("README.md"),
        format!(
            "# {name}\n\n```sh\nrustup target add wasm32-wasip2\ncargo loki build\ncargo loki package\ncargo loki dev --server PATH_TO_LOKI_SERVER\n```\n\nLOKI manages HTTP, identity and storage. No WIT binding or packaging script is needed.\n"
        ),
    )?;
    println!(
        "Created {}\nNext: cd {} and run cargo loki package",
        path.display(),
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scaffold_is_independent_and_does_not_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("example");
        create("example", Some(&path), None).unwrap();
        let manifest: toml::Value = std::fs::read_to_string(path.join("Cargo.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(manifest["package"]["version"].as_str(), Some("0.1.0"));
        assert!(manifest.get("workspace").is_some());
        let source = std::fs::read_to_string(path.join("src/lib.rs")).unwrap();
        assert!(source.contains("loki_sdk::export!(handle)"));
        assert!(!source.contains("wit_bindgen"));
        assert!(create("example", Some(&path), None).is_err());
        for name in ["../bad", "LOKI", "loki-reserved", "bad-", ""] {
            assert!(create(name, Some(&temp.path().join("invalid")), None).is_err());
        }
    }
}
