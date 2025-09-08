use semver::Version;
use tracing::{info, warn};

use crate::cmd::default_runner;
use crate::error::Result;
use crate::opts::{Mode, Options};

#[derive(Debug, Clone)]
pub enum EnvKind {
    K3s { version: Option<Version> },
    Containerd,
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub env: EnvKind,
    pub containerd_version: Option<Version>,
}

fn parse_first_semver(s: &str) -> Option<Version> {
    // Find first x.y.z pattern and try parse
    let re = regex_for_semver();
    if let Some(m) = re.find(s) {
        Version::parse(m.as_str()).ok()
    } else {
        None
    }
}

fn regex_for_semver() -> regex::Regex {
    // compile-once using once_cell
    static ONCE: once_cell::sync::OnceCell<regex::Regex> = once_cell::sync::OnceCell::new();
    ONCE.get_or_init(|| regex::Regex::new(r"\b(\d+)\.(\d+)\.(\d+)\b").unwrap())
        .clone()
}

pub fn detect(opts: &Options) -> Result<Detection> {
    if let Mode::K3s = opts.mode {
        return Ok(Detection {
            env: EnvKind::K3s {
                version: detect_k3s_version(opts)?,
            },
            containerd_version: detect_containerd_version(opts)?,
        });
    }
    if let Mode::Containerd = opts.mode {
        return Ok(Detection {
            env: EnvKind::Containerd,
            containerd_version: detect_containerd_version(opts)?,
        });
    }

    // Auto mode
    if std::path::Path::new("/var/lib/rancher/k3s").exists() {
        info!("K3s installation detected");
        return Ok(Detection {
            env: EnvKind::K3s {
                version: detect_k3s_version(opts)?,
            },
            containerd_version: detect_containerd_version(opts)?,
        });
    }
    info!("Plain containerd environment assumed");
    Ok(Detection {
        env: EnvKind::Containerd,
        containerd_version: detect_containerd_version(opts)?,
    })
}

fn detect_k3s_version(opts: &Options) -> Result<Option<Version>> {
    let runner = default_runner(&opts.nsenter);
    match runner.run_ok("k3s", &["--version"]) {
        Ok(out) => Ok(parse_first_semver(&out)),
        Err(_) => Ok(None),
    }
}

pub fn detect_containerd_version(opts: &Options) -> Result<Option<Version>> {
    let runner = default_runner(&opts.nsenter);
    // containerd --version
    if let Ok(out) = runner.run_ok("containerd", &["--version"]) {
        return Ok(parse_first_semver(&out));
    }
    // containerd -v
    if let Ok(out) = runner.run_ok("containerd", &["-v"]) {
        return Ok(parse_first_semver(&out));
    }
    // ctr version -> parse Server: containerd then Version: line (best-effort)
    if let Ok(out) = runner.run_ok("ctr", &["version"]) {
        // naive scan for first line with "Version:"
        for line in out.lines() {
            if let Some(stripped) = line.trim().strip_prefix("Version:") {
                if let Ok(v) = Version::parse(stripped.trim()) {
                    return Ok(Some(v));
                }
            }
        }
        // else try the regex fallback
        return Ok(parse_first_semver(&out));
    }

    warn!("Unable to determine containerd version");
    Ok(None)
}
