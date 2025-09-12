mod pid_source;

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use log::{debug, error, info, warn};
use tokio::sync::mpsc;
use ttrpc::r#async::TtrpcContext;

use nri::api::{
    ConfigureRequest, ConfigureResponse, CreateContainerRequest, CreateContainerResponse, Empty,
    Event, StateChangeEvent, StopContainerRequest, StopContainerResponse, SynchronizeRequest,
    SynchronizeResponse, UpdateContainerRequest, UpdateContainerResponse, UpdatePodSandboxRequest,
    UpdatePodSandboxResponse,
};
use nri::api_ttrpc::Plugin;
use nri::events_mask::EventMask;

use resctrl::{Config as ResctrlConfig, FsProvider, RealFs, Resctrl};

use crate::pid_source::{CgroupPidSource, RealCgroupPidSource};

/// Resctrl group state for a pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResctrlGroupState {
    /// Group exists at the given path
    Exists(String),
    /// Group could not be created (e.g., RMID exhaustion)
    Failed,
}

/// Event payload for an added/updated pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodResctrlAddOrUpdate {
    pub pod_uid: String,
    pub group_state: ResctrlGroupState,
    /// Number of containers known for the pod
    pub total_containers: usize,
    /// Number of containers reconciled successfully
    pub reconciled_containers: usize,
}

/// Event payload for a removed/disassociated pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodResctrlRemoved {
    pub pod_uid: String,
}

/// Events emitted by the resctrl plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PodResctrlEvent {
    AddOrUpdate(PodResctrlAddOrUpdate),
    Removed(PodResctrlRemoved),
}

/// Configuration for the resctrl NRI plugin.
#[derive(Clone, Debug)]
pub struct ResctrlPluginConfig {
    /// Prefix used for resctrl group naming (e.g., "pod_")
    pub group_prefix: String,
    /// Cleanup stale groups with the given prefix on start
    pub cleanup_on_start: bool,
    /// Max reconciliation passes when assigning tasks per pod
    pub max_reconcile_passes: usize,
    /// Max concurrent pod operations
    pub concurrency_limit: usize,
    /// Whether `resctrl` should auto-mount when not present
    pub auto_mount: bool,
}

impl Default for ResctrlPluginConfig {
    fn default() -> Self {
        Self {
            group_prefix: "pod_".to_string(),
            cleanup_on_start: true,
            max_reconcile_passes: 10,
            concurrency_limit: 1,
            auto_mount: false,
        }
    }
}

#[derive(Clone)]
struct PodState {
    group_state: ResctrlGroupState,
    total_containers: usize,
    reconciled_containers: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum ContainerSyncState {
    #[default]
    NoPod,
    Partial,
    Reconciled,
}

#[derive(Default)]
struct ContainerState {
    pod_uid: String,
    state: ContainerSyncState,
}

#[derive(Default)]
struct InnerState {
    pods: HashMap<String, PodState>,             // keyed by pod UID
    containers: HashMap<String, ContainerState>, // keyed by container ID
}

/// Resctrl NRI plugin. Generic over `FsProvider` for testability.
pub struct ResctrlPlugin<P: FsProvider = RealFs> {
    #[allow(dead_code)]
    cfg: ResctrlPluginConfig,
    #[allow(dead_code)]
    resctrl: Resctrl<P>,
    state: Mutex<InnerState>,
    tx: mpsc::Sender<PodResctrlEvent>,
    dropped_events: Arc<AtomicUsize>,
    pid_source: Arc<dyn CgroupPidSource>,
}

impl ResctrlPlugin<RealFs> {
    /// Create a new plugin with default real filesystem provider.
    /// The caller provides the event sender channel.
    pub fn new(cfg: ResctrlPluginConfig, tx: mpsc::Sender<PodResctrlEvent>) -> Self {
        let rc_cfg = ResctrlConfig {
            group_prefix: cfg.group_prefix.clone(),
            auto_mount: cfg.auto_mount,
            ..Default::default()
        };
        Self {
            cfg,
            resctrl: Resctrl::new(rc_cfg),
            state: Mutex::new(InnerState::default()),
            tx,
            dropped_events: Arc::new(AtomicUsize::new(0)),
            pid_source: Arc::new(RealCgroupPidSource::new()),
        }
    }
}

impl<P: FsProvider> ResctrlPlugin<P> {
    /// Create a new plugin with a custom resctrl handle (DI for tests).
    /// The caller provides the event sender channel.
    pub fn with_resctrl(
        cfg: ResctrlPluginConfig,
        resctrl: Resctrl<P>,
        tx: mpsc::Sender<PodResctrlEvent>,
    ) -> Self {
        Self {
            cfg,
            resctrl,
            state: Mutex::new(InnerState::default()),
            tx,
            dropped_events: Arc::new(AtomicUsize::new(0)),
            pid_source: Arc::new(RealCgroupPidSource::new()),
        }
    }

    pub fn with_pid_source(
        cfg: ResctrlPluginConfig,
        resctrl: Resctrl<P>,
        tx: mpsc::Sender<PodResctrlEvent>,
        pid_source: Arc<dyn CgroupPidSource>,
    ) -> Self {
        Self {
            cfg,
            resctrl,
            state: Mutex::new(InnerState::default()),
            tx,
            dropped_events: Arc::new(AtomicUsize::new(0)),
            pid_source,
        }
    }

    /// Number of events dropped due to a full channel.
    pub fn dropped_events(&self) -> usize {
        self.dropped_events.load(Ordering::Relaxed)
    }

    /// Emit an event to the collector, drop if channel is full.
    fn emit_event(&self, ev: PodResctrlEvent) {
        if let Err(e) = self.tx.try_send(ev) {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
            warn!("resctrl-plugin: failed to send event: {}", e);
        }
    }

    /// Emit pod state update event
    fn emit_pod_add_or_update(&self, pod_uid: &str, ps: &PodState) {
        let ev = PodResctrlEvent::AddOrUpdate(PodResctrlAddOrUpdate {
            pod_uid: pod_uid.to_string(),
            group_state: ps.group_state.clone(),
            total_containers: ps.total_containers,
            reconciled_containers: ps.reconciled_containers,
        });
        self.emit_event(ev);
    }

    // Create or fetch pod state and ensure group exists
    fn handle_new_pod(&self, pod: &nri::api::PodSandbox) {
        let pod_uid = &pod.uid;
        let mut st = self.state.lock().unwrap();

        // If pod doesn't exist yet, create it with appropriate group state
        if !st.pods.contains_key(pod_uid) {
            let group_state = match self.resctrl.create_group(pod_uid) {
                Ok(p) => ResctrlGroupState::Exists(p),
                Err(e) => {
                    warn!(
                        "resctrl-plugin: failed to create group for pod {}: {}",
                        pod_uid, e
                    );
                    ResctrlGroupState::Failed
                }
            };

            st.pods.insert(
                pod_uid.clone(),
                PodState {
                    group_state,
                    total_containers: 0,
                    reconciled_containers: 0,
                },
            );
        }

        let ps = st.pods.get(pod_uid).unwrap();
        self.emit_pod_add_or_update(pod_uid, ps);
        drop(st);
    }

    fn handle_new_container(&self, pod: &nri::api::PodSandbox, container: &nri::api::Container) {
        let pod_uid = pod.uid.clone();
        let container_id = container.id.clone();

        // Hold the lock to check for duplicates and pod presence, and to handle
        // simple state updates that don't involve external syscalls.
        let mut st = self.state.lock().unwrap();

        // First, error if the container is already known
        if st.containers.contains_key(&container_id) {
            error!(
                "resctrl-plugin: container {} already exists in state; ignoring duplicate",
                container_id
            );
            return;
        }

        if !st.pods.contains_key(&pod_uid) {
            // No pod yet: mark container as NoPod and return
            error!(
                "resctrl-plugin: container {} observed before pod {}. Marking NoPod.",
                container.id, pod_uid
            );
            st.containers.insert(
                container_id.clone(),
                ContainerState {
                    pod_uid: pod_uid.clone(),
                    state: ContainerSyncState::NoPod,
                },
            );
            return;
        }

        // Pod exists; fetch group path state
        let gp = st.pods.get(&pod_uid).and_then(|p| match &p.group_state {
            ResctrlGroupState::Exists(path) => Some(path.clone()),
            _ => None,
        });

        // If pod exists but has no group path (Failed), container is Partial
        if gp.is_none() {
            st.containers.insert(
                container_id.clone(),
                ContainerState {
                    pod_uid: pod_uid.clone(),
                    state: ContainerSyncState::Partial,
                },
            );
            let ps = st
                .pods
                .get_mut(&pod_uid)
                .expect("we already checked contains_key and we are holding the lock");
            ps.total_containers += 1;
            self.emit_pod_add_or_update(&pod_uid, ps);
            return;
        }

        // we have a valid group path; drop the lock while doing reconciliation
        drop(st);

        // The path is non-empty
        let group_path = gp.unwrap();

        // Create a closure that reads PIDs fresh each time
        let pid_source = self.pid_source.clone();
        let full = nri::compute_full_cgroup_path(container, Some(pod));
        let pid_resolver =
            move || -> Result<Vec<i32>, resctrl::Error> { pid_source.pids_for_path(&full) };

        // Reconcile this container's PIDs into the pod group
        let passes = self.cfg.max_reconcile_passes;
        let res = self
            .resctrl
            .reconcile_group(&group_path, pid_resolver, passes);

        let new_state = match res {
            Ok(ar) if ar.missing == 0 => ContainerSyncState::Reconciled,
            _ => ContainerSyncState::Partial,
        };

        // Update container state and pod counts, then emit update
        let mut st = self.state.lock().unwrap();
        st.containers.insert(
            container_id,
            ContainerState {
                pod_uid: pod_uid.clone(),
                state: new_state,
            },
        );
        if let Some(ps) = st.pods.get_mut(&pod_uid) {
            // Incremental count updates per state transition
            ps.total_containers += 1;
            if new_state == ContainerSyncState::Reconciled {
                ps.reconciled_containers += 1
            }
            self.emit_pod_add_or_update(&pod_uid, ps);
        }
    }
}

#[async_trait]
impl<P: FsProvider + Send + Sync + 'static> Plugin for ResctrlPlugin<P> {
    async fn configure(
        &self,
        _ctx: &TtrpcContext,
        req: ConfigureRequest,
    ) -> ttrpc::Result<ConfigureResponse> {
        info!(
            "Configured resctrl plugin for runtime: {} {}",
            req.runtime_name, req.runtime_version
        );

        // Subscribe to container and pod lifecycle events we handle.
        let mut events = EventMask::new();
        events.set(&[
            Event::CREATE_CONTAINER,
            Event::REMOVE_CONTAINER,
            Event::RUN_POD_SANDBOX,
            Event::REMOVE_POD_SANDBOX,
        ]);

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
            "Synchronizing resctrl plugin with {} pods and {} containers",
            req.pods.len(),
            req.containers.len()
        );

        // Ensure groups for all pods first
        for pod in &req.pods {
            self.handle_new_pod(pod);
        }

        // Then reconcile each container individually
        let pods_map: std::collections::HashMap<String, nri::api::PodSandbox> =
            req.pods.iter().map(|p| (p.id.clone(), p.clone())).collect();
        for c in &req.containers {
            if let Some(pod) = pods_map.get(&c.pod_sandbox_id) {
                self.handle_new_container(pod, c);
            }
        }

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
        debug!("resctrl-plugin: create_container: {}", req.container.id);
        if let (Some(pod), Some(container)) = (req.pod.as_ref(), req.container.as_ref()) {
            self.handle_new_container(pod, container);
        }
        Ok(CreateContainerResponse::default())
    }

    async fn update_container(
        &self,
        _ctx: &TtrpcContext,
        req: UpdateContainerRequest,
    ) -> ttrpc::Result<UpdateContainerResponse> {
        debug!("resctrl-plugin: update_container: {}", req.container.id);
        Ok(UpdateContainerResponse::default())
    }

    async fn stop_container(
        &self,
        _ctx: &TtrpcContext,
        req: StopContainerRequest,
    ) -> ttrpc::Result<StopContainerResponse> {
        debug!("resctrl-plugin: stop_container: {}", req.container.id);
        Ok(StopContainerResponse::default())
    }

    async fn update_pod_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: UpdatePodSandboxRequest,
    ) -> ttrpc::Result<UpdatePodSandboxResponse> {
        debug!("resctrl-plugin: update_pod_sandbox: {}", req.pod.uid);
        Ok(UpdatePodSandboxResponse::default())
    }

    async fn state_change(
        &self,
        _ctx: &TtrpcContext,
        req: StateChangeEvent,
    ) -> ttrpc::Result<Empty> {
        debug!("resctrl-plugin: state_change: event={:?}", req.event);
        match req.event.enum_value() {
            Ok(Event::RUN_POD_SANDBOX) => {
                if let Some(pod) = req.pod.as_ref() {
                    self.handle_new_pod(pod);
                }
            }
            Ok(Event::REMOVE_POD_SANDBOX) => {
                if let Some(pod) = req.pod.as_ref() {
                    let pod_uid = pod.uid.clone();
                    let mut st = self.state.lock().unwrap();

                    // Get group path before removing pod state
                    let group_path = st.pods.get(&pod_uid).and_then(|ps| match &ps.group_state {
                        ResctrlGroupState::Exists(path) => Some(path.clone()),
                        _ => None,
                    });

                    // Remove all containers for this pod
                    st.containers.retain(|_, c| c.pod_uid != pod_uid);
                    // Remove pod state
                    st.pods.remove(&pod_uid);
                    drop(st);

                    // Delete resctrl group if it exists
                    if let Some(gp) = group_path {
                        if let Err(e) = self.resctrl.delete_group(&gp) {
                            warn!("resctrl-plugin: failed to delete group {}: {}", gp, e);
                        }
                    }

                    // Emit removal event
                    self.emit_event(PodResctrlEvent::Removed(PodResctrlRemoved { pod_uid }));
                }
            }
            Ok(Event::REMOVE_CONTAINER) => {
                if let (Some(pod), Some(container)) = (req.pod.as_ref(), req.container.as_ref()) {
                    let pod_uid = pod.uid.clone();
                    let mut st = self.state.lock().unwrap();

                    // Adjust counts based on the removed container's previous state
                    let old_state = st.containers.remove(&container.id).map(|c| c.state);
                    if let Some(ps) = st.pods.get_mut(&pod_uid) {
                        if matches!(old_state, Some(s) if s != ContainerSyncState::NoPod) {
                            ps.total_containers = ps.total_containers.saturating_sub(1);
                        }
                        if matches!(old_state, Some(ContainerSyncState::Reconciled)) {
                            ps.reconciled_containers = ps.reconciled_containers.saturating_sub(1);
                        }
                        let snapshot = ps.clone();
                        drop(st);
                        self.emit_pod_add_or_update(&pod_uid, &snapshot);
                    } else {
                        drop(st);
                    }
                }
            }
            _ => {}
        }
        Ok(Empty::default())
    }

    async fn shutdown(&self, _ctx: &TtrpcContext, _req: Empty) -> ttrpc::Result<Empty> {
        info!("Shutting down resctrl plugin");
        Ok(Empty::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::SpecialFields;

    #[derive(Clone, Default)]
    struct TestFsState {
        files: std::collections::HashMap<std::path::PathBuf, String>,
        dirs: std::collections::HashSet<std::path::PathBuf>,
        no_perm_files: std::collections::HashSet<std::path::PathBuf>,
    }

    #[derive(Clone, Default)]
    struct TestFs {
        state: std::sync::Arc<std::sync::Mutex<TestFsState>>,
    }

    impl TestFs {
        #[allow(dead_code)]
        fn add_file(&self, p: &std::path::Path, content: &str) {
            let mut st = self.state.lock().unwrap();
            st.files.insert(p.to_path_buf(), content.to_string());
        }
        fn add_dir(&self, p: &std::path::Path) {
            let mut st = self.state.lock().unwrap();
            st.dirs.insert(p.to_path_buf());
        }
    }

    impl FsProvider for TestFs {
        fn exists(&self, p: &std::path::Path) -> bool {
            let st = self.state.lock().unwrap();
            st.dirs.contains(p) || st.files.contains_key(p)
        }
        fn create_dir(&self, p: &std::path::Path) -> std::io::Result<()> {
            let mut st = self.state.lock().unwrap();
            if st.dirs.contains(p) {
                return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
            }
            st.dirs.insert(p.to_path_buf());
            // Simulate kernel-provided tasks file for resctrl groups
            if let Some(name) = p.file_name() {
                if name.to_string_lossy().starts_with("pod_") {
                    let tasks = p.join("tasks");
                    st.files.entry(tasks).or_default();
                }
            }
            Ok(())
        }
        fn remove_dir(&self, p: &std::path::Path) -> std::io::Result<()> {
            let mut st = self.state.lock().unwrap();
            if !st.dirs.remove(p) {
                return Err(std::io::Error::from_raw_os_error(libc::ENOENT));
            }
            Ok(())
        }
        fn write_str(&self, p: &std::path::Path, data: &str) -> std::io::Result<()> {
            let mut st = self.state.lock().unwrap();
            if st.no_perm_files.contains(p) {
                return Err(std::io::Error::from_raw_os_error(libc::EACCES));
            }
            let e = st.files.entry(p.to_path_buf()).or_default();
            if !e.ends_with('\n') && !e.is_empty() {
                e.push('\n');
            }
            e.push_str(data);
            e.push('\n');
            Ok(())
        }
        fn read_to_string(&self, p: &std::path::Path) -> std::io::Result<String> {
            let st = self.state.lock().unwrap();
            match st.files.get(p) {
                Some(s) => Ok(s.clone()),
                None => Err(std::io::Error::from_raw_os_error(libc::ENOENT)),
            }
        }
        fn check_can_open_for_write(&self, p: &std::path::Path) -> std::io::Result<()> {
            let st = self.state.lock().unwrap();
            if st.files.contains_key(p) {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(libc::ENOENT))
            }
        }
        fn mount_resctrl(&self, _target: &std::path::Path) -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
        }
    }

    #[test]
    fn test_default_config() {
        let cfg = ResctrlPluginConfig::default();
        assert_eq!(cfg.group_prefix, "pod_");
        assert!(cfg.cleanup_on_start);
        assert_eq!(cfg.max_reconcile_passes, 10);
        assert_eq!(cfg.concurrency_limit, 1);
        assert!(!cfg.auto_mount);
    }

    #[tokio::test]
    async fn test_configure_event_mask() {
        let (tx, _rx) = mpsc::channel::<PodResctrlEvent>(8);
        let plugin = ResctrlPlugin::new(ResctrlPluginConfig::default(), tx);

        let ctx = TtrpcContext {
            mh: ttrpc::MessageHeader::default(),
            metadata: std::collections::HashMap::<String, Vec<String>>::default(),
            timeout_nano: 5_000,
        };
        let req = ConfigureRequest {
            config: String::new(),
            runtime_name: "test-runtime".into(),
            runtime_version: "1.0".into(),
            registration_timeout: 1000,
            request_timeout: 1000,
            special_fields: SpecialFields::default(),
        };

        let resp = plugin.configure(&ctx, req).await.unwrap();
        let events = EventMask::from_raw(resp.events);

        // Must include minimal container/pod events we need
        assert!(events.is_set(Event::CREATE_CONTAINER));
        assert!(events.is_set(Event::RUN_POD_SANDBOX));
        assert!(events.is_set(Event::REMOVE_POD_SANDBOX));
        assert!(events.is_set(Event::REMOVE_CONTAINER));
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn test_reconcile_emits_counts() {
        // This test requires Linux-specific functionality
        let fs = TestFs::default();
        // Ensure resctrl root exists
        fs.add_dir(std::path::Path::new("/sys"));
        fs.add_dir(std::path::Path::new("/sys/fs"));
        fs.add_dir(std::path::Path::new("/sys/fs/resctrl"));

        // Create a fake cgroup with two PIDs
        let cg = std::path::PathBuf::from("/cg/podX/containerA");
        fs.add_dir(cg.parent().unwrap());
        fs.add_dir(&cg);
        fs.add_file(&cg.join("cgroup.procs"), "1\n2\n");

        let rc = Resctrl::with_provider(
            fs.clone(),
            resctrl::Config {
                auto_mount: false,
                ..Default::default()
            },
        );

        // Use mock PID source from the module
        use crate::pid_source::test_support::MockCgroupPidSource;
        let mut mock_pid_src = MockCgroupPidSource::new();
        let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(8);

        // Build synchronize request with one pod and one container
        let pod = nri::api::PodSandbox {
            id: "pod-sb-1".into(),
            uid: "u123".into(),
            ..Default::default()
        };

        let linux = nri::api::LinuxContainer {
            cgroups_path: cg.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let container = nri::api::Container {
            id: "ctr1".into(),
            pod_sandbox_id: pod.id.clone(),
            linux: protobuf::MessageField::some(linux),
            ..Default::default()
        };

        // Now that we have both, register the full cgroup path with mock pid source
        let full_cg = nri::compute_full_cgroup_path(&container, Some(&pod));
        mock_pid_src.set_pids(full_cg, vec![1, 2]);

        // Create plugin with the configured mock pid source
        let plugin = ResctrlPlugin::with_pid_source(
            ResctrlPluginConfig::default(),
            rc,
            tx,
            Arc::new(mock_pid_src),
        );

        let req = SynchronizeRequest {
            pods: vec![pod.clone()],
            containers: vec![container.clone()],
            more: false,
            special_fields: SpecialFields::default(),
        };

        let ctx = TtrpcContext {
            mh: ttrpc::MessageHeader::default(),
            metadata: std::collections::HashMap::new(),
            timeout_nano: 5_000,
        };
        let _ = plugin.synchronize(&ctx, req).await.unwrap();

        // Expect pod creation event and container reconciliation event
        use tokio::time::{timeout, Duration};
        timeout(Duration::from_millis(100), async {
            let mut events_received = 0;
            while let Some(ev) = rx.recv().await {
                events_received += 1;
                match ev {
                    PodResctrlEvent::AddOrUpdate(a) => {
                        assert_eq!(a.pod_uid, "u123");
                        assert!(matches!(a.group_state, ResctrlGroupState::Exists(_)));
                        // After synchronize, we should have 1 container reconciled
                        if events_received > 1 {
                            assert_eq!(a.total_containers, 1);
                            assert_eq!(a.reconciled_containers, 1);
                            break;
                        }
                    }
                    _ => panic!("unexpected event type"),
                }
            }
        })
        .await
        .expect("Should receive events within timeout");

        // Test container removal
        let state_req = StateChangeEvent {
            event: Event::REMOVE_CONTAINER.into(),
            pod: protobuf::MessageField::some(pod.clone()),
            container: protobuf::MessageField::some(container.clone()),
            special_fields: SpecialFields::default(),
        };
        let _ = plugin.state_change(&ctx, state_req).await.unwrap();

        // Should get an update event with reduced counts
        timeout(Duration::from_millis(100), async {
            if let Some(ev) = rx.recv().await {
                match ev {
                    PodResctrlEvent::AddOrUpdate(a) => {
                        assert_eq!(a.total_containers, 0);
                        assert_eq!(a.reconciled_containers, 0);
                    }
                    _ => panic!("unexpected event type"),
                }
            }
        })
        .await
        .expect("Should receive event within timeout");
    }

    #[tokio::test]
    async fn test_run_pod_sandbox_creates_group_and_emits_event() {
        let fs = TestFs::default();
        // Ensure resctrl root exists
        fs.add_dir(std::path::Path::new("/sys"));
        fs.add_dir(std::path::Path::new("/sys/fs"));
        fs.add_dir(std::path::Path::new("/sys/fs/resctrl"));

        let rc = Resctrl::with_provider(
            fs.clone(),
            resctrl::Config {
                auto_mount: false,
                ..Default::default()
            },
        );

        use crate::pid_source::test_support::MockCgroupPidSource;
        let mock_pid_src = MockCgroupPidSource::new();
        let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(8);
        let plugin = ResctrlPlugin::with_pid_source(
            ResctrlPluginConfig::default(),
            rc,
            tx,
            Arc::new(mock_pid_src),
        );

        // Define a pod sandbox
        let pod = nri::api::PodSandbox {
            id: "pod-sb-run-test".into(),
            uid: "u789".into(),
            ..Default::default()
        };

        // Send RUN_POD_SANDBOX via state_change
        let ctx = TtrpcContext {
            mh: ttrpc::MessageHeader::default(),
            metadata: std::collections::HashMap::new(),
            timeout_nano: 5_000,
        };
        let state_req = StateChangeEvent {
            event: Event::RUN_POD_SANDBOX.into(),
            pod: protobuf::MessageField::some(pod.clone()),
            container: protobuf::MessageField::none(),
            special_fields: SpecialFields::default(),
        };
        let _ = plugin.state_change(&ctx, state_req).await.unwrap();

        // Should receive an AddOrUpdate event with existing group state
        use tokio::time::{timeout, Duration};
        timeout(Duration::from_millis(100), async {
            if let Some(ev) = rx.recv().await {
                match ev {
                    PodResctrlEvent::AddOrUpdate(a) => {
                        assert_eq!(a.pod_uid, "u789");
                        assert!(matches!(a.group_state, ResctrlGroupState::Exists(_)));
                    }
                    _ => panic!("Expected AddOrUpdate event, got: {:?}", ev),
                }
            } else {
                panic!("Expected AddOrUpdate event, got nothing");
            }
        })
        .await
        .expect("Should receive event within timeout");

        // Verify the directory for the resctrl group was created
        assert!(fs.exists(std::path::Path::new("/sys/fs/resctrl/pod_u789")));

        // Now remove the pod and verify removal event + directory deletion
        let state_req = StateChangeEvent {
            event: Event::REMOVE_POD_SANDBOX.into(),
            pod: protobuf::MessageField::some(pod.clone()),
            container: protobuf::MessageField::none(),
            special_fields: SpecialFields::default(),
        };
        let _ = plugin.state_change(&ctx, state_req).await.unwrap();

        timeout(Duration::from_millis(100), async {
            if let Some(ev) = rx.recv().await {
                match ev {
                    PodResctrlEvent::Removed(r) => {
                        assert_eq!(r.pod_uid, "u789");
                    }
                    _ => panic!("Expected Removed event, got: {:?}", ev),
                }
            } else {
                panic!("Expected Removed event, got nothing");
            }
        })
        .await
        .expect("Should receive removal event within timeout");

        assert!(!fs.exists(std::path::Path::new("/sys/fs/resctrl/pod_u789")));
    }
}
