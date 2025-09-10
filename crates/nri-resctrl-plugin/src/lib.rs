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
    last_state: Option<AssignmentState>,
    group_path: Option<String>,
}

#[derive(Default)]
#[allow(dead_code)]
struct InnerState {
    pods: HashMap<String, PodState>, // keyed by pod UID
}

/// Resctrl NRI plugin. Generic over `FsProvider` for testability.
pub struct ResctrlPlugin<P: FsProvider = RealFs> {
    #[allow(dead_code)]
    cfg: ResctrlPluginConfig,
    #[allow(dead_code)]
    resctrl: Resctrl<P>,
    #[allow(dead_code)]
    state: Mutex<InnerState>,
    tx: mpsc::Sender<PodResctrlEvent>,
    dropped_events: Arc<AtomicUsize>,
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

    fn should_emit_and_update(
        &self,
        pod_uid: &str,
        new_state: AssignmentState,
        group_path: Option<String>,
    ) -> Option<PodResctrlEvent> {
        let mut st = self.state.lock().unwrap();
        let ps = st.pods.entry(pod_uid.to_string()).or_default();

        let changed = ps.last_state.as_ref() != Some(&new_state) || ps.group_path != group_path;
        if changed {
            ps.last_state = Some(new_state.clone());
            ps.group_path = group_path.clone();
            Some(PodResctrlEvent::Added(PodResctrlAdded {
                pod_uid: pod_uid.to_string(),
                group_path,
                state: new_state,
            }))
        } else {
            None
        }
    }

    fn enumerate_container_pids(&self, container: &nri::api::Container) -> Vec<i32> {
        let mut out: Vec<i32> = Vec::new();
        // Determine cgroup path from linux fields if present
        let cg_path = container
            .linux
            .as_ref()
            .map(|l| l.cgroups_path.clone())
            .unwrap_or_default();
        if cg_path.is_empty() {
            return out;
        }

        use std::path::PathBuf;
        let fs = self.resctrl.fs_provider();
        // Prefer cgroup.procs, fallback to tasks
        let procs = PathBuf::from(&cg_path).join("cgroup.procs");
        let tasks = PathBuf::from(&cg_path).join("tasks");

        let content = match fs.read_to_string(&procs) {
            Ok(s) => Some(s),
            Err(_) => fs.read_to_string(&tasks).ok(),
        };

        if let Some(s) = content {
            for line in s.lines() {
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                if let Ok(pid) = t.parse::<i32>() {
                    out.push(pid);
                }
            }
        }
        out
    }

    fn enumerate_pod_pids(&self, containers: &[nri::api::Container]) -> Vec<i32> {
        let mut set = std::collections::BTreeSet::new();
        for c in containers {
            for pid in self.enumerate_container_pids(c) {
                set.insert(pid);
            }
        }
        set.into_iter().collect()
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
        events.set(&[Event::CREATE_CONTAINER, Event::UPDATE_CONTAINER, Event::RUN_POD_SANDBOX, Event::REMOVE_POD_SANDBOX]);

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

        // Build map from pod sandbox ID to pod and to containers for enumeration
        let _pods_map: std::collections::HashMap<String, nri::api::PodSandbox> = req
            .pods
            .iter()
            .map(|p| (p.id.clone(), p.clone()))
            .collect();
        let mut containers_by_pod: std::collections::HashMap<String, Vec<nri::api::Container>> =
            std::collections::HashMap::new();
        for c in &req.containers {
            containers_by_pod
                .entry(c.pod_sandbox_id.clone())
                .or_default()
                .push(c.clone());
        }

        for pod in &req.pods {
            let pod_uid = pod.uid.clone();
            // Create or ensure group exists
            let group_path = match self.resctrl.create_group(&pod_uid) {
                Ok(p) => Some(p),
                Err(e) => {
                    warn!("resctrl-plugin: failed to create group for pod {}: {}", pod_uid, e);
                    if let Some(ev) = self.should_emit_and_update(&pod_uid, AssignmentState::Failure, None) {
                        self.emit_event(ev);
                    }
                    None
                }
            };

            if let Some(group_path) = group_path {
                let containers = containers_by_pod
                    .get(&pod.id)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);

                let desired_containers = containers.to_vec();
                let passes = self.cfg.max_reconcile_passes;
                let result = self.resctrl.reconcile_group(
                    &group_path,
                    || {
                        // Enumerate across all containers of the pod on each pass
                        Ok(self.enumerate_pod_pids(&desired_containers))
                    },
                    passes,
                );

                let new_state = match result {
                    Ok(ar) => {
                        if ar.missing == 0 {
                            AssignmentState::Success
                        } else {
                            AssignmentState::Partial
                        }
                    }
                    Err(e) => {
                        warn!(
                            "resctrl-plugin: reconcile failed for pod {} group {}: {}",
                            pod_uid, group_path, e
                        );
                        AssignmentState::Partial
                    }
                };

                if let Some(ev) = self.should_emit_and_update(&pod_uid, new_state, Some(group_path)) {
                    self.emit_event(ev);
                }
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
        // Reconcile for the pod corresponding to this container
        if let Some(pod) = req.pod.as_ref() {
            let pod_uid = pod.uid.clone();
            if let Ok(group_path) = self.resctrl.create_group(&pod_uid) {
                let container_opt: Option<nri::api::Container> = req.container.as_ref().cloned();
                let passes = self.cfg.max_reconcile_passes;
                let result = self.resctrl.reconcile_group(
                    &group_path,
                    || {
                        let v: Vec<nri::api::Container> =
                            container_opt.as_ref().map(|c| vec![c.clone()]).unwrap_or_default();
                        Ok(self.enumerate_pod_pids(&v))
                    },
                    passes,
                );
                let new_state = match result {
                    Ok(ar) => if ar.missing == 0 { AssignmentState::Success } else { AssignmentState::Partial },
                    Err(e) => { warn!("resctrl-plugin: reconcile (create_container) failed: {}", e); AssignmentState::Partial },
                };
                if let Some(ev) = self.should_emit_and_update(&pod_uid, new_state, Some(group_path)) {
                    self.emit_event(ev);
                }
            } else if let Some(ev) = self.should_emit_and_update(&pod_uid, AssignmentState::Failure, None) {
                self.emit_event(ev);
            }
        }
        Ok(CreateContainerResponse::default())
    }

    async fn update_container(
        &self,
        _ctx: &TtrpcContext,
        req: UpdateContainerRequest,
    ) -> ttrpc::Result<UpdateContainerResponse> {
        debug!("resctrl-plugin: update_container: {}", req.container.id);
        if let Some(pod) = req.pod.as_ref() {
            let pod_uid = pod.uid.clone();
            if let Ok(group_path) = self.resctrl.create_group(&pod_uid) {
                let container_opt: Option<nri::api::Container> = req.container.as_ref().cloned();
                let passes = self.cfg.max_reconcile_passes;
                let result = self.resctrl.reconcile_group(
                    &group_path,
                    || {
                        let v: Vec<nri::api::Container> =
                            container_opt.as_ref().map(|c| vec![c.clone()]).unwrap_or_default();
                        Ok(self.enumerate_pod_pids(&v))
                    },
                    passes,
                );
                let new_state = match result {
                    Ok(ar) => if ar.missing == 0 { AssignmentState::Success } else { AssignmentState::Partial },
                    Err(e) => { warn!("resctrl-plugin: reconcile (update_container) failed: {}", e); AssignmentState::Partial },
                };
                if let Some(ev) = self.should_emit_and_update(&pod_uid, new_state, Some(group_path)) {
                    self.emit_event(ev);
                }
            } else if let Some(ev) = self.should_emit_and_update(&pod_uid, AssignmentState::Failure, None) {
                self.emit_event(ev);
            }
        }
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
    use tokio::time::{timeout, Duration};

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
    }

    #[tokio::test]
    async fn test_event_dedup_on_reconcile() {
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

        let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(8);
        let plugin = ResctrlPlugin::with_resctrl(ResctrlPluginConfig::default(), rc, tx);

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

        // Expect exactly one Added event with Success
        let ev = rx.recv().await.expect("one event");
        match ev {
            PodResctrlEvent::Added(a) => {
                assert_eq!(a.pod_uid, "u123");
                assert_eq!(a.state, AssignmentState::Success);
                assert!(a.group_path.is_some());
            }
            _ => panic!("unexpected event type"),
        }

        // Trigger update for the same container; should not emit a new event
        let ureq = UpdateContainerRequest {
            pod: protobuf::MessageField::some(pod.clone()),
            container: protobuf::MessageField::some(container.clone()),
            linux_resources: protobuf::MessageField::none(),
            special_fields: SpecialFields::default(),
        };
        let _ = plugin.update_container(&ctx, ureq).await.unwrap();

        // Ensure no further events within a short timeout
        let no_ev = timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(no_ev.is_err(), "unexpected additional event emitted");
    }
}
