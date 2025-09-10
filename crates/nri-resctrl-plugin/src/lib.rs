use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use log::{debug, info, warn};
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

/// Source of PIDs for a container based on cgroup metadata.
pub trait CgroupPidSource: Send + Sync {
    fn pids_for_container(&self, c: &nri::api::Container) -> Vec<i32>;
}

pub struct RealCgroupPidSource;

impl RealCgroupPidSource {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(target_os = "linux")]
impl CgroupPidSource for RealCgroupPidSource {
    fn pids_for_container(&self, c: &nri::api::Container) -> Vec<i32> {
        // Use cgroups-rs if available by path; fallback to reading files via std fs
        let cg_path = c
            .linux
            .as_ref()
            .map(|l| l.cgroups_path.clone())
            .unwrap_or_default();
        if cg_path.is_empty() {
            return vec![];
        }
        // Try reading cgroup.procs directly
        let procs_path = std::path::PathBuf::from(&cg_path).join("cgroup.procs");
        let tasks_path = std::path::PathBuf::from(&cg_path).join("tasks");
        let s = std::fs::read_to_string(&procs_path)
            .or_else(|_| std::fs::read_to_string(&tasks_path));
        match s {
            Ok(content) => content
                .lines()
                .filter_map(|l| l.trim().parse::<i32>().ok())
                .collect(),
            Err(_) => vec![],
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl CgroupPidSource for RealCgroupPidSource {
    fn pids_for_container(&self, _c: &nri::api::Container) -> Vec<i32> {
        vec![]
    }
}

/// Assignment state for associating pods to resctrl groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssignmentState {
    /// All known tasks assigned
    Success,
    /// Some tasks could not be assigned (race/inflight forks)
    Partial,
    /// Group could not be created (e.g., RMID exhaustion)
    Failure,
}

/// Event payload for an added/associated pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodResctrlAdded {
    pub pod_uid: String,
    /// None when Failure
    pub group_path: Option<String>,
    pub state: AssignmentState,
    /// Number of containers known for the pod
    pub total_containers: usize,
    /// Number of containers reconciled successfully (Success)
    pub reconciled_containers: usize,
}

/// Event payload for a removed/disassociated pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodResctrlRemoved {
    pub pod_uid: String,
    pub group_path: Option<String>,
}

/// Events emitted by the resctrl plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PodResctrlEvent {
    Added(PodResctrlAdded),
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

#[derive(Default)]
#[allow(dead_code)]
struct PodState {
    group_path: Option<String>,
    total_containers: usize,
    reconciled_containers: usize,
}

#[derive(Default)]
#[allow(dead_code)]
struct ContainerState {
    pod_uid: String,
    reconciled: bool,
}

#[derive(Default)]
struct InnerState {
    pods: HashMap<String, PodState>,       // keyed by pod UID
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
    #[allow(dead_code)]
    fn emit_event(&self, ev: PodResctrlEvent) {
        if let Err(e) = self.tx.try_send(ev) {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
            warn!("resctrl-plugin: failed to send event: {}", e);
        }
    }

    // Create or fetch pod state and ensure group exists
    fn handle_new_pod(&self, pod: &nri::api::PodSandbox) -> Option<String> {
        let pod_uid = &pod.uid;
        let mut st = self.state.lock().unwrap();
        let ps = st.pods.entry(pod_uid.clone()).or_insert(PodState {
            group_path: None,
            total_containers: 0,
            reconciled_containers: 0,
        });
        if ps.group_path.is_none() {
            match self.resctrl.create_group(pod_uid) {
                Ok(p) => {
                    ps.group_path = Some(p.clone());
                    Some(p)
                }
                Err(e) => {
                    warn!("resctrl-plugin: failed to create group for pod {}: {}", pod_uid, e);
                    None
                }
            }
        } else {
            ps.group_path.clone()
        }
    }

    fn handle_new_container(&self, pod: &nri::api::PodSandbox, container: &nri::api::Container) {
        let pod_uid = pod.uid.clone();
        let group_path = match self.handle_new_pod(pod) {
            Some(g) => g,
            None => {
                // Emit failure for pod without group
                let ev = PodResctrlEvent::Added(PodResctrlAdded {
                    pod_uid: pod_uid.clone(),
                    group_path: None,
                    state: AssignmentState::Failure,
                    total_containers: 0,
                    reconciled_containers: 0,
                });
                self.emit_event(ev);
                return;
            }
        };

        // Enumerate container PIDs and reconcile just this container into the pod group
        let passes = self.cfg.max_reconcile_passes;
        let pids = self.pid_source.pids_for_container(container);
        let res = self
            .resctrl
            .reconcile_group(&group_path, || Ok(pids.clone()), passes);

        let container_ok = matches!(res, Ok(ar) if ar.missing == 0);

        // Update state and emit event with counts
        // Update state and emit event with counts
        let (total, reconciled) = {
            let mut st = self.state.lock().unwrap();
            // Ensure pod entry and group path
            st.pods
                .entry(pod_uid.clone())
                .or_default()
                .group_path = Some(group_path.clone());

            // Prior reconciled status
            let _was_reconciled = st
                .containers
                .get(&container.id)
                .map(|c| c.reconciled)
                .unwrap_or(false);

            // Update/insert container state
            st.containers.insert(
                container.id.clone(),
                ContainerState { pod_uid: pod_uid.clone(), reconciled: container_ok },
            );

            // Recompute counts
            let total = st
                .containers
                .values()
                .filter(|c| c.pod_uid == pod_uid)
                .count();
            let reconciled = st
                .containers
                .values()
                .filter(|c| c.pod_uid == pod_uid && c.reconciled)
                .count();

            if let Some(ps) = st.pods.get_mut(&pod_uid) {
                ps.total_containers = total;
                ps.reconciled_containers = reconciled;
            }

            (total, reconciled)
        };

        let ev = PodResctrlEvent::Added(PodResctrlAdded {
            pod_uid: pod_uid.clone(),
            group_path: Some(group_path.clone()),
            state: if container_ok { AssignmentState::Success } else { AssignmentState::Partial },
            total_containers: total,
            reconciled_containers: reconciled,
        });
        self.emit_event(ev);
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
            Event::UPDATE_CONTAINER,
            Event::STOP_CONTAINER,
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

        // Ensure groups for all pods
        for pod in &req.pods {
            let _ = self.handle_new_pod(pod);
        }
        // Reconcile each container individually
        let pods_map: std::collections::HashMap<String, nri::api::PodSandbox> =
            req.pods.iter().map(|p| (p.id.clone(), p.clone())).collect();
        for c in &req.containers {
            if let Some(pod) = pods_map.get(&c.pod_sandbox_id) {
                self.handle_new_container(pod, c);
            }
        }

        Ok(SynchronizeResponse { update: vec![], more: req.more, special_fields: protobuf::SpecialFields::default() })
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
        if let (Some(pod), Some(container)) = (req.pod.as_ref(), req.container.as_ref()) {
            self.handle_new_container(pod, container);
        }
        Ok(UpdateContainerResponse::default())
    }

    async fn stop_container(
        &self,
        _ctx: &TtrpcContext,
        req: StopContainerRequest,
    ) -> ttrpc::Result<StopContainerResponse> {
        debug!("resctrl-plugin: stop_container: {}", req.container.id);
        if let (Some(pod), Some(container)) = (req.pod.as_ref(), req.container.as_ref()) {
            let pod_uid = pod.uid.clone();
            let mut st = self.state.lock().unwrap();
            if let Some(cstate) = st.containers.remove(&container.id) {
                if let Some(ps) = st.pods.get_mut(&pod_uid) {
                    if ps.total_containers > 0 {
                        ps.total_containers -= 1;
                    }
                    if cstate.reconciled && ps.reconciled_containers > 0 {
                        ps.reconciled_containers -= 1;
                    }
                    let ev = PodResctrlEvent::Added(PodResctrlAdded {
                        pod_uid: pod_uid.clone(),
                        group_path: ps.group_path.clone(),
                        state: AssignmentState::Success,
                        total_containers: ps.total_containers,
                        reconciled_containers: ps.reconciled_containers,
                    });
                    drop(st);
                    self.emit_event(ev);
                }
            }
        }
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
            Ok(Event::REMOVE_POD_SANDBOX) => {
                if let Some(pod) = req.pod.as_ref() {
                    let pod_uid = pod.uid.clone();
                    let mut st = self.state.lock().unwrap();
                    let group_path = st
                        .pods
                        .get(&pod_uid)
                        .and_then(|ps| ps.group_path.clone());
                    st.containers.retain(|_, c| c.pod_uid != pod_uid);
                    st.pods.remove(&pod_uid);
                    drop(st);
                    if let Some(gp) = group_path {
                        if let Err(e) = self.resctrl.delete_group(&gp) {
                            warn!("resctrl-plugin: failed to delete group {}: {}", gp, e);
                        }
                        self.emit_event(PodResctrlEvent::Removed(PodResctrlRemoved {
                            pod_uid,
                            group_path: Some(gp),
                        }));
                    } else {
                        self.emit_event(PodResctrlEvent::Removed(PodResctrlRemoved {
                            pod_uid,
                            group_path: None,
                        }));
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
    use std::sync::Arc;

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
        assert!(events.is_set(Event::UPDATE_CONTAINER));
        assert!(events.is_set(Event::STOP_CONTAINER));
        assert!(events.is_set(Event::REMOVE_CONTAINER));
    }

    #[tokio::test]
    async fn test_reconcile_emits_counts() {
        // Build a plugin with a test FS-backed resctrl
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

        // Inject a mock PID source that returns two PIDs for our container
        struct MockPidSrc;
        impl CgroupPidSource for MockPidSrc {
            fn pids_for_container(&self, _c: &nri::api::Container) -> Vec<i32> { vec![1,2] }
        }
        let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(8);
        let plugin = ResctrlPlugin::with_pid_source(ResctrlPluginConfig::default(), rc, tx, Arc::new(MockPidSrc));

        // Build synchronize request with one pod and one container
        let mut pod = nri::api::PodSandbox::default();
        pod.id = "pod-sb-1".into();
        pod.uid = "u123".into();

        let mut linux = nri::api::LinuxContainer::default();
        linux.cgroups_path = cg.to_string_lossy().into_owned();
        let mut container = nri::api::Container::default();
        container.id = "ctr1".into();
        container.pod_sandbox_id = pod.id.clone();
        container.linux = protobuf::MessageField::some(linux);

        let req = SynchronizeRequest {
            pods: vec![pod.clone()],
            containers: vec![container.clone()],
            more: false,
            special_fields: SpecialFields::default(),
        };

        let ctx = TtrpcContext { mh: ttrpc::MessageHeader::default(), metadata: std::collections::HashMap::new(), timeout_nano: 5_000 };
        let _ = plugin.synchronize(&ctx, req).await.unwrap();

        // Expect an Added event with Success and counts
        let ev = rx.recv().await.expect("one event");
        match ev {
            PodResctrlEvent::Added(a) => {
                assert_eq!(a.pod_uid, "u123");
                assert_eq!(a.state, AssignmentState::Success);
                assert!(a.group_path.is_some());
                assert_eq!(a.total_containers, 1);
                assert_eq!(a.reconciled_containers, 1);
            }
            _ => panic!("unexpected event type"),
        }

        // Trigger update for the same container; should emit another event with same counts
        let ureq = UpdateContainerRequest {
            pod: protobuf::MessageField::some(pod.clone()),
            container: protobuf::MessageField::some(container.clone()),
            linux_resources: protobuf::MessageField::none(),
            special_fields: SpecialFields::default(),
        };
        let _ = plugin.update_container(&ctx, ureq).await.unwrap();

        // Expect a second event
        let ev2 = rx.recv().await.expect("second event");
        match ev2 {
            PodResctrlEvent::Added(a) => {
                assert_eq!(a.total_containers, 1);
                assert_eq!(a.reconciled_containers, 1);
            }
            _ => panic!("unexpected event type"),
        }
    }
}
