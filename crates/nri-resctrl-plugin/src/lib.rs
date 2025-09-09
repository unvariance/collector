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

        // Subscribe to minimal container and pod lifecycle events.
        let mut events = EventMask::new();
        events.set(&[
            Event::CREATE_CONTAINER,
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
    }
}
