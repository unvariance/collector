use std::backtrace::BacktraceStatus;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{Api, DeleteParams, Patch, PatchParams, PostParams, ResourceExt},
    Client, Error as KubeError,
};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::Instant;

const MAIN_CONTAINER_NAME: &str = "main";

fn init_test_logger() {
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }
    if std::env::var_os("RUST_LIB_BACKTRACE").is_none() {
        std::env::set_var("RUST_LIB_BACKTRACE", "1");
    }
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

fn report_error(err: &anyhow::Error) {
    eprintln!("[integration_test] error details: {err:?}");
    let bt = err.backtrace();
    match bt.status() {
        BacktraceStatus::Captured => {
            eprintln!("[integration_test] captured backtrace:\n{bt}");
        }
        status => {
            eprintln!(
                "[integration_test] backtrace unavailable (status: {:?})",
                status
            );
        }
    }
}

async fn run_with_error_reporting<F>(fut: F) -> anyhow::Result<()>
where
    F: Future<Output = anyhow::Result<()>>,
{
    match fut.await {
        Ok(()) => Ok(()),
        Err(err) => {
            report_error(&err);
            Err(err)
        }
    }
}

use nri::NRI;
use nri_resctrl_plugin::{
    PodResctrlAddOrUpdate, PodResctrlEvent, ResctrlGroupState, ResctrlPlugin, ResctrlPluginConfig,
};

async fn delete_pod_if_exists(pods: &Api<Pod>, name: &str) -> anyhow::Result<()> {
    let dp = DeleteParams {
        grace_period_seconds: Some(0),
        ..DeleteParams::default()
    };
    match pods.delete(name, &dp).await {
        Ok(_resp) => {}
        Err(KubeError::Api(ae)) if ae.code == 404 => return Ok(()),
        Err(e) => return Err(e.into()),
    }

    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match pods.get(name).await {
            Ok(_) => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for pod {} deletion", name);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(KubeError::Api(ae)) if ae.code == 404 => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

async fn create_sleep_pod(pods: &Api<Pod>, name: &str) -> anyhow::Result<Pod> {
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
        },
        "spec": {
            "containers": [{
                "name": "main",
                "image": "busybox",
                "command": ["/bin/sh", "-c", "sleep 3600"],
                "imagePullPolicy": "IfNotPresent",
            }],
        }
    }))?;
    Ok(pods.create(&PostParams::default(), &pod).await?)
}

async fn wait_for_pod_running(
    pods: &Api<Pod>,
    name: &str,
    timeout: Duration,
) -> anyhow::Result<Pod> {
    let deadline = Instant::now() + timeout;
    loop {
        let pod = pods.get(name).await?;
        if let Some(status) = &pod.status {
            if status.phase.as_deref() == Some("Running") {
                let ready = status
                    .container_statuses
                    .as_ref()
                    .map(|statuses| statuses.iter().all(|cs| cs.ready))
                    .unwrap_or(false);
                if ready {
                    return Ok(pod);
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for pod {} to reach Running", name);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn gather_container_ids_from_pod(pod: &Pod) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(status) = &pod.status {
        if let Some(containers) = &status.container_statuses {
            for cs in containers {
                if let Some(id) = &cs.container_id {
                    if !id.is_empty() {
                        out.push(id.clone());
                    }
                }
            }
        }
        if let Some(ephemeral) = &status.ephemeral_container_statuses {
            for ecs in ephemeral {
                if let Some(id) = &ecs.container_id {
                    if !id.is_empty() {
                        out.push(id.clone());
                    }
                }
            }
        }
    }
    out
}

async fn wait_for_container_ids(
    pods: &Api<Pod>,
    pod_name: &str,
    expected: usize,
    timeout: Duration,
) -> anyhow::Result<Vec<String>> {
    let deadline = Instant::now() + timeout;
    loop {
        let pod = pods.get(pod_name).await?;
        let ids = gather_container_ids_from_pod(&pod);
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

fn find_container_scope_path(container_id: &str) -> anyhow::Result<PathBuf> {
    let target_name = format!("cri-containerd-{}.scope", container_id);
    let mut stack = vec![PathBuf::from("/sys/fs/cgroup")];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "reading directory {} while searching for container scope: {}",
                    dir.display(),
                    e
                ))
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
                Err(e) => return Err(e.into()),
            };

            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
                Err(e) => return Err(e.into()),
            };

            if !file_type.is_dir() {
                continue;
            }

            let path = entry.path();
            if entry.file_name().to_string_lossy() == target_name {
                return Ok(path);
            }

            stack.push(path);
        }
    }

    bail!(
        "unable to locate container scope cri-containerd-{}.scope within /sys/fs/cgroup",
        container_id
    )
}

async fn resolve_container_pids(ids: &[String]) -> anyhow::Result<Vec<i32>> {
    let mut resolved = Vec::with_capacity(ids.len());

    for id in ids {
        let trimmed = trim_container_runtime_prefix(id);
        let scope_dir = find_container_scope_path(trimmed)
            .with_context(|| format!("locating scope for container id {}", id))?;

        let procs_path = scope_dir.join("cgroup.procs");
        let procs_contents = match tokio::fs::read_to_string(&procs_path).await {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let tasks_path = scope_dir.join("tasks");
                tokio::fs::read_to_string(&tasks_path)
                    .await
                    .with_context(|| format!("reading PID list from {}", tasks_path.display()))?
            }
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "reading PID list from {}: {}",
                    procs_path.display(),
                    err
                ));
            }
        };

        let pid = procs_contents
            .lines()
            .filter_map(|line| line.trim().parse::<i32>().ok())
            .next();

        if let Some(pid) = pid {
            resolved.push(pid);
        } else {
            return Err(anyhow::anyhow!(
                "no PIDs listed in cgroup for container {} at {}",
                id,
                scope_dir.display()
            ));
        }
    }

    Ok(resolved)
}

async fn wait_for_tasks_with_pids(
    group_path: &str,
    expected_pids: &[i32],
    timeout: Duration,
) -> anyhow::Result<Vec<i32>> {
    let deadline = Instant::now() + timeout;
    let rc = resctrl::Resctrl::default();
    loop {
        match rc.list_group_tasks(group_path) {
            Ok(pids) => {
                if expected_pids.iter().all(|pid| pids.contains(pid)) {
                    return Ok(pids);
                }
            }
            Err(resctrl::Error::Io { path: _, source })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                // Group or tasks file not found yet; keep waiting until timeout
            }
            Err(e) => return Err(e.into()),
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for resctrl group {} to include {:?}",
                group_path,
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
        eprintln!("[integration_test] saw event: {:?}", ev);
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
        eprintln!("[integration_test] saw event: {:?}", ev);
        match ev {
            PodResctrlEvent::Removed(r) if r.pod_uid == pod_uid => return Ok(()),
            _ => {}
        }
    }
}

async fn add_ephemeral_container(
    pods: &Api<Pod>,
    pod_name: &str,
    target_container: &str,
) -> anyhow::Result<()> {
    let patch = json!({
        "spec": {
            "ephemeralContainers": [{
                "name": "dbg",
                "image": "busybox",
                "command": ["/bin/sh", "-c", "sleep 600"],
                "targetContainerName": target_container,
            }]
        }
    });
    pods.patch_ephemeral_containers(pod_name, &PatchParams::default(), &Patch::Strategic(patch))
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_plugin_full_flow() -> anyhow::Result<()> {
    init_test_logger();
    run_with_error_reporting(test_plugin_full_flow_impl()).await
}

async fn test_plugin_full_flow_impl() -> anyhow::Result<()> {
    // Use NRI socket path provided by the workflow via env.
    let socket_path = std::env::var("NRI_SOCKET_PATH")?;
    println!("[integration_test] Using NRI socket at: {}", socket_path);

    let client = Client::try_default().await?;
    let pods: Api<Pod> = Api::default_namespaced(client);

    // Ensure a clean slate and create the preexisting pod before plugin registration
    delete_pod_if_exists(&pods, "e2e-a").await?;
    delete_pod_if_exists(&pods, "e2e-b").await?;

    let preexisting = create_sleep_pod(&pods, "e2e-a").await?;
    let namespace = preexisting
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    println!(
        "[integration_test] Created preexisting pod e2e-a in namespace {}",
        namespace
    );
    let running_a = wait_for_pod_running(&pods, "e2e-a", Duration::from_secs(120)).await?;
    let pod_a_uid = running_a
        .metadata
        .uid
        .clone()
        .context("pod e2e-a missing metadata.uid")?;
    let containers_a = wait_for_container_ids(&pods, "e2e-a", 1, Duration::from_secs(90)).await?;
    let pids_a = resolve_container_pids(&containers_a).await?;

    // Connect to NRI runtime socket
    let socket = tokio::net::UnixStream::connect(&socket_path)
        .await
        .with_context(|| format!("failed to connect to NRI socket at {}", socket_path))?;
    println!("[integration_test] Connected to NRI socket");

    // Build plugin with an externally provided channel
    let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(256);
    let plugin = std::sync::Arc::new(ResctrlPlugin::new(ResctrlPluginConfig::default(), tx));

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

    // Post-start: add an ephemeral container using the Kubernetes API and ensure counts improve.
    add_ephemeral_container(&pods, "e2e-a", MAIN_CONTAINER_NAME).await?;
    let containers_a_after =
        wait_for_container_ids(&pods, "e2e-a", 2, Duration::from_secs(120)).await?;
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
    let _new_pod = create_sleep_pod(&pods, "e2e-b").await?;
    let running_b = wait_for_pod_running(&pods, "e2e-b", Duration::from_secs(120)).await?;
    let pod_b_uid = running_b
        .metadata
        .uid
        .clone()
        .context("pod e2e-b missing metadata.uid")?;
    let containers_b = wait_for_container_ids(&pods, "e2e-b", 1, Duration::from_secs(90)).await?;
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
    delete_pod_if_exists(&pods, "e2e-a").await?;
    wait_for_pod_removed(&mut rx, &pod_a_uid, Duration::from_secs(120)).await?;
    wait_for_group_absent(&group_path_a, Duration::from_secs(60)).await?;
    delete_pod_if_exists(&pods, "e2e-b").await?;
    wait_for_pod_removed(&mut rx, &pod_b_uid, Duration::from_secs(120)).await?;
    wait_for_group_absent(&group_path_b, Duration::from_secs(60)).await?;

    // Shut down cleanly
    nri.close().await?;
    join_handle.await??;
    println!("[integration_test] Plugin shutdown completed");

    let fs = resctrl::RealFs;
    let report = resctrl::cleanup_prefix(&fs, std::path::Path::new("/sys/fs/resctrl"), "pod_")?;
    println!("[integration_test] Cleanup report (pod_): {:?}", report);
    println!("[integration_test] Cleanup complete");

    Ok(())
}

#[tokio::test]
#[cfg(target_os = "linux")]
#[ignore]
async fn test_startup_cleanup_e2e() -> anyhow::Result<()> {
    init_test_logger();
    run_with_error_reporting(test_startup_cleanup_e2e_impl()).await
}

#[cfg(target_os = "linux")]
async fn test_startup_cleanup_e2e_impl() -> anyhow::Result<()> {
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

    let fs = resctrl::RealFs;
    let report_e2e =
        resctrl::cleanup_prefix(&fs, std::path::Path::new("/sys/fs/resctrl"), "test_e2e_")?;
    println!(
        "[integration_test] Cleanup report (test_e2e_): {:?}",
        report_e2e
    );
    let report_np = resctrl::cleanup_prefix(&fs, std::path::Path::new("/sys/fs/resctrl"), "np_")?;
    println!("[integration_test] Cleanup report (np_): {:?}", report_np);
    println!("[integration_test] Cleanup complete");

    Ok(())
}

#[tokio::test]
#[cfg(target_os = "linux")]
#[ignore]
async fn test_capacity_retry_e2e() -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use std::fs;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use tokio::time::Duration;

    init_test_logger();

    // Ensure real resctrl is mounted.
    let rc = resctrl::Resctrl::default();
    if let Err(e) = rc.ensure_mounted(true) {
        eprintln!(
            "ensure_mounted failed (need CAP_SYS_ADMIN?): {} — skipping",
            e
        );
        return Ok(());
    }

    // Kubernetes client and pods API
    let client = Client::try_default().await?;
    let pods: Api<Pod> = Api::default_namespaced(client);

    // Clean slate pods for this test
    delete_pod_if_exists(&pods, "cap-a").await?;

    // Connect the plugin to the NRI runtime socket so it listens to containerd events directly.
    let socket_path = std::env::var("NRI_SOCKET_PATH")?;
    let socket = tokio::net::UnixStream::connect(&socket_path)
        .await
        .with_context(|| format!("failed to connect to NRI socket at {}", socket_path))?;

    // Build plugin and register via NRI
    let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(256);
    let plugin = std::sync::Arc::new(ResctrlPlugin::new(
        ResctrlPluginConfig {
            cleanup_on_start: false,
            ..Default::default()
        },
        tx,
    ));
    let (nri, _join_handle) = NRI::new(socket, plugin.clone(), "resctrl-plugin", "10").await?;
    nri.register().await?;

    // Wait for initial synchronization: drain events until channel is quiet for 1s
    // so that any existing pods are handled by the plugin before proceeding.
    let mut _drained = 0usize;
    loop {
        match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Some(_ev)) => {
                _drained += 1;
                continue;
            }
            Ok(None) => break, // channel closed
            Err(_) => break,   // no messages for a full second
        }
    }

    // Prepare resctrl capacity exhaustion using placeholder groups.
    let root = PathBuf::from("/sys/fs/resctrl");
    let filler_prefix = "capfill_e2e_";
    // Clean up any stale filler groups from previous runs.
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with(filler_prefix) {
                    let _ = fs::remove_dir(entry.path());
                }
            }
        }
    }
    let mut filler_dirs: Vec<PathBuf> = Vec::new();
    let mut enospc_hit = false;
    let num_rmids_path = root.join("info").join("L3_MON").join("num_rmids");
    let num_rmids_limit = fs::read_to_string(&num_rmids_path)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(1024);
    let filler_budget = num_rmids_limit.saturating_add(2);
    for idx in 0..filler_budget {
        let dir = root
            .join("mon_groups")
            .join(format!("{}{}", filler_prefix, idx));
        match fs::create_dir(&dir) {
            Ok(()) => filler_dirs.push(dir),
            Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                enospc_hit = true;
                break;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                filler_dirs.push(dir);
            }
            Err(e) => {
                for path in filler_dirs.iter().rev() {
                    let _ = fs::remove_dir(path);
                }
                return Err(e.into());
            }
        }
    }

    if !enospc_hit {
        for path in filler_dirs.iter().rev() {
            let _ = fs::remove_dir(path);
        }
        eprintln!("resctrl capacity not exhausted; skipping capacity retry test");
        return Ok(());
    }

    // Keep one directory to free later to simulate capacity becoming available.
    let freed_dir = filler_dirs
        .pop()
        .context("at least one filler dir expected")?;

    // Create a pod while capacity is exhausted → expect Failed group state and no reconciliation.
    let pod_a = create_sleep_pod(&pods, "cap-a").await?;
    let pod_a_uid = pod_a
        .metadata
        .uid
        .clone()
        .context("pod cap-a missing metadata.uid")?;
    let _running_a = wait_for_pod_running(&pods, "cap-a", Duration::from_secs(120)).await?;

    // First event for pod creation should indicate Failed group state, 0/0 containers.
    let ev_a0 = wait_for_pod_update(&mut rx, &pod_a_uid, 0, 0, Duration::from_secs(60)).await?;
    match ev_a0.group_state {
        ResctrlGroupState::Failed => {}
        _ => bail!("expected Failed group state for cap-a on capacity exhaustion"),
    }

    // When the container starts, expect totals to increase but still Failed and not reconciled.
    let ids_a = wait_for_container_ids(&pods, "cap-a", 1, Duration::from_secs(120)).await?;
    let pids_a = resolve_container_pids(&ids_a).await?; // We won't find tasks yet since group creation failed
    let ev_a1 = wait_for_pod_update(&mut rx, &pod_a_uid, 1, 0, Duration::from_secs(90)).await?;
    match ev_a1.group_state {
        ResctrlGroupState::Failed => {}
        _ => bail!("expected Failed group state for cap-a after container start"),
    }

    // Free one placeholder group to make capacity available.
    fs::remove_dir(&freed_dir)?;

    // Perform retry_all_once() every second until we observe:
    //  - an AddOrUpdate for cap-a with group_state = Exists
    //  - another AddOrUpdate for cap-a with reconciled_containers = 1
    // Timeout after 30 seconds.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut got_exists = false;
    let mut got_reconciled = false;
    let mut group_path_a: Option<String> = None;
    while Instant::now() < deadline && !(got_exists && got_reconciled) {
        // Trigger a single retry pass
        let _ = plugin.retry_all_once();

        // Drain events for up to 1s
        loop {
            match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Some(PodResctrlEvent::AddOrUpdate(add))) if add.pod_uid == pod_a_uid => {
                    if !got_exists {
                        if let ResctrlGroupState::Exists(ref p) = add.group_state {
                            got_exists = true;
                            group_path_a = Some(p.clone());
                        }
                    }
                    if add.reconciled_containers == 1 {
                        got_reconciled = true;
                    }
                    if got_exists && got_reconciled {
                        break;
                    }
                }
                Ok(Some(_)) => {
                    // unrelated event; keep draining within this 1s window
                    continue;
                }
                Ok(None) => break,
                Err(_) => break, // no messages for a full second
            }
        }
    }

    if !(got_exists && got_reconciled) {
        bail!("timed out waiting for retry events for cap-a");
    }

    // Verify: group exists and its PIDs equal the cgroup's PIDs
    let group_path_a = group_path_a.expect("group path for cap-a after retry");
    let tasks_contents = tokio::fs::read_to_string(Path::new(&group_path_a).join("tasks")).await?;
    let mut tasks_pids: Vec<i32> = tasks_contents
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .collect();
    tasks_pids.sort_unstable();
    let mut expected = pids_a.clone();
    expected.sort_unstable();
    assert_eq!(
        tasks_pids, expected,
        "group tasks should equal container cgroup PIDs"
    );
    assert!(!tasks_pids.is_empty(), "at least one PID expected in group");

    // Cleanup: delete pod cap-a and ensure its group is removed.
    delete_pod_if_exists(&pods, "cap-a").await?;
    let _ = wait_for_pod_removed(&mut rx, &pod_a_uid, Duration::from_secs(60)).await;
    let _ = wait_for_group_absent(&group_path_a, Duration::from_secs(30)).await;

    // Cleanup filler dirs.
    for path in filler_dirs.into_iter().rev() {
        let _ = fs::remove_dir(path);
    }

    let fs = resctrl::RealFs;
    let report_pod = resctrl::cleanup_prefix(&fs, std::path::Path::new("/sys/fs/resctrl"), "pod_")?;
    println!("[integration_test] Cleanup report (pod_): {:?}", report_pod);
    let report_capfill =
        resctrl::cleanup_prefix(&fs, std::path::Path::new("/sys/fs/resctrl"), filler_prefix)?;
    println!(
        "[integration_test] Cleanup report ({}): {:?}",
        filler_prefix, report_capfill
    );
    println!("[integration_test] Cleanup complete");

    Ok(())
}
