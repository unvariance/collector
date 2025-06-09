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

// Helper function to find a container in the metadata receiver by pod name
async fn find_container_by_pod_name(
    metadata_rx: &mut mpsc::Receiver<MetadataMessage>,
    pod_name: &str,
    timeout_duration: Duration,
) -> anyhow::Result<ContainerMetadata> {
    let result = timeout(timeout_duration, async {
        while let Some(msg) = metadata_rx.recv().await {
            match msg {
                MetadataMessage::Add(_, metadata) if metadata.pod_name == pod_name => {
                    return Ok(metadata);
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
                    containers.insert(id, metadata);
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
    let plugin = MetadataPlugin::new(tx);

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
