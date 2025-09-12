mod cmd;
mod containerd;
mod detect;
mod error;
mod k3s;
pub mod opts;
mod toml_util;
mod verify;

pub use detect::EnvKind;
pub use error::{NriError, Result};
pub use opts::{LogLevel, Mode, Nsenter, Options};
pub use verify::RestartResult;

use semver::Version;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct NriOutcome {
    pub env: EnvKind,
    pub containerd_version: Option<Version>,
    pub configured: bool,
    pub restarted: bool,
    pub restart_verified: bool,
    pub socket_available: bool,
}

pub fn run(opts: Options) -> Result<NriOutcome> {
    info!("Starting NRI initialization check");
    info!(
        "Configuration: configure={}, restart={}",
        opts.configure, opts.restart
    );

    let det = detect::detect(&opts)?;

    // Early socket check
    let socket_path = opts
        .socket_path
        .as_deref()
        .unwrap_or(containerd::DEFAULT_SOCKET_PATH);
    let socket_available = std::path::Path::new(socket_path).exists();
    if socket_available {
        info!("NRI socket found at {socket_path}");
    } else {
        warn!("NRI socket not found at {socket_path}");
    }

    // Version gate: if known containerd < 1.7, skip config
    if let Some(ref v) = det.containerd_version {
        info!("Detected containerd version: {}", v);
        let min = Version::parse("1.7.0").unwrap();
        if *v < min {
            warn!("containerd {} does not support NRI (>=1.7 required)", v);
            if opts.fail_if_unavailable {
                return Err(NriError::VersionUnsupported(v.to_string()));
            }
            return Ok(NriOutcome {
                env: det.env,
                containerd_version: det.containerd_version,
                configured: false,
                restarted: false,
                restart_verified: false,
                socket_available,
            });
        }
    } else {
        warn!("Unable to determine containerd version; proceeding best-effort");
    }

    let mut configured = false;
    if opts.configure {
        match det.env {
            EnvKind::K3s { .. } => {
                configured = if let Some(ref dir) = opts.k3s_template_dir {
                    k3s::configure_k3s_templates_in(dir, opts.dry_run)
                        .map_err(NriError::Io)?
                } else {
                    k3s::configure_k3s_templates(opts.dry_run).map_err(NriError::Io)?
                };
            }
            EnvKind::Containerd => {
                let cfg_path = opts
                    .containerd_config_path
                    .as_deref()
                    .unwrap_or(containerd::DEFAULT_CONFIG_PATH);
                configured = containerd::configure_containerd(cfg_path, socket_path, opts.dry_run)?;
            }
        }
    } else {
        info!("NRI configuration is disabled (configure=false)");
    }

    // Restart path
    let mut restarted = false;
    let mut restart_verified = false;
    if opts.restart && configured {
        use verify::RestartResult::*;
        let hint = match det.env {
            EnvKind::K3s { .. } => "k3s",
            EnvKind::Containerd => "containerd",
        };
        match verify::restart_and_verify(hint, &opts)? {
            NotRequested => {}
            NotSupported => {
                warn!("Automatic restart not supported; manual restart may be required");
            }
            Issued => {
                restarted = true;
                // After issuing a restart, wait up to 60s for the socket (older nodes can be slower)
                if verify::wait_for_socket(socket_path, std::time::Duration::from_secs(60)) {
                    restart_verified = true;
                    info!("NRI socket became available");
                } else {
                    warn!("NRI socket did not appear within 10s after restart");
                }
            }
            Verified => {
                // Service restart was verified (active and timestamp increased)
                restarted = true;
                restart_verified = true;
                // Also give the NRI socket up to 60s to appear
                if verify::wait_for_socket(socket_path, std::time::Duration::from_secs(60)) {
                    info!("NRI socket available after verified restart");
                } else {
                    warn!("NRI socket not yet available within 10s after verified restart");
                }
            }
        }
    }

    // Final availability policy
    let socket_available_final = std::path::Path::new(socket_path).exists();
    if opts.fail_if_unavailable && !socket_available_final {
        error!("NRI unavailable and fail_if_unavailable=true");
        return Err(NriError::VerificationFailed(
            "NRI socket unavailable".into(),
        ));
    }

    Ok(NriOutcome {
        env: det.env,
        containerd_version: det.containerd_version,
        configured,
        restarted,
        restart_verified,
        socket_available: socket_available_final,
    })
}
