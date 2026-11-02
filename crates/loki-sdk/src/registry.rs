//! The LOKI app registry: one `apps/<id>/app.json` per app in the registry repo,
//! gathered into the `index.json` that LOKI and the marketplace site read.
use crate::{
    Error, Result,
    manifest::{CAPABILITIES, Manifest, safe_relative, valid_id},
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The registry format this SDK reads and writes.
pub const FORMAT: u32 = 1;
/// The largest package LOKI imports.
pub const MAX_PACKAGE: u64 = 536870912;

/// The latest release of one app. Updating an app replaces its entry.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Where the app's code lives, so people can review it before installing.
    #[serde(default)]
    pub source: String,
    pub version: String,
    /// Where the `.loki-app` downloads from.
    pub url: String,
    /// SHA-256 of the whole `.loki-app`, so LOKI installs only the reviewed file.
    pub sha256: String,
    pub size: u64,
    /// The `loki-sdk` the package was built with. An app needs at least the
    /// matching LOKI.
    pub sdk: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Set only in `index.json`: the icon's path from the index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Index {
    pub registry: u32,
    #[serde(default)]
    pub generated: String,
    pub apps: Vec<Entry>,
}

/// An `https://` link. Plain `http://` is accepted only on this computer, for
/// testing a local registry; the public registry's build refuses it.
fn link(value: &str) -> bool {
    [
        "https://",
        "http://127.0.0.1:",
        "http://127.0.0.1/",
        "http://localhost:",
        "http://localhost/",
    ]
    .iter()
    .any(|p| value.starts_with(p))
        && value.len() > 8
        && value.len() <= 2048
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

impl Entry {
    /// The entry for a package, before it knows where the package is hosted.
    pub fn for_package(manifest: &Manifest, url: &str, sha256: &str, size: u64) -> Result<Self> {
        let sdk = manifest
            .built_with
            .as_ref()
            .ok_or_else(|| Error::bad("The package does not say which loki-sdk built it."))?;
        let entry = Self {
            id: manifest.id.clone(),
            name: manifest.name.clone(),
            description: manifest.description.clone(),
            source: String::new(),
            version: manifest.version.clone(),
            url: url.into(),
            sha256: sha256.to_ascii_lowercase(),
            size,
            sdk: sdk.sdk.clone(),
            capabilities: manifest.capabilities.clone(),
            icon: None,
        };
        entry.validate()?;
        Ok(entry)
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        let problem = if !valid_id(&self.id) || self.id.starts_with("loki-") {
            "The app ID is missing or invalid."
        } else if self.name.is_empty() || self.name.len() > 100 {
            "The app name is missing or longer than 100 characters."
        } else if self.description.len() > 2000 {
            "The description is longer than 2000 characters."
        } else if !self.source.is_empty() && !link(&self.source) {
            "The source must be an https:// link."
        } else if semver::Version::parse(&self.version).is_err() {
            "The version is not a valid semantic version."
        } else if !link(&self.url) {
            "The download must be an https:// link."
        } else if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            "The sha256 must be 64 lowercase hex digits."
        } else if self.size == 0 || self.size > MAX_PACKAGE {
            "The package size must be between 1 byte and 512 MiB."
        } else if semver::Version::parse(&self.sdk).is_err() {
            "The loki-sdk version is not a valid version."
        } else if self
            .capabilities
            .iter()
            .any(|c| !CAPABILITIES.contains(&c.as_str()) || !seen.insert(c))
        {
            "The capabilities list has an unknown or repeated entry."
        } else if self
            .icon
            .as_deref()
            .is_some_and(|i| !safe_relative(i) || !i.ends_with(".png"))
        {
            "The icon path is not a safe relative .png path."
        } else {
            return Ok(());
        };
        Err(Error::bad(format!("{}: {problem}", self.id)))
    }

    /// Whether a downloaded package is the release this entry describes.
    pub fn matches(&self, manifest: &Manifest) -> Result<()> {
        let sdk = manifest.built_with.as_ref().map(|b| b.sdk.as_str());
        if manifest.id != self.id
            || manifest.version != self.version
            || sdk != Some(self.sdk.as_str())
            || manifest.capabilities != self.capabilities
        {
            return Err(Error::bad(format!(
                "The package is not the release the registry lists for {} (expected {} {}).",
                self.id, self.id, self.version
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        Entry {
            id: "notes".into(),
            name: "Notes".into(),
            description: String::new(),
            source: "https://github.com/example/notes".into(),
            version: "1.0.0".into(),
            url: "https://example.com/notes.loki-app".into(),
            sha256: "a".repeat(64),
            size: 10,
            sdk: "0.3.0".into(),
            capabilities: vec!["storage".into()],
            icon: None,
        }
    }

    #[test]
    fn entries_are_checked() {
        entry().validate().unwrap();
        let bad: [fn(&mut Entry); 7] = [
            |e| e.id = "Bad".into(),
            |e| e.url = "http://example.com/x".into(),
            |e| e.sha256 = "A".repeat(64),
            |e| e.size = MAX_PACKAGE + 1,
            |e| e.capabilities = vec!["storage".into(), "storage".into()],
            |e| e.version = "1".into(),
            |e| e.icon = Some("../icon.png".into()),
        ];
        for change in bad {
            let mut e = entry();
            change(&mut e);
            assert!(e.validate().is_err(), "{e:?}");
        }
    }
}
