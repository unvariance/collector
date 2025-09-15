use std::time::Duration;

use tokio::sync::mpsc;

fn init_test_logger() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

use nri::NRI;
use nri_resctrl_plugin::{PodResctrlEvent, ResctrlPlugin, ResctrlPluginConfig};

#[tokio::test]
#[ignore]
async fn test_resctrl_plugin_registers_with_nri() -> anyhow::Result<()> {
    init_test_logger();

    // Use NRI socket path provided by the workflow via env.
    let socket_path = std::env::var("NRI_SOCKET_PATH")?;
    println!("[integration_test] Using NRI socket at: {}", socket_path);

    // Connect to NRI runtime socket
    let socket = tokio::net::UnixStream::connect(&socket_path).await?;
    println!("[integration_test] Connected to NRI socket");

    // Build plugin with an externally provided channel
    let (tx, mut _rx) = mpsc::channel::<PodResctrlEvent>(64);
    let plugin = ResctrlPlugin::new(ResctrlPluginConfig::default(), tx);

    // Start NRI server for plugin and register
    let (nri, join_handle) = NRI::new(socket, plugin, "resctrl-plugin", "10").await?;
    println!("[integration_test] Created NRI instance; registering plugin");
    nri.register().await?;
    println!("[integration_test] Plugin registered successfully");

    // Allow runtime to settle briefly
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Shut down cleanly
    nri.close().await?;
    join_handle.await??;
    println!("[integration_test] Plugin shutdown completed");

    Ok(())
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn test_startup_cleanup_e2e() -> anyhow::Result<()> {
    init_test_logger();
    // Guard: explicit opt-in for E2E and only on Linux systems with permissions.
    if std::env::var("RESCTRL_E2E").ok().as_deref() != Some("1") {
        eprintln!("RESCTRL_E2E not set; skipping E2E cleanup test");
        return Ok(());
    }

    // Precondition: ensure resctrl is mounted using RealFs without shelling out.
    let rc = resctrl::Resctrl::default();
    if let Err(e) = rc.ensure_mounted(true) {
        eprintln!(
            "ensure_mounted failed (need CAP_SYS_ADMIN?): {} — skipping",
            e
        );
        return Ok(());
    }

    // Setup test directories under the real resctrl filesystem
    use std::fs;
    use std::path::PathBuf;

    let root = PathBuf::from("/sys/fs/resctrl");
    let mon_groups = root.join("mon_groups");
    // mon_groups may not exist on all kernels; require it for this E2E
    if !mon_groups.exists() {
        eprintln!("mon_groups not present; skipping E2E cleanup test");
        return Ok(());
    }

    let p_a = root.join("test_e2e_a");
    let p_b = root.join("test_e2e_b");
    let p_np_c = root.join("np_e2e_c");
    let mg_m1 = mon_groups.join("test_e2e_m1");
    let mg_np_m2 = mon_groups.join("np_e2e_m2");

    // Helper to mkdir if missing
    let ensure_dir = |p: &std::path::Path| {
        if !p.exists() {
            match fs::create_dir(p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    };

    ensure_dir(&p_a)?;
    ensure_dir(&p_b)?;
    ensure_dir(&p_np_c)?;
    ensure_dir(&mg_m1)?;
    ensure_dir(&mg_np_m2)?;

    // Record if info dir exists to check it's untouched
    let info_path = root.join("info");
    let info_existed = info_path.exists();

    // Execute: start plugin with cleanup_on_start=true and test prefix
    let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(64);
    let plugin = ResctrlPlugin::new(
        ResctrlPluginConfig {
            group_prefix: "test_e2e_".into(),
            cleanup_on_start: true,
            auto_mount: true,
            ..Default::default()
        },
        tx,
    );

    // Call configure then synchronize with empty sets
    let ctx = ttrpc::r#async::TtrpcContext {
        mh: ttrpc::MessageHeader::default(),
        metadata: std::collections::HashMap::new(),
        timeout_nano: 5_000,
    };
    let _ = nri::api_ttrpc::Plugin::configure(
        &plugin,
        &ctx,
        nri::api::ConfigureRequest {
            config: String::new(),
            runtime_name: "e2e-runtime".into(),
            runtime_version: "1.0".into(),
            registration_timeout: 1000,
            request_timeout: 1000,
            special_fields: protobuf::SpecialFields::default(),
        },
    )
    .await?;

    let _ = nri::api_ttrpc::Plugin::synchronize(
        &plugin,
        &ctx,
        nri::api::SynchronizeRequest {
            pods: vec![],
            containers: vec![],
            more: false,
            special_fields: protobuf::SpecialFields::default(),
        },
    )
    .await?;

    // Verify: no events emitted for cleanup-only run
    assert!(rx.try_recv().is_err());

    // Verify: root cleanup behavior
    assert!(!p_a.exists(), "{} should be removed", p_a.display());
    assert!(!p_b.exists(), "{} should be removed", p_b.display());
    assert!(p_np_c.exists(), "{} should remain", p_np_c.display());

    // Verify: mon_groups cleanup behavior
    assert!(!mg_m1.exists(), "{} should be removed", mg_m1.display());
    assert!(mg_np_m2.exists(), "{} should remain", mg_np_m2.display());

    // Verify: info untouched if it existed at start
    if info_existed {
        assert!(info_path.exists(), "info directory should remain untouched");
    }

    // Teardown: remove any leftover test_e2e_* artifacts
    let _ = fs::remove_dir(&p_a);
    let _ = fs::remove_dir(&p_b);
    let _ = fs::remove_dir(&mg_m1);
    // Leave non-prefix artifacts as-is by spec; but clean any stray matching dirs
    for entry in std::fs::read_dir(&root)? {
        let de = entry?;
        if de.file_type()?.is_dir() {
            if let Some(name) = de.file_name().to_str() {
                if name.starts_with("test_e2e_") {
                    let _ = fs::remove_dir(de.path());
                }
            }
        }
    }
    if mon_groups.exists() {
        for entry in std::fs::read_dir(&mon_groups)? {
            let de = entry?;
            if de.file_type()?.is_dir() {
                if let Some(name) = de.file_name().to_str() {
                    if name.starts_with("test_e2e_") {
                        let _ = fs::remove_dir(de.path());
                    }
                }
            }
        }
    }

    Ok(())
}
