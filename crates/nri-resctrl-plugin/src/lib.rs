mod pid_source;

use std::collections::HashMap;
use std::ops::DerefMut as _;
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
pub(crate) enum ContainerSyncState {
    #[default]
    NoPod,
    Partial,
    Reconciled,
}

#[derive(Default)]
struct ContainerState {
    pod_uid: String,
    // Last known full cgroup path for this container
    cgroup_path: String,
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

// Plugin-specific error to distinguish benign races from resctrl errors
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("pod not found")]
    PodNotFound,
    #[error("container not found")]
    ContainerNotFound,
    #[error(transparent)]
    Resctrl(#[from] resctrl::Error),
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
            let full = nri::compute_full_cgroup_path(container, None);
            st.containers.insert(
                container_id.clone(),
                ContainerState {
                    pod_uid: pod_uid.clone(),
                    cgroup_path: full,
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
            let full = nri::compute_full_cgroup_path(container, Some(pod));
            st.containers.insert(
                container_id.clone(),
                ContainerState {
                    pod_uid: pod_uid.clone(),
                    cgroup_path: full,
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
        let full_path = nri::compute_full_cgroup_path(container, Some(pod));
        let full_for_closure = full_path.clone();
        let pid_resolver = move || -> Result<Vec<i32>, resctrl::Error> {
            pid_source.pids_for_path(&full_for_closure)
        };

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
                cgroup_path: full_path,
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

    /// Try to create a resctrl group for a pod if currently Failed.
    /// Emits AddOrUpdate only on state transition.
    pub fn retry_group_creation(&self, pod_uid: &str) -> Result<ResctrlGroupState, PluginError> {
        // Snapshot decision under lock. If pod missing → PodNotFound.
        // If state is not Failed, return current state immediately to avoid unlock/relock races.
        {
            let st = self.state.lock().unwrap();
            match st.pods.get(pod_uid) {
                Some(pod_state) => match &pod_state.group_state {
                    ResctrlGroupState::Failed => { /* continue and try create */ }
                    ResctrlGroupState::Exists(path) => {
                        return Ok(ResctrlGroupState::Exists(path.clone()))
                    }
                },
                None => return Err(PluginError::PodNotFound),
            }
        }

        // Drop lock while performing filesystem operation
        let res = self.resctrl.create_group(pod_uid);
        match res {
            Ok(path) => {
                let mut st = self.state.lock().unwrap();
                // Re-check and update under lock using exhaustive match
                match st.pods.get_mut(pod_uid) {
                    Some(pod_state) => match &pod_state.group_state {
                        ResctrlGroupState::Failed => {
                            pod_state.group_state = ResctrlGroupState::Exists(path.clone());
                            // Emit under lock to preserve ordering
                            self.emit_pod_add_or_update(pod_uid, pod_state);
                            Ok(ResctrlGroupState::Exists(path))
                        }
                        ResctrlGroupState::Exists(p) => Ok(ResctrlGroupState::Exists(p.clone())),
                    },
                    None => {
                        // Pod disappeared concurrently; best-effort cleanup not under lock
                        drop(st);
                        if let Err(e) = self.resctrl.delete_group(&path) {
                            warn!(
                                "resctrl-plugin: created group for removed pod {}; cleanup failed: {}",
                                pod_uid, e
                            );
                        }
                        Err(PluginError::PodNotFound)
                    }
                }
            }
            Err(e) => Err(PluginError::from(e)),
        }
    }

    /// Retry reconciling a single container if its pod group exists.
    /// Emits AddOrUpdate only if reconciled count is incremented.
    pub(crate) fn retry_container_reconcile(
        &self,
        container_id: &str,
    ) -> Result<ContainerSyncState, PluginError> {
        // Snapshot under lock: group path, cgroup path, passes, current state
        let (group_path, cgroup_path, pod_uid, _current_state, passes) = {
            let st = self.state.lock().unwrap();
            let container_state = st
                .containers
                .get(container_id)
                .ok_or(PluginError::ContainerNotFound)?;
            if container_state.state == ContainerSyncState::NoPod {
                return Ok(ContainerSyncState::NoPod);
            }
            let pod_state = st
                .pods
                .get(&container_state.pod_uid)
                .ok_or(PluginError::PodNotFound)?;
            let group_path = match &pod_state.group_state {
                ResctrlGroupState::Exists(p) => p.clone(),
                _ => return Ok(container_state.state),
            };
            (
                group_path,
                container_state.cgroup_path.clone(),
                container_state.pod_uid.clone(),
                container_state.state,
                self.cfg.max_reconcile_passes,
            )
        };

        // Perform reconcile outside the lock
        let pid_source = self.pid_source.clone();
        let pid_resolver =
            move || -> resctrl::Result<Vec<i32>> { pid_source.pids_for_path(&cgroup_path) };
        let res = self
            .resctrl
            .reconcile_group(&group_path, pid_resolver, passes)
            .map_err(PluginError::from)?;
        let new_state = if res.missing == 0 {
            ContainerSyncState::Reconciled
        } else {
            ContainerSyncState::Partial
        };

        // Re-acquire lock and update counters/state conditionally.
        // Ensure both container and pod are present before applying any change.
        let mut st = self.state.lock().unwrap();
        let st_mut = st.deref_mut();
        let container_entry = st_mut
            .containers
            .get_mut(container_id)
            .ok_or(PluginError::ContainerNotFound)?;
        let pod_entry = st_mut
            .pods
            .get_mut(&pod_uid)
            .ok_or(PluginError::PodNotFound)?;

        if matches!(&container_entry.state, ContainerSyncState::Partial)
            && new_state == ContainerSyncState::Reconciled
        {
            container_entry.state = ContainerSyncState::Reconciled;
            pod_entry.reconciled_containers += 1;
            // Emit under lock to preserve ordering
            self.emit_pod_add_or_update(&pod_uid, pod_entry);
            return Ok(ContainerSyncState::Reconciled);
        }
        Ok(container_entry.state)
    }

    /// Retry once across all pods/containers.
    /// Stops group-creation retries on first Capacity error in this pass.
    pub fn retry_all_once(&self) -> Result<(), PluginError> {
        // Snapshot lists under lock
        let (failed_pods, partial_containers): (Vec<String>, Vec<String>) = {
            let st = self.state.lock().unwrap();
            let pods = st
                .pods
                .iter()
                .filter_map(|(uid, ps)| {
                    if matches!(ps.group_state, ResctrlGroupState::Failed) {
                        Some(uid.clone())
                    } else {
                        None
                    }
                })
                .collect();
            let containers = st
                .containers
                .iter()
                .filter_map(|(cid, cs)| {
                    if cs.state == ContainerSyncState::Partial {
                        Some(cid.clone())
                    } else {
                        None
                    }
                })
                .collect();
            (pods, containers)
        };

        // Retry group creation until first capacity error
        for uid in failed_pods {
            match self.retry_group_creation(&uid) {
                Err(PluginError::Resctrl(resctrl::Error::Capacity { .. })) => break,
                Err(PluginError::PodNotFound) => continue,
                Err(e) => return Err(e),
                Ok(_) => {}
            }
        }

        // Retry container reconcile for partial containers
        for cid in partial_containers {
            match self.retry_container_reconcile(&cid) {
                Ok(_) => {}
                Err(PluginError::ContainerNotFound) | Err(PluginError::PodNotFound) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
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
        // Ensure resctrl is mounted according to config on every startup synchronize.
        // If mounting fails, log and continue; subsequent operations may be no-ops.
        let mounted_ok = match self.resctrl.ensure_mounted(self.cfg.auto_mount) {
            Ok(()) => true,
            Err(e) => {
                warn!("resctrl-plugin: ensure_mounted failed: {}", e);
                false
            }
        };

        // Startup cleanup: if enabled and mounted, remove stale groups.
        if self.cfg.cleanup_on_start && mounted_ok {
            match self.resctrl.cleanup_all() {
                Ok(rep) => {
                    info!(
                        "resctrl-plugin: startup cleanup report: removed={}, failures={}, race={}, non_prefix={}",
                        rep.removed, rep.removal_failures, rep.removal_race, rep.non_prefix_groups
                    );
                }
                Err(e) => {
                    // Log and continue; do not emit events for cleanup-only actions
                    warn!("resctrl-plugin: cleanup_all failed: {}", e);
                }
            }
        }
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
                    let group_path =
                        st.pods
                            .get(&pod_uid)
                            .and_then(|pod_state| match &pod_state.group_state {
                                ResctrlGroupState::Exists(path) => Some(path.clone()),
                                _ => None,
                            });

                    // Remove all containers for this pod
                    st.containers.retain(|_, c| c.pod_uid != pod_uid);
                    // Remove pod state
                    st.pods.remove(&pod_uid);
                    // Emit removal event under lock to preserve ordering
                    self.emit_event(PodResctrlEvent::Removed(PodResctrlRemoved {
                        pod_uid: pod_uid.clone(),
                    }));
                    drop(st);

                    // Delete resctrl group if it exists
                    if let Some(group_path) = group_path {
                        if let Err(e) = self.resctrl.delete_group(&group_path) {
                            warn!(
                                "resctrl-plugin: failed to delete group {}: {}",
                                group_path, e
                            );
                        }
                    }
                }
            }
            Ok(Event::REMOVE_CONTAINER) => {
                if let (Some(pod), Some(container)) = (req.pod.as_ref(), req.container.as_ref()) {
                    let pod_uid = pod.uid.clone();
                    let mut st = self.state.lock().unwrap();

                    // Adjust counts based on the removed container's previous state
                    let old_state = st.containers.remove(&container.id).map(|c| c.state);
                    if let Some(pod_state) = st.pods.get_mut(&pod_uid) {
                        if matches!(old_state, Some(s) if s != ContainerSyncState::NoPod) {
                            pod_state.total_containers =
                                pod_state.total_containers.saturating_sub(1);
                        }
                        if matches!(old_state, Some(ContainerSyncState::Reconciled)) {
                            pod_state.reconciled_containers =
                                pod_state.reconciled_containers.saturating_sub(1);
                        }
                        // Emit under lock to preserve ordering
                        self.emit_pod_add_or_update(&pod_uid, pod_state);
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

    #[derive(Clone)]
    struct TestFsState {
        files: std::collections::HashMap<std::path::PathBuf, String>,
        dirs: std::collections::HashSet<std::path::PathBuf>,
        no_perm_files: std::collections::HashSet<std::path::PathBuf>,
        nospace_dirs: std::collections::HashSet<std::path::PathBuf>,
        mkdir_calls: std::collections::HashMap<std::path::PathBuf, usize>,
        missing_pids: std::collections::HashSet<i32>,
    }

    #[derive(Clone)]
    struct TestFs {
        state: std::sync::Arc<std::sync::Mutex<TestFsState>>,
    }

    impl Default for TestFsState {
        fn default() -> Self {
            let mut s = TestFsState {
                files: Default::default(),
                dirs: Default::default(),
                no_perm_files: Default::default(),
                nospace_dirs: Default::default(),
                mkdir_calls: Default::default(),
                missing_pids: Default::default(),
            };
            // Provide a default /proc/mounts that indicates resctrl is mounted
            s.files.insert(
                std::path::PathBuf::from("/proc/mounts"),
                "resctrl /sys/fs/resctrl resctrl rw 0 0\n".to_string(),
            );
            s
        }
    }

    impl Default for TestFs {
        fn default() -> Self {
            TestFs {
                state: std::sync::Arc::new(std::sync::Mutex::new(TestFsState::default())),
            }
        }
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
        fn set_nospace_dir(&self, p: &std::path::Path) {
            let mut st = self.state.lock().unwrap();
            st.nospace_dirs.insert(p.to_path_buf());
        }
        fn clear_nospace_dir(&self, p: &std::path::Path) {
            let mut st = self.state.lock().unwrap();
            st.nospace_dirs.remove(p);
        }
        fn mkdir_count(&self, p: &std::path::Path) -> usize {
            let st = self.state.lock().unwrap();
            *st.mkdir_calls.get(p).unwrap_or(&0)
        }
        fn set_missing_pid(&self, pid: i32) {
            let mut st = self.state.lock().unwrap();
            st.missing_pids.insert(pid);
        }
        fn clear_missing_pid(&self, pid: i32) {
            let mut st = self.state.lock().unwrap();
            st.missing_pids.remove(&pid);
        }
    }

    impl FsProvider for TestFs {
        fn exists(&self, p: &std::path::Path) -> bool {
            let st = self.state.lock().unwrap();
            st.dirs.contains(p) || st.files.contains_key(p)
        }
        fn create_dir(&self, p: &std::path::Path) -> std::io::Result<()> {
            let mut st = self.state.lock().unwrap();
            *st.mkdir_calls.entry(p.to_path_buf()).or_default() += 1;
            if st.nospace_dirs.contains(p) {
                return Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
            }
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
            // If writing to tasks, simulate ESRCH for configured PIDs
            if p.file_name() == Some(std::ffi::OsStr::new("tasks")) {
                for line in data.lines() {
                    if let Ok(pid) = line.trim().parse::<i32>() {
                        if st.missing_pids.contains(&pid) {
                            return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                        }
                    }
                }
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
        fn read_child_dirs(&self, p: &std::path::Path) -> std::io::Result<Vec<String>> {
            let st = self.state.lock().unwrap();
            if !st.dirs.contains(p) {
                return Err(std::io::Error::from_raw_os_error(libc::ENOENT));
            }
            let mut out = Vec::new();
            for d in st.dirs.iter() {
                if d.parent() == Some(p) {
                    if let Some(name) = d.file_name() {
                        out.push(name.to_string_lossy().into_owned());
                    }
                }
            }
            Ok(out)
        }
        fn mount_resctrl(&self, _target: &std::path::Path) -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
        }
    }

    #[tokio::test]
    async fn test_cleanup_on_start_removes_only_prefix() {
        let fs = TestFs::default();
        // Ensure resctrl directories exist and contain entries
        let root = std::path::PathBuf::from("/sys/fs/resctrl");
        fs.add_dir(std::path::Path::new("/sys"));
        fs.add_dir(std::path::Path::new("/sys/fs"));
        fs.add_dir(&root);
        fs.add_dir(&root.join("mon_groups"));
        // Add some groups
        fs.add_dir(&root.join("pod_x1"));
        fs.add_dir(&root.join("other"));
        fs.add_dir(&root.join("mon_groups").join("pod_mx"));
        fs.add_dir(&root.join("mon_groups").join("foo"));

        let rc = Resctrl::with_provider(fs.clone(), resctrl::Config::default());
        let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(8);
        let plugin = ResctrlPlugin::with_resctrl(ResctrlPluginConfig::default(), rc, tx);

        let ctx = TtrpcContext {
            mh: ttrpc::MessageHeader::default(),
            metadata: std::collections::HashMap::new(),
            timeout_nano: 5_000,
        };
        let _ = plugin
            .synchronize(
                &ctx,
                SynchronizeRequest {
                    pods: vec![],
                    containers: vec![],
                    more: false,
                    special_fields: protobuf::SpecialFields::default(),
                },
            )
            .await
            .unwrap();

        // No events emitted for cleanup-only
        assert!(rx.try_recv().is_err());

        // Prefix dirs removed, others remain
        assert!(!fs.exists(&root.join("pod_x1")));
        assert!(fs.exists(&root.join("other")));
        assert!(!fs.exists(&root.join("mon_groups").join("pod_mx")));
        assert!(fs.exists(&root.join("mon_groups").join("foo")));
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

        let rc = Resctrl::with_provider(fs.clone(), resctrl::Config::default());

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

        let rc = Resctrl::with_provider(fs.clone(), resctrl::Config::default());

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

    #[tokio::test]
    async fn test_capacity_error_emits_failed_and_retry_group_creation_transitions() {
        use crate::pid_source::test_support::MockCgroupPidSource;
        use tokio::time::{timeout, Duration};

        let fs = TestFs::default();
        // Ensure resctrl root exists
        fs.add_dir(std::path::Path::new("/sys"));
        fs.add_dir(std::path::Path::new("/sys/fs"));
        fs.add_dir(std::path::Path::new("/sys/fs/resctrl"));

        let rc = Resctrl::with_provider(fs.clone(), resctrl::Config::default());

        let mock_pid_src = Arc::new(MockCgroupPidSource::new());
        let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(16);
        let plugin = ResctrlPlugin::with_pid_source(
            ResctrlPluginConfig::default(),
            rc,
            tx,
            mock_pid_src.clone(),
        );

        // Configure ENOSPC for the pod's group dir
        let group_path = std::path::PathBuf::from("/sys/fs/resctrl/pod_u1");
        fs.set_nospace_dir(&group_path);

        // Define pod and container
        let pod = nri::api::PodSandbox {
            id: "sb1".into(),
            uid: "u1".into(),
            ..Default::default()
        };
        let linux = nri::api::LinuxContainer {
            cgroups_path: "/cg/runtime:cri-containerd:c1".into(),
            ..Default::default()
        };
        let container = nri::api::Container {
            id: "c1".into(),
            pod_sandbox_id: pod.id.clone(),
            linux: protobuf::MessageField::some(linux),
            ..Default::default()
        };

        let ctx = TtrpcContext {
            mh: ttrpc::MessageHeader::default(),
            metadata: std::collections::HashMap::new(),
            timeout_nano: 5_000,
        };

        // Send RUN_POD_SANDBOX; expect Failed event
        let state_req = StateChangeEvent {
            event: Event::RUN_POD_SANDBOX.into(),
            pod: protobuf::MessageField::some(pod.clone()),
            container: protobuf::MessageField::none(),
            special_fields: SpecialFields::default(),
        };
        let _ = plugin.state_change(&ctx, state_req).await.unwrap();

        // Receive Failed AddOrUpdate (0/0)
        let ev = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("event")
            .expect("ev");
        match ev {
            PodResctrlEvent::AddOrUpdate(a) => {
                assert_eq!(a.pod_uid, "u1");
                assert!(matches!(a.group_state, ResctrlGroupState::Failed));
                assert_eq!(a.total_containers, 0);
                assert_eq!(a.reconciled_containers, 0);
            }
            _ => panic!("unexpected event"),
        }

        // Add a container while pod Failed → expect counts 1/0
        let create_req = CreateContainerRequest {
            pod: protobuf::MessageField::some(pod.clone()),
            container: protobuf::MessageField::some(container.clone()),
            special_fields: SpecialFields::default(),
        };
        let _ = Plugin::create_container(&plugin, &ctx, create_req)
            .await
            .unwrap();
        // Expect update with counts 1/0
        let ev = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("event")
            .expect("ev");
        match ev {
            PodResctrlEvent::AddOrUpdate(a) => {
                assert_eq!(a.total_containers, 1);
                assert_eq!(a.reconciled_containers, 0);
                assert!(matches!(a.group_state, ResctrlGroupState::Failed));
            }
            _ => panic!("unexpected event"),
        }

        // First retry: still ENOSPC → expect Error::Capacity and no event
        let err = plugin.retry_group_creation("u1").unwrap_err();
        match err {
            PluginError::Resctrl(resctrl::Error::Capacity { .. }) => {}
            other => panic!("expected capacity error, got: {:?}", other),
        }
        assert!(
            timeout(Duration::from_millis(50), rx.recv())
                .await
                .ok()
                .is_none(),
            "no duplicate events expected"
        );

        // Clear ENOSPC and retry again → should transition to Exists and emit event with counts 1/0
        fs.clear_nospace_dir(&group_path);
        let st = plugin.retry_group_creation("u1").expect("retry ok");
        match st {
            ResctrlGroupState::Exists(p) => assert!(p.ends_with("/sys/fs/resctrl/pod_u1")),
            _ => panic!("expected Exists"),
        }
        let ev = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("event")
            .expect("ev");
        match ev {
            PodResctrlEvent::AddOrUpdate(a) => {
                assert!(matches!(a.group_state, ResctrlGroupState::Exists(_)));
                assert_eq!(a.total_containers, 1);
                assert_eq!(a.reconciled_containers, 0);
            }
            _ => panic!("unexpected event"),
        }
    }

    #[tokio::test]
    async fn test_retry_container_reconcile_improves_counts() {
        use crate::pid_source::test_support::MockCgroupPidSource;
        use tokio::time::{timeout, Duration};

        let fs = TestFs::default();
        fs.add_dir(std::path::Path::new("/sys"));
        fs.add_dir(std::path::Path::new("/sys/fs"));
        fs.add_dir(std::path::Path::new("/sys/fs/resctrl"));

        let rc = Resctrl::with_provider(fs.clone(), resctrl::Config::default());

        let gp = std::path::PathBuf::from("/sys/fs/resctrl/pod_u1");
        fs.add_dir(&gp);
        fs.add_file(&gp.join("tasks"), "");

        let pod = nri::api::PodSandbox {
            id: "sb1".into(),
            uid: "u1".into(),
            ..Default::default()
        };
        let linux = nri::api::LinuxContainer {
            cgroups_path: "/cg/x:cri-containerd:c1".into(),
            ..Default::default()
        };
        let container = nri::api::Container {
            id: "c1".into(),
            pod_sandbox_id: pod.id.clone(),
            linux: protobuf::MessageField::some(linux),
            ..Default::default()
        };
        let full_cg = nri::compute_full_cgroup_path(&container, Some(&pod));

        let mut mock_pid_src = Arc::new(MockCgroupPidSource::new());
        Arc::get_mut(&mut mock_pid_src)
            .unwrap()
            .set_pids(full_cg.clone(), vec![101, 102]);

        let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(16);
        let plugin = ResctrlPlugin::with_pid_source(
            ResctrlPluginConfig::default(),
            rc,
            tx,
            mock_pid_src.clone(),
        );
        // Initially PIDs unassignable (ESRCH)
        fs.set_missing_pid(101);
        fs.set_missing_pid(102);

        // Run pod + add container → expect counts 1/0
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
        let create_req = CreateContainerRequest {
            pod: protobuf::MessageField::some(pod.clone()),
            container: protobuf::MessageField::some(container.clone()),
            special_fields: SpecialFields::default(),
        };
        let _ = Plugin::create_container(&plugin, &ctx, create_req)
            .await
            .unwrap();

        // Drain two events (pod created Exists and container accounted)
        let _ = timeout(Duration::from_millis(100), rx.recv()).await; // pod exists
        let _ = timeout(Duration::from_millis(200), rx.recv()).await; // container accounted

        // Make current PIDs assignable by clearing missing flags
        fs.clear_missing_pid(101);
        fs.clear_missing_pid(102);

        // Retry just this container → expect transition to Reconciled and one event with counts 1/1
        let st = plugin.retry_container_reconcile("c1").expect("retry ok");
        assert_eq!(st, ContainerSyncState::Reconciled);
        // Drain the event emitted for the transition to Reconciled (counts 1/1)
        let ev = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("event")
            .expect("ev");
        match ev {
            PodResctrlEvent::AddOrUpdate(a) => {
                assert_eq!(a.total_containers, 1);
                assert_eq!(a.reconciled_containers, 1);
            }
            _ => panic!("unexpected event"),
        }

        // Verify resctrl tasks now include the desired PIDs (101, 102)
        let pids = plugin
            .resctrl
            .list_group_tasks(gp.to_str().unwrap())
            .expect("list tasks");
        assert!(pids.contains(&101) && pids.contains(&102));

        // Validate internal state updated and counts improved
        {
            let inner = plugin.state.lock().unwrap();
            let ps = inner.pods.get("u1").expect("pod state");
            assert_eq!(ps.total_containers, 1);
            assert_eq!(ps.reconciled_containers, 1);
            let cs = inner.containers.get("c1").expect("container");
            assert_eq!(cs.state, ContainerSyncState::Reconciled);
        }
        // Re-run should not change counts further
        let _ = plugin.retry_container_reconcile("c1").expect("ok");

        // Ensure no further events are emitted after the second reconcile
        assert!(
            timeout(Duration::from_millis(50), rx.recv())
                .await
                .ok()
                .is_none(),
            "no extra event expected after second reconcile"
        );
    }

    #[tokio::test]
    async fn test_retry_all_once_early_stop_on_capacity_and_reconcile_others() {
        use crate::pid_source::test_support::MockCgroupPidSource;
        use tokio::time::{timeout, Duration};

        let fs = TestFs::default();
        fs.add_dir(std::path::Path::new("/sys"));
        fs.add_dir(std::path::Path::new("/sys/fs"));
        fs.add_dir(std::path::Path::new("/sys/fs/resctrl"));
        let rc = Resctrl::with_provider(fs.clone(), resctrl::Config::default());
        let (tx, mut rx) = mpsc::channel::<PodResctrlEvent>(32);
        let pod_a = nri::api::PodSandbox {
            id: "sbA".into(),
            uid: "uA".into(),
            ..Default::default()
        };
        let pod_b = nri::api::PodSandbox {
            id: "sbB".into(),
            uid: "uB".into(),
            ..Default::default()
        };
        let linux_b = nri::api::LinuxContainer {
            cgroups_path: "/cg/b:cri-containerd:b1".into(),
            ..Default::default()
        };
        let ctr_b = nri::api::Container {
            id: "b1".into(),
            pod_sandbox_id: pod_b.id.clone(),
            linux: protobuf::MessageField::some(linux_b),
            ..Default::default()
        };

        let mut mock_pid_src = Arc::new(MockCgroupPidSource::new());
        let cg_b = nri::compute_full_cgroup_path(&ctr_b, Some(&pod_b));
        Arc::get_mut(&mut mock_pid_src)
            .unwrap()
            .set_pids(cg_b.clone(), vec![222, 223]);

        let plugin = ResctrlPlugin::with_pid_source(
            ResctrlPluginConfig::default(),
            rc,
            tx,
            mock_pid_src.clone(),
        );

        // uA: Failed pod due to ENOSPC
        let u_a_gp = std::path::PathBuf::from("/sys/fs/resctrl/pod_uA");
        fs.set_nospace_dir(&u_a_gp);
        // uB: Existing group and one Partial container
        let u_b_gp = std::path::PathBuf::from("/sys/fs/resctrl/pod_uB");
        fs.add_dir(&u_b_gp);
        fs.add_file(&u_b_gp.join("tasks"), "");
        fs.set_missing_pid(222);
        fs.set_missing_pid(223);

        // Feed state
        let ctx = TtrpcContext {
            mh: ttrpc::MessageHeader::default(),
            metadata: std::collections::HashMap::new(),
            timeout_nano: 5_000,
        };
        let _ = plugin
            .state_change(
                &ctx,
                StateChangeEvent {
                    event: Event::RUN_POD_SANDBOX.into(),
                    pod: protobuf::MessageField::some(pod_a.clone()),
                    container: protobuf::MessageField::none(),
                    special_fields: SpecialFields::default(),
                },
            )
            .await
            .unwrap();
        let _ = plugin
            .state_change(
                &ctx,
                StateChangeEvent {
                    event: Event::RUN_POD_SANDBOX.into(),
                    pod: protobuf::MessageField::some(pod_b.clone()),
                    container: protobuf::MessageField::none(),
                    special_fields: SpecialFields::default(),
                },
            )
            .await
            .unwrap();
        let _ = Plugin::create_container(
            &plugin,
            &ctx,
            CreateContainerRequest {
                pod: protobuf::MessageField::some(pod_b.clone()),
                container: protobuf::MessageField::some(ctr_b.clone()),
                special_fields: SpecialFields::default(),
            },
        )
        .await
        .unwrap();

        // Drain initial events
        let _ = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("no-timeout")
            .expect("received event"); // uA failed
        let _ = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("no-timeout")
            .expect("received event"); // uB exists
        let ev = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("no-timeout")
            .expect("received event"); // uB counts 1/0
        match ev {
            PodResctrlEvent::AddOrUpdate(a) => {
                assert_eq!(a.pod_uid, "uB");
                assert_eq!(a.total_containers, 1);
                assert_eq!(a.reconciled_containers, 0);
            }
            _ => panic!("unexpected event"),
        }

        // Make current PIDs assignable now
        fs.clear_missing_pid(222);
        fs.clear_missing_pid(223);

        // Run retry_all_once: should attempt uA once and stop on capacity, then reconcile uB
        let before = fs.mkdir_count(&u_a_gp);
        plugin.retry_all_once().expect("retry all ok");
        // mkdir called exactly once for uA during this pass
        let after = fs.mkdir_count(&u_a_gp);
        assert_eq!(
            after.saturating_sub(before),
            1,
            "expected single create_dir attempt in this pass"
        );

        // Validate internal state improved for uB
        {
            let inner = plugin.state.lock().unwrap();
            let ps = inner.pods.get("uB").expect("pod uB");
            assert_eq!(ps.reconciled_containers, 1);
        }
    }
}
