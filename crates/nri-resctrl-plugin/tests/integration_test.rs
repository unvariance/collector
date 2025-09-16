use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

fn init_test_logger() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

use nri::NRI;
use nri_resctrl_plugin::{
    PodResctrlAddOrUpdate, PodResctrlEvent, ResctrlGroupState, ResctrlPlugin, ResctrlPluginConfig,
};

async fn run_kubectl(args: &[&str]) -> anyhow::Result<()> {
    let status = tokio::process::Command::new("kubectl")
        .args(args)
        .status()
        .await?;
    anyhow::ensure!(status.success(), "kubectl {:?} failed: {:?}", args, status);
    Ok(())
}

async fn kubectl_json(args: &[&str]) -> anyhow::Result<Value> {
    let output = tokio::process::Command::new("kubectl")
        .args(args)
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "kubectl {:?} failed: {:?}",
        args,
        output.status
    );
    let v: Value = serde_json::from_slice(&output.stdout)?;
    Ok(v)
}

async fn load_pod_json(name: &str) -> anyhow::Result<Value> {
    kubectl_json(&["get", "pod", name, "-o", "json"]).await
}

fn gather_container_ids(pod: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(status) = pod.get("status") {
        if let Some(arr) = status.get("containerStatuses").and_then(Value::as_array) {
            for entry in arr {
                if let (Some(name), Some(id)) = (
                    entry.get("name").and_then(Value::as_str),
                    entry.get("containerID").and_then(Value::as_str),
                ) {
                    if !id.is_empty() {
                        out.push((name.to_string(), id.to_string()));
                    }
                }
            }
        }
        if let Some(arr) = status
            .get("ephemeralContainerStatuses")
            .and_then(Value::as_array)
        {
            for entry in arr {
                if let (Some(name), Some(id)) = (
                    entry.get("name").and_then(Value::as_str),
                    entry.get("containerID").and_then(Value::as_str),
                ) {
                    if !id.is_empty() {
                        out.push((name.to_string(), id.to_string()));
                    }
                }
            }
        }
    }
    out
}

async fn wait_for_container_ids(
    pod_name: &str,
    expected: usize,
    timeout: Duration,
) -> anyhow::Result<Vec<(String, String)>> {
    let deadline = Instant::now() + timeout;
    loop {
        let pod = load_pod_json(pod_name).await?;
        let ids = gather_container_ids(&pod);
        if ids.len() >= expected {
            return Ok(ids);
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for {} containers on pod {}; last seen {}",
                expected,
                pod_name,
                ids.len()
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn trim_container_runtime_prefix(container_id: &str) -> &str {
    container_id
        .split_once("//")
        .map(|(_, rest)| rest)
        .unwrap_or(container_id)
}

async fn container_pid(container_id: &str) -> anyhow::Result<i32> {
    let trimmed = trim_container_runtime_prefix(container_id);
    let output = tokio::process::Command::new("crictl")
        .args(["inspect", "--output", "json", trimmed])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "crictl inspect failed for {}: {:?}",
        container_id,
        output.status
    );
    let v: Value = serde_json::from_slice(&output.stdout)?;
    let pid = v
        .pointer("/info/pid")
        .and_then(Value::as_i64)
        .or_else(|| v.pointer("/status/pid").and_then(Value::as_i64))
        .or_else(|| v.pointer("/status/linux/pid").and_then(Value::as_i64))
        .context("PID not found in crictl inspect output")?;
    Ok(pid as i32)
}

async fn resolve_container_pids(ids: &[(String, String)]) -> anyhow::Result<Vec<i32>> {
    let mut pids = Vec::with_capacity(ids.len());
    for (_, id) in ids {
        pids.push(container_pid(id).await?);
    }
    Ok(pids)
}

async fn wait_for_tasks_with_pids(
    group_path: &str,
    expected_pids: &[i32],
    timeout: Duration,
) -> anyhow::Result<Vec<i32>> {
    let tasks_path = Path::new(group_path).join("tasks");
    let deadline = Instant::now() + timeout;
    loop {
        match tokio::fs::read_to_string(&tasks_path).await {
            Ok(contents) => {
                let pids: Vec<i32> = contents
                    .lines()
                    .filter_map(|l| l.trim().parse::<i32>().ok())
                    .collect();
                if expected_pids.iter().all(|pid| pids.contains(pid)) {
                    return Ok(pids);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for tasks file {} to include {:?}",
                tasks_path.display(),
                expected_pids
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_group_absent(group_path: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match tokio::fs::metadata(group_path).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        if Instant::now() >= deadline {
            bail!(
                "group {} still present after waiting {:?}",
                group_path,
                timeout
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_pod_update(
    rx: &mut mpsc::Receiver<PodResctrlEvent>,
    pod_uid: &str,
    expected_total: usize,
    expected_reconciled: usize,
    timeout: Duration,
) -> anyhow::Result<PodResctrlAddOrUpdate> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(rem) if !rem.is_zero() => rem,
            _ => {
                bail!(
                    "timed out waiting for AddOrUpdate for pod {} with counts {}/{}",
                    pod_uid,
                    expected_total,
                    expected_reconciled
                )
            }
        };
        let ev = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => bail!("event channel closed while waiting for pod {pod_uid}"),
            Err(_) => {
                bail!(
                    "timed out waiting for AddOrUpdate for pod {} with counts {}/{}",
                    pod_uid,
                    expected_total,
                    expected_reconciled
                )
            }
        };
        match ev {
            PodResctrlEvent::AddOrUpdate(add) if add.pod_uid == pod_uid => {
                if add.total_containers == expected_total
                    && add.reconciled_containers == expected_reconciled
                {
                    return Ok(add);
                }
            }
            PodResctrlEvent::Removed(r) if r.pod_uid == pod_uid => {
                bail!(
                    "saw premature Removed event for pod {} while waiting for counts",
                    pod_uid
                );
            }
            _ => {}
        }
    }
}

async fn wait_for_pod_removed(
    rx: &mut mpsc::Receiver<PodResctrlEvent>,
    pod_uid: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(rem) if !rem.is_zero() => rem,
            _ => bail!("timed out waiting for Removed event for pod {}", pod_uid),
        };
        let ev = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => bail!("event channel closed while waiting for removal of {pod_uid}"),
            Err(_) => bail!("timed out waiting for Removed event for pod {}", pod_uid),
        };
        match ev {
            PodResctrlEvent::Removed(r) if r.pod_uid == pod_uid => return Ok(()),
            _ => {}
        }
    }
}

#[tokio::test]
#[ignore]
async fn test_plugin_full_flow() -> anyhow::Result<()> {
    init_test_logger();

    // Gate: only run when explicitly requested and when tools are available
    if std::env::var("RESCTRL_E2E").ok().as_deref() != Some("1") {
        eprintln!("RESCTRL_E2E not set; skipping full flow e2e test");
        return Ok(());
    }

    // Use NRI socket path provided by the workflow via env.
    let socket_path = std::env::var("NRI_SOCKET_PATH")?;
    println!("[integration_test] Using NRI socket at: {}", socket_path);

    // Pre-create a pod before plugin registration (preexisting assignment)
    // Best-effort cleanup then create
    let _ = run_kubectl(&["delete", "pod", "e2e-a", "--ignore-not-found=true"]).await;
    run_kubectl(&[
        "run",
        "e2e-a",
        "--image=busybox",
        "--restart=Never",
        "--",
        "/bin/sh",
        "-c",
        "sleep 3600",
    ])
    .await?;
    run_kubectl(&[
        "wait",
        "--for=condition=Ready",
        "pod/e2e-a",
        "--timeout=120s",
    ])
    .await?;

    // Record pod metadata and current containers before plugin registration.
    let pod_a = load_pod_json("e2e-a").await?;
    let pod_a_uid = pod_a
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .context("pod e2e-a missing metadata.uid")?
        .to_string();
    let containers_a = wait_for_container_ids("e2e-a", 1, Duration::from_secs(90)).await?;
    let pids_a = resolve_container_pids(&containers_a).await?;

    // Connect to NRI runtime socket
    let socket = tokio::net::UnixStream::connect(&socket_path).await?;
    println!("[integration_test] Connected to NRI socket");

    // Build plugin with an externally provided channel
    let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(256);
    let plugin = ResctrlPlugin::new(ResctrlPluginConfig::default(), tx);

    // Start NRI server for plugin and register
    let (nri, join_handle) = NRI::new(socket, plugin, "resctrl-plugin", "10").await?;
    println!("[integration_test] Created NRI instance; registering plugin");
    nri.register().await?;
    println!("[integration_test] Plugin registered successfully");

    // Expect startup synchronization events for the preexisting pod.
    let _ = wait_for_pod_update(&mut rx, &pod_a_uid, 0, 0, Duration::from_secs(60)).await?;
    let event_a = wait_for_pod_update(
        &mut rx,
        &pod_a_uid,
        containers_a.len(),
        containers_a.len(),
        Duration::from_secs(60),
    )
    .await?;
    let group_path_a = match event_a.group_state {
        ResctrlGroupState::Exists(ref path) => path.clone(),
        ResctrlGroupState::Failed => bail!("preexisting pod group creation failed"),
    };

    // Verify tasks reflect existing containers.
    let _ = wait_for_tasks_with_pids(&group_path_a, &pids_a, Duration::from_secs(30)).await?;

    // Post-start: add an ephemeral container using kubectl debug and ensure counts improve.
    run_kubectl(&[
        "debug",
        "pod/e2e-a",
        "-c",
        "dbg",
        "--image=busybox",
        "--target=e2e-a",
        "--",
        "/bin/sh",
        "-c",
        "sleep 600",
    ])
    .await?;
    let containers_a_after = wait_for_container_ids("e2e-a", 2, Duration::from_secs(120)).await?;
    let pids_a_after = resolve_container_pids(&containers_a_after).await?;
    let update_a_after = wait_for_pod_update(
        &mut rx,
        &pod_a_uid,
        containers_a_after.len(),
        containers_a_after.len(),
        Duration::from_secs(90),
    )
    .await?;
    if let ResctrlGroupState::Exists(path) = &update_a_after.group_state {
        assert_eq!(
            path, &group_path_a,
            "group path should remain stable for pod e2e-a"
        );
    }
    let _ = wait_for_tasks_with_pids(&group_path_a, &pids_a_after, Duration::from_secs(60)).await?;

    // Post-start: create a new pod and ensure a new group is provisioned and reconciled.
    let _ = run_kubectl(&["delete", "pod", "e2e-b", "--ignore-not-found=true"]).await;
    run_kubectl(&[
        "run",
        "e2e-b",
        "--image=busybox",
        "--restart=Never",
        "--",
        "/bin/sh",
        "-c",
        "sleep 3600",
    ])
    .await?;
    run_kubectl(&[
        "wait",
        "--for=condition=Ready",
        "pod/e2e-b",
        "--timeout=120s",
    ])
    .await?;
    let pod_b = load_pod_json("e2e-b").await?;
    let pod_b_uid = pod_b
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .context("pod e2e-b missing metadata.uid")?
        .to_string();
    let containers_b = wait_for_container_ids("e2e-b", 1, Duration::from_secs(90)).await?;
    let pids_b = resolve_container_pids(&containers_b).await?;
    let _ = wait_for_pod_update(&mut rx, &pod_b_uid, 0, 0, Duration::from_secs(60)).await?;
    let update_b = wait_for_pod_update(
        &mut rx,
        &pod_b_uid,
        containers_b.len(),
        containers_b.len(),
        Duration::from_secs(60),
    )
    .await?;
    let group_path_b = match update_b.group_state {
        ResctrlGroupState::Exists(ref path) => path.clone(),
        ResctrlGroupState::Failed => bail!("new pod group creation failed"),
    };
    let _ = wait_for_tasks_with_pids(&group_path_b, &pids_b, Duration::from_secs(30)).await?;

    // Pod removal: delete both pods and ensure Removed events plus cleanup.
    run_kubectl(&["delete", "pod", "e2e-a", "--timeout=60s"]).await?;
    run_kubectl(&["delete", "pod", "e2e-b", "--timeout=60s"]).await?;
    wait_for_pod_removed(&mut rx, &pod_a_uid, Duration::from_secs(120)).await?;
    wait_for_group_absent(&group_path_a, Duration::from_secs(60)).await?;
    wait_for_pod_removed(&mut rx, &pod_b_uid, Duration::from_secs(120)).await?;
    wait_for_group_absent(&group_path_b, Duration::from_secs(60)).await?;

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
