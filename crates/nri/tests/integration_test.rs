use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{Api, DeleteParams, PostParams, ResourceExt},
    Client,
};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::*;

use nri::metadata::{ContainerMetadata, MetadataMessage, MetadataPlugin};
use nri::NRI;
use nri::{api, api_ttrpc::Plugin};
use ttrpc::r#async::TtrpcContext;

// Minimal plugin to capture container create events and run inline verification
type CheckFn =
    dyn Fn(&api::Container, Option<&api::PodSandbox>) -> anyhow::Result<String> + Send + Sync;

#[derive(Clone)]
struct EventCapturePlugin {
    tx: mpsc::Sender<String>,
    check: std::sync::Arc<CheckFn>,
}

impl EventCapturePlugin {
    fn new(tx: mpsc::Sender<String>, check: std::sync::Arc<CheckFn>) -> Self {
        Self { tx, check }
    }
}

#[async_trait::async_trait]
impl Plugin for EventCapturePlugin {
    async fn configure(
        &self,
        _ctx: &TtrpcContext,
        _req: api::ConfigureRequest,
    ) -> ttrpc::Result<api::ConfigureResponse> {
        let mut events = nri::events_mask::EventMask::new();
        events.set(&[api::Event::START_CONTAINER]);
        Ok(api::ConfigureResponse {
            events: events.raw_value(),
            special_fields: protobuf::SpecialFields::default(),
        })
    }

    async fn synchronize(
        &self,
        _ctx: &TtrpcContext,
        _req: api::SynchronizeRequest,
    ) -> ttrpc::Result<api::SynchronizeResponse> {
        // We don't need initial state for this test
        Ok(api::SynchronizeResponse::default())
    }

    async fn create_container(
        &self,
        _ctx: &TtrpcContext,
        _req: api::CreateContainerRequest,
    ) -> ttrpc::Result<api::CreateContainerResponse> {
        Ok(api::CreateContainerResponse::default())
    }

    async fn update_container(
        &self,
        _ctx: &TtrpcContext,
        _req: api::UpdateContainerRequest,
    ) -> ttrpc::Result<api::UpdateContainerResponse> {
        Ok(api::UpdateContainerResponse::default())
    }

    async fn stop_container(
        &self,
        _ctx: &TtrpcContext,
        _req: api::StopContainerRequest,
    ) -> ttrpc::Result<api::StopContainerResponse> {
        Ok(api::StopContainerResponse::default())
    }

    async fn update_pod_sandbox(
        &self,
        _ctx: &TtrpcContext,
        _req: api::UpdatePodSandboxRequest,
    ) -> ttrpc::Result<api::UpdatePodSandboxResponse> {
        Ok(api::UpdatePodSandboxResponse::default())
    }

    async fn state_change(
        &self,
        _ctx: &TtrpcContext,
        req: api::StateChangeEvent,
    ) -> ttrpc::Result<api::Empty> {
        // Only act on START_CONTAINER events
        if let Ok(api::Event::START_CONTAINER) = req.event.enum_value() {
            let container = req.container.as_ref().cloned().unwrap_or_default();
            let pod = req.pod.as_ref().cloned();
            if let Ok(pod_name) = (self.check)(&container, pod.as_ref()) {
                let _ = self.tx.try_send(pod_name);
            }
        }
        Ok(api::Empty::default())
    }

    async fn shutdown(&self, _ctx: &TtrpcContext, _req: api::Empty) -> ttrpc::Result<api::Empty> {
        Ok(api::Empty::default())
    }
}

// Helper function to create a test pod
async fn create_test_pod(
    api: &Api<Pod>,
    name: &str,
    labels: Option<HashMap<String, String>>,
    annotations: Option<HashMap<String, String>>,
) -> anyhow::Result<Pod> {
    // Prepare labels
    let mut pod_labels = HashMap::new();
    pod_labels.insert("app".to_string(), "nri-test".to_string());

    if let Some(extra_labels) = labels {
        pod_labels.extend(extra_labels);
    }

    // Prepare annotations
    let mut pod_annotations = HashMap::new();
    pod_annotations.insert("nri-test".to_string(), "true".to_string());

    if let Some(extra_annotations) = annotations {
        pod_annotations.extend(extra_annotations);
    }

    // Create pod spec
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "labels": pod_labels,
            "annotations": pod_annotations
        },
        "spec": {
            "containers": [{
                "name": "test-container",
                "image": "busybox:latest",
                "command": ["sleep", "3600"]
            }],
        }
    }))?;

    // Create the pod
    let pp = PostParams::default();
    let created_pod = api.create(&pp, &pod).await?;

    info!("Created pod: {}", created_pod.name_any());

    Ok(created_pod)
}

// Helper function to wait for a pod to be running
async fn wait_for_pod_running(api: &Api<Pod>, name: &str) -> anyhow::Result<Pod> {
    let timeout_duration = Duration::from_secs(60);
    let start_time = std::time::Instant::now();

    loop {
        let pod = api.get(name).await?;

        if let Some(status) = &pod.status {
            if let Some(phase) = &status.phase {
                if phase == "Running" {
                    info!("Pod {} is now running", name);
                    return Ok(pod);
                }
            }
        }

        if start_time.elapsed() > timeout_duration {
            return Err(anyhow::anyhow!(
                "Timeout waiting for pod {} to be running",
                name
            ));
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// Helper function to delete a pod
async fn delete_pod(api: &Api<Pod>, name: &str) -> anyhow::Result<()> {
    // Set grace period to 1 second for faster deletion
    let dp = DeleteParams {
        grace_period_seconds: Some(1),
        ..DeleteParams::default()
    };
    api.delete(name, &dp).await?;
    info!("Deleted pod: {}", name);

    // Wait for pod to be deleted
    let timeout_duration = Duration::from_secs(60);
    let start_time = std::time::Instant::now();

    loop {
        match api.get(name).await {
            Ok(_) => {
                if start_time.elapsed() > timeout_duration {
                    return Err(anyhow::anyhow!(
                        "Timeout waiting for pod {} to be deleted",
                        name
                    ));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(_) => {
                info!("Pod {} has been deleted", name);
                return Ok(());
            }
        }
    }
}

// Check function executed inside the plugin for each container event.
// Returns the verified pod name on success.
fn verify_cgroup_path_and_return_pod_name(
    container: &nri::api::Container,
    pod: Option<&nri::api::PodSandbox>,
) -> anyhow::Result<String> {
    let full_path = nri::compute_full_cgroup_path(container, pod);
    info!("Computed cgroup path: {}", full_path);

    if !full_path.starts_with("/sys/fs/cgroup") {
        return Err(anyhow::anyhow!(
            "cgroup path does not start with /sys/fs/cgroup: {}",
            full_path
        ));
    }

    let scope_dir = std::path::Path::new(&full_path);
    if !scope_dir.exists() {
        let cgroup_root = std::path::Path::new("/sys/fs/cgroup");
        let mut current = scope_dir.parent();
        while let Some(dir) = current {
            eprintln!("\nListing for {}:", dir.display());
            match std::fs::read_dir(dir) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let file_type = entry.file_type().ok();
                        let marker = match file_type {
                            Some(ft) if ft.is_dir() => "/",
                            Some(ft) if ft.is_symlink() => "@",
                            _ => "",
                        };
                        eprintln!(
                            "  {}{}",
                            path.file_name().unwrap().to_string_lossy(),
                            marker
                        );
                    }
                }
                Err(err) => {
                    eprintln!("  <failed to read_dir: {}>", err);
                }
            }
            if dir == cgroup_root {
                break;
            }
            current = dir.parent();
        }
        return Err(anyhow::anyhow!("cgroup path does not exist: {}", full_path));
    }
    if !scope_dir.is_dir() {
        return Err(anyhow::anyhow!(
            "cgroup path is not a directory: {}",
            full_path
        ));
    }

    let procs_candidates = ["cgroup.procs", "cgroups.procs"]; // accept both just in case
    let mut pids: Vec<String> = Vec::new();
    for fname in procs_candidates {
        let p = scope_dir.join(fname);
        if p.exists() {
            // Try to open and read the file (might be empty)
            match std::fs::read_to_string(&p) {
                Ok(content) => {
                    for line in content.lines() {
                        let l = line.trim();
                        if !l.is_empty() {
                            pids.push(l.to_string());
                        }
                    }
                    break;
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Failed to read {}: {}", p.display(), e));
                }
            }
        }
    }
    if pids.is_empty() {
        return Err(anyhow::anyhow!(
            "No PIDs listed in cgroup.procs under {}",
            full_path
        ));
    }

    // Log details for each PID we found to aid auditing
    for pid in &pids {
        let proc_dir = std::path::Path::new("/proc").join(pid);
        let cmdline_path = proc_dir.join("cmdline");
        let comm_path = proc_dir.join("comm");

        let cmdline_str = match std::fs::read(&cmdline_path) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    String::from("")
                } else {
                    // /proc/<pid>/cmdline is NUL-separated
                    let parts: Vec<String> = bytes
                        .split(|b| *b == 0)
                        .filter(|s| !s.is_empty())
                        .map(|s| String::from_utf8_lossy(s).to_string())
                        .collect();
                    parts.join(" ")
                }
            }
            Err(e) => format!("<error reading cmdline: {}>", e),
        };

        let comm_str = match std::fs::read_to_string(&comm_path) {
            Ok(s) => s.trim().to_string(),
            Err(e) => format!("<error reading comm: {}>", e),
        };

        info!(
            pid = %pid,
            cmdline = %cmdline_str,
            comm = %comm_str,
            scope_dir = %scope_dir.display(),
            "Process in cgroup"
        );
    }

    // Return the pod name if known
    let pod_name = pod
        .and_then(|p| {
            if p.name.is_empty() {
                None
            } else {
                Some(p.name.clone())
            }
        })
        .unwrap_or_else(|| "<unknown-pod>".to_string());

    Ok(pod_name)
}

#[tokio::test]
#[ignore] // Requires a real Kubernetes cluster and NRI socket
async fn test_compute_full_cgroup_path_kubernetes() -> anyhow::Result<()> {
    // Initialize tracing
    let _ = tracing_subscriber::fmt::try_init();

    // Check NRI socket first; skip if not present
    let socket_path =
        std::env::var("NRI_SOCKET_PATH").unwrap_or_else(|_| "/var/run/nri/nri.sock".to_string());
    if !Path::new(&socket_path).exists() {
        info!("NRI socket not found at {}, skipping test", socket_path);
        return Ok(());
    }

    // Connect to Kubernetes
    let client = Client::try_default().await?;
    let pods: Api<Pod> = Api::default_namespaced(client.clone());
    let namespace = "default".to_string();
    info!("Using namespace: {}", namespace);

    // Create a channel to receive pod verifications
    let (tx, mut rx) = mpsc::channel::<String>(100);
    let plugin = std::sync::Arc::new(EventCapturePlugin::new(
        tx,
        std::sync::Arc::new(verify_cgroup_path_and_return_pod_name),
    ));

    // Connect to the NRI socket and register the plugin
    info!("Connecting to NRI socket at {}", socket_path);
    let socket = tokio::net::UnixStream::connect(&socket_path).await?;
    let (nri, join_handle) = NRI::new(socket, plugin, "cgroup-path-test-plugin", "10").await?;
    nri.register().await?;

    // Create a new pod to trigger a CreateContainer event
    let pod_name = format!(
        "nri-cgroup-path-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    info!("Creating test pod: {}", pod_name);
    let _pod = create_test_pod(&pods, &pod_name, None, None).await?;
    let _running_pod = wait_for_pod_running(&pods, &pod_name).await?;

    // Wait for the plugin to verify our pod inline with the event
    let timeout_duration = Duration::from_secs(60);
    match timeout(timeout_duration, async {
        loop {
            if let Some(verified) = rx.recv().await {
                if verified == pod_name {
                    break Ok(());
                }
            } else {
                break Err(anyhow::anyhow!("Verification channel closed"));
            }
        }
    })
    .await
    {
        Ok(res) => res?,
        Err(_) => Err(anyhow::anyhow!(
            "Timeout waiting for verification for pod {}",
            pod_name
        ))?,
    };

    // After initial synchronization, create another pod and verify its cgroup
    let pod_name2 = format!(
        "nri-cgroup-path-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    info!("Creating second test pod: {}", pod_name2);
    let _pod2 = create_test_pod(&pods, &pod_name2, None, None).await?;
    let _running_pod2 = wait_for_pod_running(&pods, &pod_name2).await?;

    // Wait for the plugin to verify the second pod inline with the event
    let timeout_duration = Duration::from_secs(60);
    match timeout(timeout_duration, async {
        loop {
            if let Some(verified) = rx.recv().await {
                if verified == pod_name2 {
                    break Ok(());
                }
            } else {
                break Err(anyhow::anyhow!("Verification channel closed"));
            }
        }
    })
    .await
    {
        Ok(res) => res?,
        Err(_) => Err(anyhow::anyhow!(
            "Timeout waiting for verification for pod {}",
            pod_name2
        ))?,
    };

    // Cleanup: delete pods and close NRI connection
    info!("Deleting test pod: {}", pod_name);
    delete_pod(&pods, &pod_name).await?;
    info!("Deleting second test pod: {}", pod_name2);
    delete_pod(&pods, &pod_name2).await?;

    info!("Closing NRI connection");
    nri.close().await?;
    join_handle.await??;

    Ok(())
}

// Helper function to find a container in the metadata receiver by pod name
async fn find_container_by_pod_name(
    metadata_rx: &mut mpsc::Receiver<MetadataMessage>,
    pod_name: &str,
    timeout_duration: Duration,
) -> anyhow::Result<ContainerMetadata> {
    let result = timeout(timeout_duration, async {
        while let Some(msg) = metadata_rx.recv().await {
            match msg {
                MetadataMessage::Add(_, metadata) if metadata.as_ref().pod_name == pod_name => {
                    return Ok(*metadata);
                }
                _ => continue,
            }
        }
        Err(anyhow::anyhow!("Channel closed without finding container"))
    })
    .await;

    match result {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("Timeout waiting for container metadata")),
    }
}

// Helper function to verify container removal
async fn verify_container_removal(
    metadata_rx: &mut mpsc::Receiver<MetadataMessage>,
    container_id: &str,
    timeout_duration: Duration,
) -> anyhow::Result<()> {
    let result = timeout(timeout_duration, async {
        while let Some(msg) = metadata_rx.recv().await {
            match msg {
                MetadataMessage::Remove(id) if id == container_id => {
                    return Ok(());
                }
                _ => continue,
            }
        }
        Err(anyhow::anyhow!(
            "Channel closed without finding container removal"
        ))
    })
    .await;

    match result {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("Timeout waiting for container removal")),
    }
}

// Helper function to collect all initial containers
async fn collect_initial_containers(
    metadata_rx: &mut mpsc::Receiver<MetadataMessage>,
    timeout_duration: Duration,
) -> anyhow::Result<HashMap<String, ContainerMetadata>> {
    let mut containers = HashMap::new();

    // Use a timeout to avoid waiting indefinitely
    let result = timeout(timeout_duration, async {
        let mut last_received = std::time::Instant::now();
        let quiet_period = Duration::from_secs(2); // Consider done if no messages for 2 seconds

        while last_received.elapsed() < quiet_period {
            match tokio::time::timeout(quiet_period, metadata_rx.recv()).await {
                Ok(Some(MetadataMessage::Add(id, metadata))) => {
                    containers.insert(id, *metadata);
                    last_received = std::time::Instant::now();
                }
                Ok(Some(_)) => {
                    last_received = std::time::Instant::now();
                }
                Ok(None) => break, // Channel closed
                Err(_) => {
                    // Timeout on receive, check if we've been quiet long enough
                    if last_received.elapsed() >= quiet_period {
                        break;
                    }
                }
            }
        }

        Ok(containers)
    })
    .await;

    match result {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("Timeout collecting initial containers")),
    }
}

#[tokio::test]
#[ignore] // Ignore by default as it requires a real Kubernetes cluster
async fn test_metadata_plugin_with_kubernetes() -> anyhow::Result<()> {
    // Initialize tracing
    let _ = tracing_subscriber::fmt::try_init();

    // Connect to Kubernetes
    let client = Client::try_default().await?;
    let pods: Api<Pod> = Api::default_namespaced(client.clone());

    // Get the current namespace (default for default_namespaced API)
    let namespace = "default".to_string();
    info!("Using namespace: {}", namespace);

    // Create a "pre-existing" pod before connecting to NRI
    let pre_existing_pod_name = "nri-pre-existing-pod";

    // Add custom labels and annotations for the pre-existing pod
    // Note: These are pod-level and may not be directly visible in container metadata
    let mut pre_existing_labels = HashMap::new();
    pre_existing_labels.insert("test-label".to_string(), "pre-existing-value".to_string());
    pre_existing_labels.insert("component".to_string(), "nri-test-pre".to_string());

    let mut pre_existing_annotations = HashMap::new();
    pre_existing_annotations.insert(
        "test-annotation".to_string(),
        "pre-existing-annotation-value".to_string(),
    );
    pre_existing_annotations.insert("io.kubernetes.pod/role".to_string(), "test-pre".to_string());

    info!("Creating pre-existing test pod: {}", pre_existing_pod_name);
    let _pre_existing_pod = create_test_pod(
        &pods,
        pre_existing_pod_name,
        Some(pre_existing_labels.clone()),
        Some(pre_existing_annotations.clone()),
    )
    .await?;

    // Wait for pre-existing pod to be running
    let running_pre_existing_pod = wait_for_pod_running(&pods, pre_existing_pod_name).await?;
    let pre_existing_pod_uid = running_pre_existing_pod
        .metadata
        .uid
        .as_ref()
        .unwrap()
        .clone();
    info!(
        "Pre-existing pod is running with UID: {}",
        pre_existing_pod_uid
    );

    // Create a channel for metadata updates
    let (tx, mut rx) = mpsc::channel(100);

    // Create metadata plugin
    let plugin = std::sync::Arc::new(MetadataPlugin::new(tx));

    // Path to the NRI socket - this would be the actual socket in a real environment
    // For testing, we might need to mock this or use a real socket if testing in a container
    let socket_path =
        std::env::var("NRI_SOCKET_PATH").unwrap_or_else(|_| "/var/run/nri/nri.sock".to_string());

    // Check if socket exists, if not we'll skip the test
    if !Path::new(&socket_path).exists() {
        info!("NRI socket not found at {}, skipping test", socket_path);
        // Clean up pre-existing pod before returning
        delete_pod(&pods, pre_existing_pod_name).await?;
        return Ok(());
    }

    // Connect to the socket
    info!("Connecting to NRI socket at {}", socket_path);
    let socket = tokio::net::UnixStream::connect(&socket_path).await?;

    // Create NRI instance
    let (nri, join_handle) = NRI::new(socket, plugin, "metadata-test-plugin", "10").await?;

    // Register the plugin with the runtime
    nri.register().await?;

    // Wait a moment for initial synchronization
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Collect all pre-existing containers
    info!("Collecting pre-existing containers");
    let pre_existing = collect_initial_containers(&mut rx, Duration::from_secs(10)).await?;
    info!("Found {} pre-existing containers", pre_existing.len());

    // Verify our pre-existing container is found
    let mut found_pre_existing = false;
    let mut pre_existing_container_id = String::new();
    let mut pre_existing_container_metadata = None;

    for (id, metadata) in &pre_existing {
        if metadata.pod_name == pre_existing_pod_name && metadata.pod_uid == pre_existing_pod_uid {
            info!("Found our pre-existing container with ID: {}", id);
            found_pre_existing = true;
            pre_existing_container_id = id.clone();
            pre_existing_container_metadata = Some(metadata.clone());
            break;
        }
    }

    assert!(
        found_pre_existing,
        "Pre-existing container was not found in metadata"
    );

    // Verify pre-existing container metadata
    let pre_metadata = pre_existing_container_metadata.unwrap();
    info!("Pre-existing container metadata: {:?}", pre_metadata);

    // Verify basic metadata
    assert_eq!(pre_metadata.pod_name, pre_existing_pod_name);
    assert_eq!(pre_metadata.pod_uid, pre_existing_pod_uid);
    assert_eq!(pre_metadata.pod_namespace, namespace);

    // Verify container labels - these are set by the container runtime
    assert!(pre_metadata.labels.contains_key("io.kubernetes.pod.name"));
    assert_eq!(
        pre_metadata.labels.get("io.kubernetes.pod.name"),
        Some(&pre_existing_pod_name.to_string())
    );
    assert!(pre_metadata
        .labels
        .contains_key("io.kubernetes.pod.namespace"));
    assert_eq!(
        pre_metadata.labels.get("io.kubernetes.pod.namespace"),
        Some(&namespace)
    );
    assert!(pre_metadata.labels.contains_key("io.kubernetes.pod.uid"));
    assert_eq!(
        pre_metadata.labels.get("io.kubernetes.pod.uid"),
        Some(&pre_existing_pod_uid)
    );
    assert!(pre_metadata
        .labels
        .contains_key("io.kubernetes.container.name"));
    assert_eq!(
        pre_metadata.labels.get("io.kubernetes.container.name"),
        Some(&"test-container".to_string())
    );

    // Verify container annotations - these are set by the container runtime
    assert!(pre_metadata
        .annotations
        .contains_key("io.kubernetes.cri.sandbox-name"));
    assert_eq!(
        pre_metadata
            .annotations
            .get("io.kubernetes.cri.sandbox-name"),
        Some(&pre_existing_pod_name.to_string())
    );
    assert!(pre_metadata
        .annotations
        .contains_key("io.kubernetes.cri.sandbox-namespace"));
    assert_eq!(
        pre_metadata
            .annotations
            .get("io.kubernetes.cri.sandbox-namespace"),
        Some(&namespace)
    );
    assert!(pre_metadata
        .annotations
        .contains_key("io.kubernetes.cri.sandbox-uid"));
    assert_eq!(
        pre_metadata
            .annotations
            .get("io.kubernetes.cri.sandbox-uid"),
        Some(&pre_existing_pod_uid)
    );

    // Create a new test pod after NRI connection
    let new_pod_name = "nri-test-pod";

    // Add custom labels and annotations for the new pod
    // Note: These are pod-level and may not be directly visible in container metadata
    let mut new_pod_labels = HashMap::new();
    new_pod_labels.insert("test-label".to_string(), "new-pod-value".to_string());
    new_pod_labels.insert("component".to_string(), "nri-test-new".to_string());

    let mut new_pod_annotations = HashMap::new();
    new_pod_annotations.insert(
        "test-annotation".to_string(),
        "new-pod-annotation-value".to_string(),
    );
    new_pod_annotations.insert("io.kubernetes.pod/role".to_string(), "test-new".to_string());

    info!("Creating new test pod: {}", new_pod_name);
    let _new_pod = create_test_pod(
        &pods,
        new_pod_name,
        Some(new_pod_labels.clone()),
        Some(new_pod_annotations.clone()),
    )
    .await?;

    // Wait for new pod to be running
    let running_new_pod = wait_for_pod_running(&pods, new_pod_name).await?;
    let new_pod_uid = running_new_pod.metadata.uid.as_ref().unwrap().clone();

    // Wait for new container metadata to appear
    info!("Waiting for container metadata for pod: {}", new_pod_name);
    let new_container_metadata =
        find_container_by_pod_name(&mut rx, new_pod_name, Duration::from_secs(30)).await?;
    info!("New container metadata: {:?}", new_container_metadata);

    // Verify new container metadata matches the pod
    assert_eq!(new_container_metadata.pod_name, new_pod_name);
    assert_eq!(&new_container_metadata.pod_uid, &new_pod_uid);
    assert_eq!(new_container_metadata.pod_namespace, namespace);

    // Verify container labels - these are set by the container runtime
    assert!(new_container_metadata
        .labels
        .contains_key("io.kubernetes.pod.name"));
    assert_eq!(
        new_container_metadata.labels.get("io.kubernetes.pod.name"),
        Some(&new_pod_name.to_string())
    );
    assert!(new_container_metadata
        .labels
        .contains_key("io.kubernetes.pod.namespace"));
    assert_eq!(
        new_container_metadata
            .labels
            .get("io.kubernetes.pod.namespace"),
        Some(&namespace)
    );
    assert!(new_container_metadata
        .labels
        .contains_key("io.kubernetes.pod.uid"));
    assert_eq!(
        new_container_metadata.labels.get("io.kubernetes.pod.uid"),
        Some(&new_pod_uid)
    );
    assert!(new_container_metadata
        .labels
        .contains_key("io.kubernetes.container.name"));
    assert_eq!(
        new_container_metadata
            .labels
            .get("io.kubernetes.container.name"),
        Some(&"test-container".to_string())
    );

    // Verify container annotations - these are set by the container runtime
    assert!(new_container_metadata
        .annotations
        .contains_key("io.kubernetes.cri.sandbox-name"));
    assert_eq!(
        new_container_metadata
            .annotations
            .get("io.kubernetes.cri.sandbox-name"),
        Some(&new_pod_name.to_string())
    );
    assert!(new_container_metadata
        .annotations
        .contains_key("io.kubernetes.cri.sandbox-namespace"));
    assert_eq!(
        new_container_metadata
            .annotations
            .get("io.kubernetes.cri.sandbox-namespace"),
        Some(&namespace)
    );
    assert!(new_container_metadata
        .annotations
        .contains_key("io.kubernetes.cri.sandbox-uid"));
    assert_eq!(
        new_container_metadata
            .annotations
            .get("io.kubernetes.cri.sandbox-uid"),
        Some(&new_pod_uid)
    );

    // Store new container ID for later verification
    let new_container_id = new_container_metadata.container_id.clone();
    info!("Found new container ID: {}", new_container_id);

    // Delete the new pod
    info!("Deleting new pod: {}", new_pod_name);
    delete_pod(&pods, new_pod_name).await?;

    // Verify new container removal
    info!(
        "Verifying new container removal for ID: {}",
        new_container_id
    );
    verify_container_removal(&mut rx, &new_container_id, Duration::from_secs(30)).await?;

    // Delete the pre-existing pod
    info!("Deleting pre-existing pod: {}", pre_existing_pod_name);
    delete_pod(&pods, pre_existing_pod_name).await?;

    // Verify pre-existing container removal
    info!(
        "Verifying pre-existing container removal for ID: {}",
        pre_existing_container_id
    );
    verify_container_removal(&mut rx, &pre_existing_container_id, Duration::from_secs(30)).await?;

    // Close the NRI connection
    info!("Closing NRI connection");
    nri.close().await?;

    // Wait for the join handle to complete
    join_handle.await??;

    Ok(())
}
