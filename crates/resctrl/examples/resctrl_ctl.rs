use resctrl::{Resctrl, Config};
use std::env;
use std::process::ExitCode;

fn print_usage() {
    eprintln!("Usage: resctrl_ctl <detect|ensure> [--auto-mount=<true|false>]");
}

fn parse_flag(name: &str) -> Option<String> {
    for arg in env::args().skip(2) {
        if let Some(rest) = arg.strip_prefix("--") {
            let mut parts = rest.splitn(2, '=');
            let key = parts.next().unwrap_or("");
            let val = parts.next().unwrap_or("");
            if key == name {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let cmd = match args.next() {
        Some(c) => c,
        None => {
            print_usage();
            return ExitCode::from(2);
        }
    };

    match cmd.as_str() {
        "detect" => {
            let rc = Resctrl::default();
            match rc.detect_support() {
                Ok(info) => {
                    println!(
                        "mounted={mounted} mount_point={mp} writable={w}",
                        mounted = info.mounted,
                        mp = info
                            .mount_point
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "".to_string()),
                        w = info.writable
                    );
                    ExitCode::from(0)
                }
                Err(e) => {
                    eprintln!("detect_support error: {e}");
                    ExitCode::from(1)
                }
            }
        }
        "ensure" => {
            let auto_mount = parse_flag("auto-mount")
                .as_deref()
                .unwrap_or("false")
                .eq_ignore_ascii_case("true");
            let mut cfg = Config::default();
            cfg.auto_mount = auto_mount;
            let rc = Resctrl::new(cfg);
            match rc.ensure_mounted() {
                Ok(()) => {
                    println!("ensure_mounted: ok");
                    ExitCode::from(0)
                }
                Err(e) => {
                    eprintln!("ensure_mounted error: {e}");
                    ExitCode::from(1)
                }
            }
        }
        _ => {
            print_usage();
            ExitCode::from(2)
        }
    }
}

