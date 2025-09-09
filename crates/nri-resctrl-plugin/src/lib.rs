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
    self,
    ConfigureRequest,
    ConfigureResponse,
    CreateContainerRequest,
    CreateContainerResponse,
    Empty,
    Event,
    StateChangeEvent,
    StopContainerRequest,
    StopContainerResponse,
    SynchronizeRequest,
    SynchronizeResponse,
    UpdateContainerRequest,
    UpdateContainerResponse,
    UpdatePodSandboxRequest,
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
    /// Capacity of the event channel to the collector
    pub event_channel_capacity: usize,
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
            event_channel_capacity: 128,
            auto_mount: false,
        }
    }
}

#[derive(Default)]
struct PodState {
    last_state: Option<AssignmentState>,
    group_path: Option<String>,
}

#[derive(Default)]
struct InnerState {
    pods: HashMap<String, PodState>, // keyed by pod UID
}

/// Resctrl NRI plugin. Generic over `FsProvider` for testability.
pub struct ResctrlPlugin<P: FsProvider = RealFs> {
    cfg: ResctrlPluginConfig,
    resctrl: Resctrl<P>,
    state: Mutex<InnerState>,
    tx: mpsc::Sender<PodResctrlEvent>,
    dropped_events: Arc<AtomicUsize>,
}

impl ResctrlPlugin<RealFs> {
    /// Create a new plugin with default real filesystem provider.
    /// Returns the plugin and the receiver for emitted events.
    pub fn new(cfg: ResctrlPluginConfig) -> (Self, mpsc::Receiver<PodResctrlEvent>) {
        let (tx, rx) = mpsc::channel(cfg.event_channel_capacity);
        let rc_cfg = ResctrlConfig {
            group_prefix: cfg.group_prefix.clone(),
            auto_mount: cfg.auto_mount,
            ..Default::default()
        };
        let plugin = Self {
            cfg,
            resctrl: Resctrl::new(rc_cfg),
            state: Mutex::new(InnerState::default()),
            tx,
            dropped_events: Arc::new(AtomicUsize::new(0)),
        };
        (plugin, rx)
    }
}

impl<P: FsProvider> ResctrlPlugin<P> {
    /// Create a new plugin with a custom resctrl handle (DI for tests).
    /// Returns the plugin and the receiver for emitted events.
    pub fn with_resctrl(
        cfg: ResctrlPluginConfig,
        resctrl: Resctrl<P>,
    ) -> (Self, mpsc::Receiver<PodResctrlEvent>) {
        let (tx, rx) = mpsc::channel(cfg.event_channel_capacity);
        let plugin = Self {
            cfg,
            resctrl,
            state: Mutex::new(InnerState::default()),
            tx,
            dropped_events: Arc::new(AtomicUsize::new(0)),
        };
        (plugin, rx)
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

        // Subscribe to container and pod lifecycle events.
        let mut events = EventMask::new();
        events.set(&[
            Event::CREATE_CONTAINER,
            Event::UPDATE_CONTAINER,
            Event::STOP_CONTAINER,
            Event::UPDATE_POD_SANDBOX,
            Event::RUN_POD_SANDBOX,
            Event::STOP_POD_SANDBOX,
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

        // Skeleton: no-op, reconciliation implemented in later sub-issues.
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

    async fn state_change(&self, _ctx: &TtrpcContext, req: StateChangeEvent) -> ttrpc::Result<Empty> {
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

    #[test]
    fn test_default_config() {
        let cfg = ResctrlPluginConfig::default();
        assert_eq!(cfg.group_prefix, "pod_");
        assert!(cfg.cleanup_on_start);
        assert_eq!(cfg.max_reconcile_passes, 10);
        assert_eq!(cfg.concurrency_limit, 1);
        assert_eq!(cfg.event_channel_capacity, 128);
        assert!(!cfg.auto_mount);
    }

    #[tokio::test]
    async fn test_configure_event_mask() {
        let (plugin, _rx) = ResctrlPlugin::new(ResctrlPluginConfig::default());

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

        // Must include container create/stop and pod events
        assert!(events.is_set(Event::CREATE_CONTAINER));
        assert!(events.is_set(Event::STOP_CONTAINER));
        assert!(events.is_set(Event::UPDATE_CONTAINER));
        assert!(events.is_set(Event::UPDATE_POD_SANDBOX));
        assert!(events.is_set(Event::RUN_POD_SANDBOX));
        assert!(events.is_set(Event::STOP_POD_SANDBOX));
        assert!(events.is_set(Event::REMOVE_POD_SANDBOX));
    }

    #[test]
    fn test_channel_capacity_no_drops_under_normal_flow() {
        let mut cfg = ResctrlPluginConfig::default();
        cfg.event_channel_capacity = 4;
        let (plugin, mut rx) = ResctrlPlugin::new(cfg);

        for i in 0..4 {
            plugin.emit_event(PodResctrlEvent::Added(PodResctrlAdded {
                pod_uid: format!("pod-{i}"),
                group_path: Some(format!("/sys/fs/resctrl/pod_{i}")),
                state: AssignmentState::Success,
            }));
        }

        // No drops expected
        assert_eq!(plugin.dropped_events(), 0);

        // Receive all 4
        for _ in 0..4 {
            let ev = rx.try_recv().expect("must receive event");
            match ev {
                PodResctrlEvent::Added(a) => {
                    assert!(a.group_path.is_some());
                }
                _ => panic!("unexpected event variant"),
            }
        }
    }
}
