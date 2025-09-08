use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::cmd::default_runner;
use crate::error::Result;
use crate::opts::Options;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartResult {
    NotRequested,
    NotSupported,
    Issued,
    Verified,
}

fn service_monotonic_start(runner: &crate::cmd::Runner, svc: &str) -> Option<u128> {
    // Read ExecMainStartTimestampMonotonic in microseconds
    if let Ok(out) = runner.run_ok(
        "systemctl",
        &["show", svc, "-p", "ExecMainStartTimestampMonotonic"],
    ) {
        for line in out.lines() {
            if let Some(val) = line.strip_prefix("ExecMainStartTimestampMonotonic=") {
                if let Ok(v) = val.trim().parse::<u128>() {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn is_active(runner: &crate::cmd::Runner, svc: &str) -> bool {
    if let Ok(out) = runner.run_ok("systemctl", &["is-active", svc]) {
        return out.trim() == "active";
    }
    false
}

pub fn restart_and_verify(service_hint: &str, opts: &Options) -> Result<RestartResult> {
    if !opts.restart {
        return Ok(RestartResult::NotRequested);
    }
    let runner = default_runner(&opts.nsenter);

    // Determine candidate services for hint
    let candidates: Vec<&str> = match service_hint {
        "k3s" => vec!["k3s", "k3s-agent"],
        _ => vec!["containerd"],
    };

    // Observe pre-restart timestamp for first available candidate
    let mut chosen: Option<&str> = None;
    let mut before: Option<u128> = None;
    for svc in &candidates {
        if let Some(ts) = service_monotonic_start(&runner, svc) {
            chosen = Some(svc);
            before = Some(ts);
            break;
        }
    }

    // Attempt restart across candidates
    let mut issued = false;
    if let Some(svc) = chosen.or_else(|| candidates.first().copied()) {
        if runner.run_capture("systemctl", &["restart", svc])?.0 == 0 {
            info!("Issued restart via systemctl for {svc}");
            issued = true;
            chosen = Some(svc);
        } else if runner.run_capture("service", &[svc, "restart"])?.0 == 0 {
            info!("Issued restart via service for {svc}");
            issued = true;
            chosen = Some(svc);
        }
    }

    if !issued {
        warn!("Unable to restart service automatically");
        return Ok(RestartResult::NotSupported);
    }

    // Poll for active and timestamp increase
    if let Some(svc) = chosen {
        let start = Instant::now();
        let mut verified = false;
        while start.elapsed() < Duration::from_secs(60) {
            if is_active(&runner, svc) {
                if let (Some(b), Some(a)) = (before, service_monotonic_start(&runner, svc)) {
                    if a > b {
                        verified = true;
                        break;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        return Ok(if verified {
            RestartResult::Verified
        } else {
            RestartResult::Issued
        });
    }

    Ok(RestartResult::Issued)
}

pub fn wait_for_socket(path: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if Path::new(path).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    false
}
