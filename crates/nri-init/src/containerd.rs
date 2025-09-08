use std::fs;
use std::path::Path;
use toml_edit::DocumentMut;
use tracing::info;

use crate::error::{NriError, Result};
use crate::toml_util::{ensure_nri_section, ensure_version2};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/containerd/config.toml";
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/nri/nri.sock";

pub fn configure_containerd(path: &str, socket_path: &str, dry_run: bool) -> Result<bool> {
    let p = Path::new(path);
    if !p.exists() {
        info!("Containerd config not found at {path}, creating minimal file");
        if dry_run {
            return Ok(true);
        }
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(p, b"version = 2\n")?;
    }

    let content = fs::read_to_string(p)?;
    let mut doc: DocumentMut = content
        .parse()
        .map_err(|e| NriError::TomlMutation(format!("parse error: {e}")))?;
    let mut changed = ensure_version2(&mut doc);
    changed |= ensure_nri_section(&mut doc, socket_path);

    if changed {
        info!("Updating containerd NRI configuration at {path}");
        if !dry_run {
            fs::write(p, doc.to_string())?;
        }
    } else {
        info!("Containerd NRI configuration already up to date");
    }
    Ok(changed)
}
