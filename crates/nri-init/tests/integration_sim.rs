use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

use nri_init::{LogLevel, Mode, Options};

fn temp_path(dir: &TempDir, rel: &str) -> String {
    let mut p = PathBuf::from(dir.path());
    p.push(rel);
    p.to_string_lossy().into_owned()
}

#[test]
fn containerd_config_minimal_is_created_and_patched() {
    let tmp = TempDir::new().unwrap();
    let cfg = temp_path(&tmp, "etc/containerd/config.toml");
    fs::create_dir_all(PathBuf::from(&cfg).parent().unwrap()).unwrap();
    // Don't create file; ensure our code creates version=2

    let socket = temp_path(&tmp, "var/run/nri/nri.sock");
    fs::create_dir_all(PathBuf::from(&socket).parent().unwrap()).unwrap();

    let opts = Options {
        configure: true,
        restart: false,
        fail_if_unavailable: false,
        mode: Mode::Containerd,
        nsenter: None,
        log_level: LogLevel::Info,
        dry_run: false,
        containerd_config_path: Some(cfg.clone()),
        socket_path: Some(socket.clone()),
        k3s_template_dir: None,
    };
    let out = nri_init::run(opts).expect("run ok");
    assert!(out.configured);

    let content = fs::read_to_string(cfg).unwrap();
    assert!(content.contains("version = 2"));
    assert!(content.contains("plugins.\"io.containerd.nri.v1.nri\""));
    assert!(content.contains("disable = false"));
}

#[test]
fn k3s_templates_created_and_patched() {
    let tmp = TempDir::new().unwrap();
    let base = temp_path(&tmp, "var/lib/rancher/k3s/agent/etc/containerd");

    let opts = Options {
        configure: true,
        restart: false,
        fail_if_unavailable: false,
        mode: Mode::K3s,
        nsenter: None,
        log_level: LogLevel::Info,
        dry_run: false,
        containerd_config_path: None,
        socket_path: None,
        k3s_template_dir: Some(base.clone()),
    };
    let out = nri_init::run(opts).expect("run ok");
    assert!(out.configured);

    let v2 = PathBuf::from(&base).join("config.toml.tmpl");
    let v3 = PathBuf::from(&base).join("config-v3.toml.tmpl");
    let c2 = fs::read_to_string(v2).unwrap();
    let c3 = fs::read_to_string(v3).unwrap();
    assert!(c2.contains("plugins.\"io.containerd.nri.v1.nri\""));
    assert!(c3.contains("plugins.\"io.containerd.nri.v1.nri\""));
}

#[test]
fn containerd_config_idempotent() {
    let tmp = TempDir::new().unwrap();
    let cfg = temp_path(&tmp, "etc/containerd/config.toml");
    std::fs::create_dir_all(std::path::PathBuf::from(&cfg).parent().unwrap()).unwrap();

    // First run creates and patches
    let opts1 = Options {
        configure: true,
        restart: false,
        fail_if_unavailable: false,
        mode: Mode::Containerd,
        nsenter: None,
        log_level: LogLevel::Info,
        dry_run: false,
        containerd_config_path: Some(cfg.clone()),
        socket_path: None,
        k3s_template_dir: None,
    };
    // Clone to avoid moving opts1 so we can reuse it below
    let out1 = nri_init::run(opts1.clone()).expect("first run ok");
    assert!(out1.configured);

    // Second run should find no changes to apply
    let mut opts2 = opts1.clone();
    opts2.containerd_config_path = Some(cfg.clone());
    let out2 = nri_init::run(opts2).expect("second run ok");
    assert!(
        !out2.configured,
        "second run should be idempotent (no changes)"
    );
}
