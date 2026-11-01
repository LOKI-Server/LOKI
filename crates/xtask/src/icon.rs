//! Render LOKI's icon from `packaging/icon/loki.syar` with Synarith's `syar` CLI:
//! the Windows `.ico`, the dashboard favicons, and a preview for the README.
use anyhow::{Context, Result};
use std::{path::PathBuf, process::Command};

#[derive(clap::Args)]
pub struct Options {
    /// The Synarith CLI executable.
    #[arg(long, default_value = "syar")]
    syar: PathBuf,
}

const SOURCE: &str = "packaging/icon/loki.syar";

/// Sizes up to 48 px use the bolder `small` output; larger ones show every detail.
fn output(size: u32) -> &'static str {
    if size <= 48 { "small" } else { "main" }
}

pub fn run(options: Options) -> Result<()> {
    let scratch = tempfile::tempdir()?;
    let render = |size: u32| -> Result<Vec<u8>> {
        let path = scratch.path().join(format!("{size}.png"));
        let dimension = size.to_string();
        super::run(
            Command::new(&options.syar)
                .args(["render", SOURCE, "--output-name", output(size)])
                .args([
                    "--width",
                    &dimension,
                    "--height",
                    &dimension,
                    "--samples",
                    "4",
                ])
                .arg("-o")
                .arg(&path),
        )
        .with_context(|| format!("Could not render with {}", options.syar.display()))?;
        Ok(std::fs::read(path)?)
    };

    let sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256];
    let images = sizes.map(|size| render(size).map(|png| (size, png)));
    let images = images.into_iter().collect::<Result<Vec<_>>>()?;
    std::fs::write("packaging/icon/loki.ico", ico(&images))?;

    let png = |size: u32| &images.iter().find(|(s, _)| *s == size).unwrap().1;
    std::fs::write("crates/loki-server/static/icon-32.png", png(32))?;
    std::fs::write("crates/loki-server/static/icon-256.png", png(256))?;
    std::fs::write("packaging/icon/loki.png", render(512)?)?;
    println!("Rendered the LOKI icon from {SOURCE}");
    Ok(())
}

/// An ICO directory whose entries are PNG images, as Windows has read since Vista.
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let count = images.len() as u16;
    let mut out = [0u16, 1, count]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect::<Vec<_>>();
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, png) in images {
        let edge = if *size >= 256 { 0 } else { *size as u8 };
        out.extend([edge, edge, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((png.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in images {
        out.extend(png);
    }
    out
}
