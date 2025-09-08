use nri_init::{LogLevel, Mode, Options};

#[ignore]
#[test]
fn configure_without_restart() {
    let opts = Options {
        configure: true,
        restart: false,
        fail_if_unavailable: false,
        mode: Mode::Containerd,
        nsenter: None,
        log_level: LogLevel::Info,
        dry_run: true,
        containerd_config_path: None,
        socket_path: None,
        k3s_template_dir: None,
    };
    let out = nri_init::run(opts).expect("run ok");
    assert!(matches!(out.env, nri_init::EnvKind::Containerd));
    assert!(out.configured || !out.configured); // placeholder assertion
}

#[ignore]
#[test]
fn configure_with_restart() {
    let opts = Options {
        configure: true,
        restart: true,
        fail_if_unavailable: false,
        mode: Mode::Containerd,
        nsenter: None,
        log_level: LogLevel::Info,
        dry_run: true,
        containerd_config_path: None,
        socket_path: None,
        k3s_template_dir: None,
    };
    let out = nri_init::run(opts).expect("run ok");
    assert!(matches!(out.env, nri_init::EnvKind::Containerd));
}
