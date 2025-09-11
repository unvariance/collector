use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use log::{debug, info, warn};
use tokio::sync::mpsc;
use ttrpc::r#async::TtrpcContext;

use crate::api::{
    self, ConfigureRequest, ConfigureResponse, CreateContainerRequest, CreateContainerResponse,
    Empty, Event, StopContainerRequest, StopContainerResponse, SynchronizeRequest,
    SynchronizeResponse, UpdateContainerRequest, UpdateContainerResponse, UpdatePodSandboxRequest,
    UpdatePodSandboxResponse,
};
use crate::api_ttrpc::Plugin;
use crate::events_mask::EventMask;

/// Container metadata collected from NRI.
#[derive(Debug, Clone)]
pub struct ContainerMetadata {
    /// Container ID
    pub container_id: String,
    /// Pod name
    pub pod_name: String,
    /// Pod namespace
    pub pod_namespace: String,
    /// Pod UID
    pub pod_uid: String,
    /// Container name
    pub container_name: String,
    /// Cgroup path
    pub cgroup_path: String,
    /// Container process PID
    pub pid: Option<u32>,
    /// Container labels
    pub labels: HashMap<String, String>,
    /// Container annotations
    pub annotations: HashMap<String, String>,
}

/// Message types sent through the metadata channel.
#[derive(Debug)]
pub enum MetadataMessage {
    /// Add or update metadata for a container
    Add(String, Box<ContainerMetadata>),
    /// Remove metadata for a container
    Remove(String),
}

/// Metadata plugin for NRI.
///
/// This plugin collects container metadata from the NRI runtime and sends it through
/// a channel. It handles container lifecycle events and synchronization events.
#[derive(Clone)]
pub struct MetadataPlugin {
    /// Channel for sending metadata messages
    tx: mpsc::Sender<MetadataMessage>,
    /// Counter for dropped messages
    dropped_messages: Arc<AtomicUsize>,
}

impl MetadataPlugin {
    /// Create a new metadata plugin with the given sender.
    pub fn new(tx: mpsc::Sender<MetadataMessage>) -> Self {
        Self {
            tx,
            dropped_messages: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get the number of dropped messages.
    pub fn dropped_messages(&self) -> usize {
        self.dropped_messages.load(Ordering::Relaxed)
    }

    /// Extract container metadata from a container and pod.
    fn extract_metadata(
        &self,
        container: &api::Container,
        pod: Option<&api::PodSandbox>,
    ) -> ContainerMetadata {
        let cgroup_path = crate::compute_full_cgroup_path(container, pod);

        let (pod_name, pod_namespace, pod_uid) = if let Some(pod) = pod {
            (pod.name.clone(), pod.namespace.clone(), pod.uid.clone())
        } else {
            (String::new(), String::new(), String::new())
        };

        ContainerMetadata {
            container_id: container.id.clone(),
            pod_name,
            pod_namespace,
            pod_uid,
            container_name: container.name.clone(),
            cgroup_path,
            pid: if container.pid > 0 {
                Some(container.pid)
            } else {
                None
            },
            labels: container.labels.clone(),
            annotations: container.annotations.clone(),
        }
    }

    /// Send a metadata message through the channel.
    fn send_message(&self, message: MetadataMessage) {
        // Use try_send to avoid blocking the runtime
        if let Err(e) = self.tx.try_send(message) {
            self.dropped_messages.fetch_add(1, Ordering::Relaxed);
            warn!("Failed to send metadata message: {}", e);
        }
    }

    /// Initial synchronization handler for containers: send metadata messages.
    fn process_containers(&self, containers: &[api::Container], pods: &[api::PodSandbox]) {
        let pods_map: HashMap<String, &api::PodSandbox> =
            pods.iter().map(|pod| (pod.id.clone(), pod)).collect();

        for container in containers {
            let pod = pods_map.get(&container.pod_sandbox_id).copied();
            let metadata = self.extract_metadata(container, pod);

            debug!("Adding container metadata: {:?}", metadata);
            self.send_message(MetadataMessage::Add(container.id.clone(), Box::new(metadata)));
        }
    }
}

#[async_trait::async_trait]
impl Plugin for MetadataPlugin {
    async fn configure(
        &self,
        _ctx: &TtrpcContext,
        req: ConfigureRequest,
    ) -> ttrpc::Result<ConfigureResponse> {
        info!(
            "Configured metadata plugin for runtime: {} {}",
            req.runtime_name, req.runtime_version
        );

        // Subscribe to container lifecycle events
        let mut events = EventMask::new();
        events.set(&[Event::CREATE_CONTAINER, Event::STOP_CONTAINER]);

        Ok(ConfigureResponse {
            events: events.raw_value(),
            special_fields: protobuf::SpecialFields::default(),
        })
    }

    async fn synchronize(
        &self,
        _ctx: &TtrpcContext,
        req: SynchronizeRequest,
    ) -> ttrpc::Result<SynchronizeResponse> {
        info!(
            "Synchronizing metadata plugin with {} pods and {} containers",
            req.pods.len(),
            req.containers.len()
        );

        // Process existing containers
        self.process_containers(&req.containers, &req.pods);

        // We don't request any container updates
        Ok(SynchronizeResponse {
            update: vec![],
            more: req.more,
            special_fields: protobuf::SpecialFields::default(),
        })
    }

    async fn create_container(
        &self,
        _ctx: &TtrpcContext,
        req: CreateContainerRequest,
    ) -> ttrpc::Result<CreateContainerResponse> {
        let container = &req.container;

        // Convert MessageField<PodSandbox> to &PodSandbox for extract_metadata
        let pod = req.pod.as_ref();

        debug!("Container created: {}", container.id);
        let metadata = self.extract_metadata(container, pod);
        self.send_message(MetadataMessage::Add(container.id.clone(), Box::new(metadata)));

        // We don't request any container adjustments
        Ok(CreateContainerResponse::default())
    }

    async fn update_container(
        &self,
        _ctx: &TtrpcContext,
        req: UpdateContainerRequest,
    ) -> ttrpc::Result<UpdateContainerResponse> {
        let container = &req.container;

        // Convert MessageField<PodSandbox> to &PodSandbox for extract_metadata
        let pod = req.pod.as_ref();

        debug!("Container updated: {}", container.id);
        let metadata = self.extract_metadata(container, pod);
        self.send_message(MetadataMessage::Add(container.id.clone(), Box::new(metadata)));

        // We don't request any container updates
        Ok(UpdateContainerResponse::default())
    }

    async fn stop_container(
        &self,
        _ctx: &TtrpcContext,
        req: StopContainerRequest,
    ) -> ttrpc::Result<StopContainerResponse> {
        let container_id = &req.container.id;

        debug!("Container stopped/removed: {}", container_id);
        self.send_message(MetadataMessage::Remove(container_id.clone()));

        // We don't request any container updates
        Ok(StopContainerResponse::default())
    }

    async fn update_pod_sandbox(
        &self,
        _ctx: &TtrpcContext,
        _req: UpdatePodSandboxRequest,
    ) -> ttrpc::Result<UpdatePodSandboxResponse> {
        // We don't care about pod sandbox updates
        debug!("Pod sandbox updated: {:?}", _req);
        Ok(UpdatePodSandboxResponse::default())
    }

    async fn shutdown(&self, _ctx: &TtrpcContext, _req: Empty) -> ttrpc::Result<Empty> {
        info!("Shutting down metadata plugin");
        Ok(Empty::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::{EnumOrUnknown, MessageField, SpecialFields};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_metadata_extraction() {
        // Create a channel for testing
        let (tx, mut rx) = mpsc::channel(100);
        let plugin = MetadataPlugin::new(tx);

        // Create a test container with colon-delimited cgroups_path
        let container = api::Container {
            id: "container1".to_string(),
            pod_sandbox_id: "pod1".to_string(),
            name: "test-container".to_string(),
            pid: 1234,
            linux: MessageField::some(api::LinuxContainer {
                cgroups_path:
                    "kubelet-kubepods-besteffort-pod123.slice:cri-containerd:abc123def456"
                        .to_string(),
                namespaces: vec![],
                devices: vec![],
                resources: MessageField::none(),
                oom_score_adj: MessageField::none(),
                special_fields: SpecialFields::default(),
            }),
            ..Default::default()
        };

        for with_prefix in [false, true] {
            // Create a test pod with linux cgroup_parent (optionally with /sys/fs/cgroup prefix)
            let parent_no_prefix = "/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-besteffort.slice/kubelet-kubepods-besteffort-pod123.slice";
            let parent = if with_prefix {
                format!("/sys/fs/cgroup{}", parent_no_prefix)
            } else {
                parent_no_prefix.to_string()
            };

            let pod = api::PodSandbox {
                id: "pod1".to_string(),
                name: "test-pod".to_string(),
                namespace: "test-namespace".to_string(),
                uid: "pod-uid-123".to_string(),
                labels: Default::default(),
                annotations: Default::default(),
                runtime_handler: "".to_string(),
                linux: MessageField::some(api::LinuxPodSandbox {
                    cgroup_parent: parent,
                    cgroups_path: String::new(),
                    pod_overhead: MessageField::none(),
                    pod_resources: MessageField::none(),
                    resources: MessageField::none(),
                    namespaces: vec![],
                    special_fields: SpecialFields::default(),
                }),
                pid: 0,
                ips: vec![],
                special_fields: SpecialFields::default(),
            };

            // Extract metadata
            let metadata = plugin.extract_metadata(&container, Some(&pod));

            // Verify metadata (prefix should not be duplicated and overall path should be the same)
            assert_eq!(metadata.container_id, "container1");
            assert_eq!(metadata.pod_name, "test-pod");
            assert_eq!(metadata.pod_namespace, "test-namespace");
            assert_eq!(metadata.pod_uid, "pod-uid-123");
            assert_eq!(metadata.container_name, "test-container");
            assert_eq!(metadata.cgroup_path, "/sys/fs/cgroup/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-besteffort.slice/kubelet-kubepods-besteffort-pod123.slice/cri-containerd-abc123def456.scope");
            assert_eq!(metadata.pid, Some(1234));

            // Test sending a message per iteration
            plugin.send_message(MetadataMessage::Add(container.id.clone(), Box::new(metadata)));

            // Verify message was received
            let message = rx.recv().await.unwrap();
            match message {
                MetadataMessage::Add(id, metadata) => {
                    assert_eq!(id, "container1");
                    assert_eq!(metadata.container_id, "container1");
                    assert_eq!(metadata.pod_name, "test-pod");
                }
                _ => panic!("Expected Add message"),
            }
        }
    }

    #[tokio::test]
    async fn test_metadata_extraction_without_pod() {
        // Create a channel for testing
        let (tx, _rx) = mpsc::channel(100);
        let plugin = MetadataPlugin::new(tx);

        // Create a test container without pod information
        let container = api::Container {
            id: "container1".to_string(),
            pod_sandbox_id: "pod1".to_string(),
            name: "test-container".to_string(),
            pid: 1234,
            linux: MessageField::some(api::LinuxContainer {
                cgroups_path: "system.slice/docker-abc123.scope".to_string(),
                namespaces: vec![],
                devices: vec![],
                resources: MessageField::none(),
                oom_score_adj: MessageField::none(),
                special_fields: SpecialFields::default(),
            }),
            ..Default::default()
        };

        // Extract metadata without pod
        let metadata = plugin.extract_metadata(&container, None);

        // Verify metadata - should fall back to prefixing the container path
        assert_eq!(metadata.container_id, "container1");
        assert_eq!(metadata.pod_name, "");
        assert_eq!(metadata.pod_namespace, "");
        assert_eq!(metadata.pod_uid, "");
        assert_eq!(metadata.container_name, "test-container");
        assert_eq!(
            metadata.cgroup_path,
            "/sys/fs/cgroup/system.slice/docker-abc123.scope"
        );
        assert_eq!(metadata.pid, Some(1234));
    }

    #[tokio::test]
    async fn test_metadata_plugin_lifecycle() {
        // Create a channel for testing with sufficient capacity
        let (tx, mut rx) = mpsc::channel(100);
        let plugin = MetadataPlugin::new(tx);

        // Helper function to create test containers
        fn create_test_container(
            id: &str,
            pod_id: &str,
            name: &str,
            container_hash: &str,
        ) -> api::Container {
            api::Container {
                id: id.to_string(),
                pod_sandbox_id: pod_id.to_string(),
                name: name.to_string(),
                state: EnumOrUnknown::from(api::ContainerState::CONTAINER_RUNNING),
                labels: Default::default(),
                annotations: Default::default(),
                linux: MessageField::some(api::LinuxContainer {
                    cgroups_path: format!(
                        "kubelet-kubepods-besteffort-{}.slice:cri-containerd:{}",
                        pod_id, container_hash
                    ),
                    namespaces: vec![],
                    devices: vec![],
                    resources: MessageField::none(),
                    oom_score_adj: MessageField::none(),
                    special_fields: SpecialFields::default(),
                }),
                pid: 1000,
                args: vec![],
                env: vec![],
                mounts: vec![],
                hooks: MessageField::none(),
                rlimits: vec![],
                created_at: 0,
                started_at: 0,
                finished_at: 0,
                exit_code: 0,
                status_reason: "".to_string(),
                status_message: "".to_string(),
                special_fields: SpecialFields::default(),
            }
        }

        // Helper function to create test pods
        fn create_test_pod(id: &str, name: &str, namespace: &str) -> api::PodSandbox {
            api::PodSandbox {
                id: id.to_string(),
                name: name.to_string(),
                namespace: namespace.to_string(),
                uid: format!("{}-uid", id),
                labels: Default::default(),
                annotations: Default::default(),
                runtime_handler: "".to_string(),
                linux: MessageField::some(api::LinuxPodSandbox {
                    cgroup_parent: format!("/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-besteffort.slice/kubelet-kubepods-besteffort-{}.slice", id),
                    cgroups_path: String::new(),
                    pod_overhead: MessageField::none(),
                    pod_resources: MessageField::none(),
                    resources: MessageField::none(),
                    namespaces: vec![],
                    special_fields: SpecialFields::default(),
                }),
                pid: 0,
                ips: vec![],
                special_fields: SpecialFields::default(),
            }
        }

        // Helper function to verify container metadata
        fn verify_container_metadata(
            metadata: &ContainerMetadata,
            expected_container_id: &str,
            expected_pod_name: &str,
            expected_container_name: &str,
            expected_pod_id: &str,
            expected_container_hash: &str,
        ) {
            assert_eq!(metadata.container_id, expected_container_id);
            assert_eq!(metadata.pod_name, expected_pod_name);
            assert_eq!(metadata.container_name, expected_container_name);
            let expected_cgroup = format!("/sys/fs/cgroup/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-besteffort.slice/kubelet-kubepods-besteffort-{}.slice/cri-containerd-{}.scope", expected_pod_id, expected_container_hash);
            assert_eq!(metadata.cgroup_path, expected_cgroup);
        }

        let context = TtrpcContext {
            mh: ttrpc::MessageHeader::default(),
            metadata: HashMap::<String, Vec<String>>::default(),
            timeout_nano: 5000,
        };

        // Test 1: Configure the plugin
        let configure_req = ConfigureRequest {
            config: "test-config".to_string(),
            runtime_name: "test-runtime".to_string(),
            runtime_version: "1.0.0".to_string(),
            registration_timeout: 5000,
            request_timeout: 5000,
            special_fields: SpecialFields::default(),
        };

        let configure_resp = plugin.configure(&context, configure_req).await.unwrap();

        // Verify plugin subscribed to container events using EventMask
        let events = EventMask::from_raw(configure_resp.events);
        assert_ne!(events.raw_value(), 0, "Plugin should subscribe to events");
        assert!(
            events.is_set(Event::CREATE_CONTAINER),
            "Plugin should subscribe to container creation events"
        );
        assert!(
            events.is_set(Event::STOP_CONTAINER),
            "Plugin should subscribe to container stop events"
        );

        // Test 2: Synchronize with existing containers
        let test_pod = create_test_pod("pod1", "test-pod", "test-namespace");
        let test_container =
            create_test_container("container1", "pod1", "test-container", "abc123def456");

        let sync_req = SynchronizeRequest {
            pods: vec![test_pod],
            containers: vec![test_container],
            more: false,
            special_fields: SpecialFields::default(),
        };

        let _ = plugin.synchronize(&context, sync_req).await.unwrap();

        // Verify metadata message for synchronized container
        let message = rx.recv().await.unwrap();
        match message {
            MetadataMessage::Add(id, metadata) => {
                assert_eq!(id, "container1");
                verify_container_metadata(
                    &metadata,
                    "container1",
                    "test-pod",
                    "test-container",
                    "pod1",
                    "abc123def456",
                );
            }
            _ => panic!("Expected Add message for container1"),
        }

        // Test 3: Create a new container
        let new_pod = create_test_pod("pod2", "new-pod", "test-namespace");
        let new_container =
            create_test_container("container2", "pod2", "new-container", "xyz789ghi012");

        let create_req = CreateContainerRequest {
            pod: MessageField::some(new_pod),
            container: MessageField::some(new_container),
            special_fields: SpecialFields::default(),
        };

        let _ = plugin.create_container(&context, create_req).await.unwrap();

        // Verify metadata message for created container
        let message = rx.recv().await.unwrap();
        match message {
            MetadataMessage::Add(id, metadata) => {
                assert_eq!(id, "container2");
                verify_container_metadata(
                    &metadata,
                    "container2",
                    "new-pod",
                    "new-container",
                    "pod2",
                    "xyz789ghi012",
                );
            }
            _ => panic!("Expected Add message for container2"),
        }

        // Test 4: Update a container
        let updated_pod = create_test_pod("pod2", "new-pod", "test-namespace");
        let mut updated_container =
            create_test_container("container2", "pod2", "new-container", "xyz789ghi012");
        updated_container.pid = 2000;

        let update_req = UpdateContainerRequest {
            pod: MessageField::some(updated_pod),
            container: MessageField::some(updated_container),
            linux_resources: MessageField::none(),
            special_fields: SpecialFields::default(),
        };

        let _ = plugin.update_container(&context, update_req).await.unwrap();

        // Verify metadata message for updated container
        let message = rx.recv().await.unwrap();
        match message {
            MetadataMessage::Add(id, metadata) => {
                assert_eq!(id, "container2");
                verify_container_metadata(
                    &metadata,
                    "container2",
                    "new-pod",
                    "new-container",
                    "pod2",
                    "xyz789ghi012",
                );
                assert_eq!(metadata.pid, Some(2000));
            }
            _ => panic!("Expected Add message for updated container2"),
        }

        // Test 5: Stop a container
        let stop_pod = create_test_pod("pod1", "test-pod", "test-namespace");
        let stop_container =
            create_test_container("container1", "pod1", "test-container", "abc123def456");

        let stop_req = StopContainerRequest {
            pod: MessageField::some(stop_pod),
            container: MessageField::some(stop_container),
            special_fields: SpecialFields::default(),
        };

        let _ = plugin.stop_container(&context, stop_req).await.unwrap();

        // Verify metadata message for stopped container
        let message = rx.recv().await.unwrap();
        match message {
            MetadataMessage::Remove(id) => {
                assert_eq!(id, "container1");
            }
            _ => panic!("Expected Remove message for container1"),
        }
    }
}
