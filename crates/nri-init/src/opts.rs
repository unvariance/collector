#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    K3s,
    Containerd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone)]
pub struct Nsenter {
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub configure: bool,
    pub restart: bool,
    pub fail_if_unavailable: bool,
    pub mode: Mode,
    pub nsenter: Option<Nsenter>,
    pub log_level: LogLevel,
    pub dry_run: bool,
    // Test/CI overrides
    pub containerd_config_path: Option<String>,
    pub socket_path: Option<String>,
    pub k3s_template_dir: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            configure: false,
            restart: false,
            fail_if_unavailable: false,
            mode: Mode::Auto,
            nsenter: None,
            log_level: LogLevel::Info,
            dry_run: false,
            containerd_config_path: None,
            socket_path: None,
            k3s_template_dir: None,
        }
    }
}

fn parse_bool(s: &str) -> bool {
    matches!(s.trim(), "1" | "true" | "TRUE" | "True" | "yes" | "on")
}

pub fn from_env_and_args() -> Options {
    // Minimal, argument-light parser to avoid extra deps; flags override envs.
    let mut opts = Options::default();

    // Env vars
    if let Ok(v) = std::env::var("NRI_CONFIGURE") {
        opts.configure = parse_bool(&v);
    }
    if let Ok(v) = std::env::var("NRI_RESTART") {
        opts.restart = parse_bool(&v);
    }
    if let Ok(v) = std::env::var("NRI_FAIL_IF_UNAVAILABLE") {
        opts.fail_if_unavailable = parse_bool(&v);
    }

    // Args
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--configure" => opts.configure = true,
            "--no-configure" => opts.configure = false,
            "--restart" => opts.restart = true,
            "--no-restart" => opts.restart = false,
            "--fail-if-unavailable" => opts.fail_if_unavailable = true,
            "--no-fail-if-unavailable" => opts.fail_if_unavailable = false,
            "--mode" => {
                if let Some(v) = args.next() {
                    opts.mode = match v.as_str() {
                        "auto" => Mode::Auto,
                        "k3s" => Mode::K3s,
                        "containerd" => Mode::Containerd,
                        _ => Mode::Auto,
                    };
                }
            }
            "--dry-run" => opts.dry_run = true,
            "--nsenter-path" => {
                if let Some(p) = args.next() {
                    opts.nsenter = Some(Nsenter { path: p });
                }
            }
            "--containerd-config" => {
                if let Some(p) = args.next() {
                    opts.containerd_config_path = Some(p);
                }
            }
            "--socket-path" => {
                if let Some(p) = args.next() {
                    opts.socket_path = Some(p);
                }
            }
            "--k3s-template-dir" => {
                if let Some(p) = args.next() {
                    opts.k3s_template_dir = Some(p);
                }
            }
            "--log-level" => {
                if let Some(v) = args.next() {
                    opts.log_level = match v.as_str() {
                        "error" => LogLevel::Error,
                        "warn" => LogLevel::Warn,
                        "info" => LogLevel::Info,
                        "debug" => LogLevel::Debug,
                        "trace" => LogLevel::Trace,
                        _ => LogLevel::Info,
                    };
                }
            }
            _ => {}
        }
    }

    // Auto-discover nsenter typical path inside k8s host mount
    if opts.nsenter.is_none() && std::path::Path::new("/host/proc/1/ns/mnt").exists() {
        opts.nsenter = Some(Nsenter {
            path: "nsenter".to_string(),
        });
    }

    opts
}
