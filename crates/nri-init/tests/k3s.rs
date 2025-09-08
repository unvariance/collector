use nri_init::{LogLevel, Mode, Options};

#[ignore]
#[test]
fn detect_k3s_and_configure_without_restart() {
    let opts = Options {
        configure: true,
        restart: false,
        fail_if_unavailable: false,
        mode: Mode::K3s,
        nsenter: None,
        log_level: LogLevel::Info,
        dry_run: true,
        containerd_config_path: None,
        socket_path: None,
        k3s_template_dir: None,
    };
    let out = nri_init::run(opts).expect("run ok");
    match out.env {
        nri_init::EnvKind::K3s { .. } => {}
        _ => panic!("not k3s"),
    }
}

#[ignore]
#[test]
fn detect_k3s_and_configure_with_restart() {
    // In CI we don't expect an actual NRI socket to be present; keep fail_if_unavailable=false.
    let opts = Options {
        configure: true,
        restart: true,
        fail_if_unavailable: false,
        mode: Mode::K3s,
        nsenter: None,
        log_level: LogLevel::Info,
        dry_run: true,
        containerd_config_path: None,
        socket_path: None,
        k3s_template_dir: None,
    };
    let out = nri_init::run(opts).expect("run ok");
    match out.env {
        nri_init::EnvKind::K3s { .. } => {}
        _ => panic!("not k3s"),
    }
}
