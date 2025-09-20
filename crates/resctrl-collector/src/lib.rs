use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use arrow_array::builder::{Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use log::{debug, info, warn};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use nri::metadata::{ContainerMetadata, MetadataMessage, MetadataPlugin};
use nri::NRI;
use nri_resctrl_plugin::{PodResctrlEvent, ResctrlGroupState, ResctrlPlugin, ResctrlPluginConfig};

/// Default channel capacity for communication with the plugins
const DEFAULT_CHANNEL_CAPACITY: usize = 256;

/// Create the Arrow schema for resctrl LLC occupancy samples
pub fn create_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::Int64, false),
        Field::new("pod_namespace", DataType::Utf8, true),
        Field::new("pod_name", DataType::Utf8, true),
        Field::new("pod_uid", DataType::Utf8, true),
        Field::new("resctrl_group", DataType::Utf8, true),
        Field::new("llc_occupancy_bytes", DataType::Int64, false),
    ]))
}

/// Resctrl collector instance state
#[derive(Default)]
pub struct ResctrlCollector {
    resctrl_synced: AtomicBool,
    metadata_synced: AtomicBool,
}

impl ResctrlCollector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns true when both resctrl and metadata have produced at least
    /// one event since startup, indicating initial synchronize completed.
    pub fn ready(&self) -> bool {
        self.resctrl_synced.load(Ordering::Relaxed) && self.metadata_synced.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct PodState {
    group_path: Option<String>,
    total_containers: usize,
    reconciled_containers: usize,
}

/// Internal mutable state and handlers for the collector.
///
/// This isolates logic to make it easier to unit test individual handlers
/// without having to run the full event loop and real plugins.
pub(crate) struct ResctrlCollectorState {
    this: Arc<ResctrlCollector>,
    pods: HashMap<String, PodState>,               // keyed by pod_uid
    pod_labels: HashMap<String, (String, String)>, // pod_uid -> (ns, name)
    llc_reader: Box<dyn LlcReader + Send + Sync>,
    schema: SchemaRef,
    batch_sender: mpsc::Sender<RecordBatch>,
    dropped_batches: u64,
}

impl ResctrlCollectorState {
    pub(crate) fn new(
        this: Arc<ResctrlCollector>,
        batch_sender: mpsc::Sender<RecordBatch>,
        cfg: &ResctrlCollectorConfig,
    ) -> Self {
        let rc = resctrl::Resctrl::new(resctrl::Config {
            root: cfg.mountpoint.clone(),
            ..Default::default()
        });
        Self {
            this,
            pods: HashMap::new(),
            pod_labels: HashMap::new(),
            llc_reader: Box::new(rc),
            schema: create_schema(),
            batch_sender,
            dropped_batches: 0,
        }
    }

    /// Handle a periodic sampling tick: read group occupancies and emit a batch.
    pub(crate) fn handle_sample_timer(&mut self) {
        let now_ns: i64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0) as i64;

        // Snapshot groups to sample to avoid holding borrows
        let groups_to_sample: Vec<(String, String)> = self
            .pods
            .iter()
            .filter_map(|(uid, ps)| ps.group_path.as_ref().map(|gp| (uid.clone(), gp.clone())))
            .collect();

        let rows_cap = groups_to_sample.len();
        if rows_cap > 0 {
            let mut ts_b = Int64Builder::with_capacity(rows_cap);
            let mut ns_b = StringBuilder::with_capacity(rows_cap, rows_cap * 16);
            let mut name_b = StringBuilder::with_capacity(rows_cap, rows_cap * 16);
            let mut uid_b = StringBuilder::with_capacity(rows_cap, rows_cap * 16);
            let mut grp_b = StringBuilder::with_capacity(rows_cap, rows_cap * 24);
            let mut llc_b = Int64Builder::with_capacity(rows_cap);

            let mut rows_appended = 0usize;
            for (uid, gp) in groups_to_sample {
                match self.llc_reader.llc_occupancy_total_bytes(&gp) {
                    Ok(total) => {
                        let labels = self.pod_labels.get(&uid).cloned();
                        ts_b.append_value(now_ns);
                        if let Some((ns, name)) = labels {
                            ns_b.append_value(ns.as_str());
                            name_b.append_value(name.as_str());
                        } else {
                            ns_b.append_null();
                            name_b.append_null();
                        }
                        uid_b.append_value(uid.as_str());
                        grp_b.append_value(gp.as_str());
                        llc_b.append_value(total as i64);
                        rows_appended += 1;
                    }
                    Err(e) => {
                        debug!("resctrl read failed for {}: {}", gp, e);
                    }
                }
            }

            if rows_appended > 0 {
                let arrays: Vec<ArrayRef> = vec![
                    Arc::new(ts_b.finish()),
                    Arc::new(ns_b.finish()),
                    Arc::new(name_b.finish()),
                    Arc::new(uid_b.finish()),
                    Arc::new(grp_b.finish()),
                    Arc::new(llc_b.finish()),
                ];
                let batch = match RecordBatch::try_new(self.schema.clone(), arrays) {
                    Ok(b) => b,
                    Err(e) => {
                        debug!("failed to build occupancy batch: {}", e);
                        return;
                    }
                };
                if let Err(_e) = self.batch_sender.try_send(batch) {
                    self.dropped_batches += 1;
                }
            }
        }

        if self.dropped_batches > 0 {
            warn!(
                "Dropped {} occupancy batches in the last second",
                self.dropped_batches
            );
            self.dropped_batches = 0;
        }
    }

    /// Handle periodic health logging.
    pub(crate) fn handle_health_timer(&self) {
        let (failed, not_reconciled) = self.compute_health_counts();
        info!(
            "resctrl health: pods_failed={}, pods_unreconciled={}",
            failed, not_reconciled
        );
    }

    /// Compute health metrics for logging and tests.
    pub(crate) fn compute_health_counts(&self) -> (usize, usize) {
        let mut failed = 0usize;
        let mut not_reconciled = 0usize;
        for (_uid, ps) in self.pods.iter() {
            if ps.group_path.is_none() {
                failed += 1;
            }
            if ps.reconciled_containers < ps.total_containers {
                not_reconciled += 1;
            }
        }
        (failed, not_reconciled)
    }

    /// Handle a resctrl plugin event.
    pub(crate) fn handle_resctrl_event(&mut self, ev: PodResctrlEvent) {
        if !self.this.resctrl_synced.load(Ordering::Relaxed) {
            self.this.resctrl_synced.store(true, Ordering::Relaxed);
        }
        match ev {
            PodResctrlEvent::AddOrUpdate(add) => {
                let entry = self.pods.entry(add.pod_uid.clone()).or_default();
                entry.total_containers = add.total_containers;
                entry.reconciled_containers = add.reconciled_containers;
                if let ResctrlGroupState::Exists(p) = add.group_state {
                    entry.group_path = Some(p);
                } else {
                    entry.group_path = None;
                }
            }
            PodResctrlEvent::Removed(r) => {
                self.pods.remove(&r.pod_uid);
                self.pod_labels.remove(&r.pod_uid);
            }
        }
    }

    /// Handle a metadata plugin event.
    pub(crate) fn handle_metadata_event(&mut self, msg: MetadataMessage) {
        if !self.this.metadata_synced.load(Ordering::Relaxed) {
            self.this.metadata_synced.store(true, Ordering::Relaxed);
        }
        match msg {
            MetadataMessage::Add(_cid, boxed) => {
                let ContainerMetadata {
                    pod_uid,
                    pod_namespace,
                    pod_name,
                    ..
                } = *boxed;
                if !pod_uid.is_empty() {
                    self.pod_labels.insert(pod_uid, (pod_namespace, pod_name));
                }
            }
            MetadataMessage::Remove(_cid) => {
                // nothing to do at pod map
            }
        }
    }

    /// Handle a retry tick by invoking the plugin's retry mechanism.
    pub(crate) fn handle_retry_timer(&self, resctrl_plugin: &ResctrlPlugin) {
        if let Err(e) = resctrl_plugin.retry_all_once() {
            debug!("retry_all_once error: {:?}", e);
        }
    }
}

/// Tiny indirection over resctrl for sampling, to enable hermetic tests.
pub(crate) trait LlcReader {
    fn llc_occupancy_total_bytes(&self, group_path: &str) -> anyhow::Result<u64>;
}

impl<P: resctrl::FsProvider> LlcReader for resctrl::Resctrl<P> {
    fn llc_occupancy_total_bytes(&self, group_path: &str) -> anyhow::Result<u64> {
        Ok(resctrl::Resctrl::llc_occupancy_total_bytes(
            self, group_path,
        )?)
    }
}

#[cfg(test)]
impl ResctrlCollectorState {
    /// Test-only: override the LLC reader with a fake.
    pub(crate) fn set_llc_reader_for_test(&mut self, reader: Box<dyn LlcReader + Send + Sync>) {
        self.llc_reader = reader;
    }
}

#[derive(Clone, Debug)]
pub struct ResctrlCollectorConfig {
    /// Sampling interval
    pub sample_interval: Duration,
    /// Retry-all interval for the plugin
    pub retry_interval: Duration,
    /// Health logging interval
    pub health_interval: Duration,
    /// Output channel capacity (RecordBatches)
    pub channel_capacity: usize,
    /// resctrl mountpoint (root path)
    pub mountpoint: PathBuf,
}

impl Default for ResctrlCollectorConfig {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_secs(1),
            retry_interval: Duration::from_secs(10),
            health_interval: Duration::from_secs(60),
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            mountpoint: PathBuf::from("/sys/fs/resctrl"),
        }
    }
}

impl ResctrlCollectorConfig {
    /// Create a config from environment variables with sensible defaults.
    /// Supported variables:
    /// - `RESCTRL_SAMPLING_INTERVAL` (humantime, e.g., "1s", "500ms")
    /// - `RESCTRL_RETRY_INTERVAL` (humantime)
    /// - `RESCTRL_HEALTH_INTERVAL` (humantime)
    /// - `RESCTRL_CHANNEL_CAPACITY` (usize > 0)
    /// - `RESCTRL_MOUNT` (path)
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(s) = env::var("RESCTRL_SAMPLING_INTERVAL") {
            if let Ok(d) = humantime::parse_duration(&s) {
                cfg.sample_interval = d;
            }
        }
        if let Ok(s) = env::var("RESCTRL_RETRY_INTERVAL") {
            if let Ok(d) = humantime::parse_duration(&s) {
                cfg.retry_interval = d;
            }
        }
        if let Ok(s) = env::var("RESCTRL_HEALTH_INTERVAL") {
            if let Ok(d) = humantime::parse_duration(&s) {
                cfg.health_interval = d;
            }
        }
        if let Ok(s) = env::var("RESCTRL_CHANNEL_CAPACITY") {
            if let Ok(n) = s.parse::<usize>() {
                if n > 0 {
                    cfg.channel_capacity = n;
                }
            }
        }
        if let Ok(m) = env::var("RESCTRL_MOUNT") {
            if !m.is_empty() {
                cfg.mountpoint = PathBuf::from(m);
            }
        }
        cfg
    }
}

/// Run the resctrl collector loop.
///
/// Best-effort: if NRI runtime is not available, the task runs idle.
pub async fn run(
    this: Arc<ResctrlCollector>,
    batch_sender: mpsc::Sender<RecordBatch>,
    shutdown: CancellationToken,
    cfg: ResctrlCollectorConfig,
) -> Result<()> {
    // Outgoing channel is provided by caller; we use try_send and drop on full
    let (resctrl_tx, mut resctrl_rx) = mpsc::channel::<PodResctrlEvent>(cfg.channel_capacity);
    let (meta_tx, mut meta_rx) = mpsc::channel::<MetadataMessage>(cfg.channel_capacity);

    // Create plugins
    let resctrl_plugin = Arc::new(ResctrlPlugin::new(
        ResctrlPluginConfig::default(),
        resctrl_tx,
    ));
    let meta_plugin = Arc::new(MetadataPlugin::new(meta_tx));

    let task_tracker = TaskTracker::new();

    // Helper to connect a plugin to NRI (best-effort)
    async fn connect_plugin<P: nri::api_ttrpc::Plugin + Send + Sync + 'static>(
        plugin: Arc<P>,
        name: &str,
        idx: &str,
        task_tracker: &TaskTracker,
        shutdown: &CancellationToken,
    ) -> Result<Option<NRI>> {
        let socket_path = std::env::var("NRI_SOCKET_PATH")
            .unwrap_or_else(|_| "/var/run/nri/nri.sock".to_string());
        match tokio::net::UnixStream::connect(&socket_path).await {
            Ok(stream) => {
                info!("Connecting {} to NRI at {}", name, socket_path);
                let (nri, join_handle) = NRI::new(stream, plugin, name, idx).await?;
                if let Err(e) = nri.register().await {
                    warn!("{} registration failed (continuing without): {}", name, e);
                    Ok(None)
                } else {
                    task_tracker.spawn(tokio_helpers::join_handle_completion_handler(
                        join_handle,
                        shutdown.clone(),
                        String::from("NRIPlugin-") + name,
                    ));
                    Ok(Some(nri))
                }
            }
            Err(e) => {
                warn!(
                    "NRI socket unavailable for {} ({}): {}",
                    name, socket_path, e
                );
                Ok(None)
            }
        }
    }

    // Attempt connections
    let nri_resctrl = connect_plugin(
        resctrl_plugin.clone(),
        "resctrl-plugin",
        "10",
        &task_tracker,
        &shutdown,
    )
    .await?;
    let nri_meta = connect_plugin(
        meta_plugin.clone(),
        "metadata-for-resctrl-plugin",
        "10",
        &task_tracker,
        &shutdown,
    )
    .await?;

    task_tracker.close();

    // Internal state
    let mut state = ResctrlCollectorState::new(this.clone(), batch_sender, &cfg);

    // Intervals
    let mut sample_tick = tokio::time::interval(cfg.sample_interval);
    let mut retry_tick = tokio::time::interval(cfg.retry_interval);
    let mut health_tick = tokio::time::interval(cfg.health_interval);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                break;
            }
            // Periodic sample
            _ = sample_tick.tick() => {
                state.handle_sample_timer();
            }
            // Retry plugin work if connected
            _ = retry_tick.tick(), if nri_resctrl.is_some() => {
                state.handle_retry_timer(&resctrl_plugin);
            }
            // Health logging
            _ = health_tick.tick() => {
                state.handle_health_timer();
            }
            // Resctrl events
            maybe_ev = resctrl_rx.recv() => {
                if let Some(ev) = maybe_ev {
                    state.handle_resctrl_event(ev);
                }
            }
            // Metadata events
            maybe_meta = meta_rx.recv() => {
                if let Some(msg) = maybe_meta {
                    state.handle_metadata_event(msg);
                }
            }
        }
    }

    // Best-effort: close NRI connections
    if let Some(nri) = nri_resctrl {
        let _ = nri.close().await;
    }
    if let Some(nri) = nri_meta {
        let _ = nri.close().await;
    }

    // Close the sender to signal shutdown downstream
    drop(state.batch_sender);

    // Signal the cancellation token
    shutdown.cancel();

    task_tracker.wait().await;

    Ok(())
}

/// Test-only variant of the loop: inject receivers directly to avoid NRI runtime.
#[cfg(test)]
pub async fn run_with_injected_receivers(
    this: Arc<ResctrlCollector>,
    batch_sender: mpsc::Sender<RecordBatch>,
    shutdown: CancellationToken,
    cfg: ResctrlCollectorConfig,
    mut resctrl_rx: mpsc::Receiver<PodResctrlEvent>,
    mut meta_rx: mpsc::Receiver<MetadataMessage>,
) -> Result<()> {
    // Create dummy plugins for retry handler, but skip NRI wiring.
    let resctrl_plugin = Arc::new(ResctrlPlugin::new(
        ResctrlPluginConfig::default(),
        // Unused here; create a throwaway sender to satisfy API
        tokio::sync::mpsc::channel::<PodResctrlEvent>(cfg.channel_capacity).0,
    ));

    // Internal state
    let mut state = ResctrlCollectorState::new(this.clone(), batch_sender, &cfg);

    // Intervals
    let mut sample_tick = tokio::time::interval(cfg.sample_interval);
    let mut retry_tick = tokio::time::interval(cfg.retry_interval);
    let mut health_tick = tokio::time::interval(cfg.health_interval);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                break;
            }
            _ = sample_tick.tick() => {
                state.handle_sample_timer();
            }
            _ = retry_tick.tick() => {
                state.handle_retry_timer(&resctrl_plugin);
            }
            _ = health_tick.tick() => {
                state.handle_health_timer();
            }
            maybe_ev = resctrl_rx.recv() => {
                if let Some(ev) = maybe_ev {
                    state.handle_resctrl_event(ev);
                }
            }
            maybe_meta = meta_rx.recv() => {
                if let Some(msg) = maybe_meta {
                    state.handle_metadata_event(msg);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Array, Int64Array, StringArray};
    use nri::metadata::ContainerMetadata;
    use nri_resctrl_plugin::{PodResctrlAddOrUpdate, PodResctrlRemoved};

    struct FakeReader {
        map: std::collections::HashMap<String, std::result::Result<u64, ()>>,
    }
    impl FakeReader {
        fn new(map: std::collections::HashMap<String, std::result::Result<u64, ()>>) -> Self {
            Self { map }
        }
    }
    impl LlcReader for FakeReader {
        fn llc_occupancy_total_bytes(&self, group_path: &str) -> anyhow::Result<u64> {
            match self.map.get(group_path) {
                Some(Ok(v)) => Ok(*v),
                Some(Err(_)) => Err(anyhow::anyhow!("read error")),
                None => Ok(0),
            }
        }
    }

    fn drain_one_batch(rx: &mut tokio::sync::mpsc::Receiver<RecordBatch>) -> Option<RecordBatch> {
        rx.try_recv().ok()
    }

    #[tokio::test]
    async fn l0b_ready_gating_via_handlers() {
        let this = ResctrlCollector::new();
        let (tx, _rx) = mpsc::channel(4);
        let cfg = ResctrlCollectorConfig::default();
        let mut st = ResctrlCollectorState::new(this.clone(), tx, &cfg);

        assert!(!this.ready());
        st.handle_resctrl_event(PodResctrlEvent::AddOrUpdate(PodResctrlAddOrUpdate {
            pod_uid: "u1".into(),
            group_state: ResctrlGroupState::Exists("/sys/fs/resctrl/mon_groups/pod_u1".into()),
            total_containers: 1,
            reconciled_containers: 1,
        }));
        assert!(!this.ready());
        st.handle_metadata_event(MetadataMessage::Add(
            "c1".into(),
            Box::new(ContainerMetadata {
                container_id: "c1".into(),
                pod_name: "p".into(),
                pod_namespace: "ns".into(),
                pod_uid: "u1".into(),
                container_name: "n".into(),
                cgroup_path: String::new(),
                pid: None,
                labels: Default::default(),
                annotations: Default::default(),
            }),
        ));
        assert!(this.ready());
    }

    #[tokio::test]
    async fn l0b_schema_and_labeling() {
        let this = ResctrlCollector::new();
        let (tx, mut rx) = mpsc::channel(4);
        let cfg = ResctrlCollectorConfig::default();
        let mut st = ResctrlCollectorState::new(this.clone(), tx, &cfg);

        // Setup state: one pod, one group, with labels
        st.handle_resctrl_event(PodResctrlEvent::AddOrUpdate(PodResctrlAddOrUpdate {
            pod_uid: "u1".into(),
            group_state: ResctrlGroupState::Exists("/g1".into()),
            total_containers: 1,
            reconciled_containers: 1,
        }));
        st.handle_metadata_event(MetadataMessage::Add(
            "c1".into(),
            Box::new(ContainerMetadata {
                container_id: "c1".into(),
                pod_name: "p".into(),
                pod_namespace: "ns".into(),
                pod_uid: "u1".into(),
                container_name: "n".into(),
                cgroup_path: String::new(),
                pid: None,
                labels: Default::default(),
                annotations: Default::default(),
            }),
        ));

        // Inject fake reader
        let mut map = std::collections::HashMap::new();
        map.insert("/g1".to_string(), Ok(1234u64));
        st.set_llc_reader_for_test(Box::new(FakeReader::new(map)));

        // Sample
        st.handle_sample_timer();
        let batch = drain_one_batch(&mut rx).expect("expected batch");
        let schema = batch.schema();
        assert_eq!(schema.field(0).name(), "timestamp");
        assert_eq!(schema.field(1).name(), "pod_namespace");
        assert_eq!(schema.field(2).name(), "pod_name");
        assert_eq!(schema.field(3).name(), "pod_uid");
        assert_eq!(schema.field(4).name(), "resctrl_group");
        assert_eq!(schema.field(5).name(), "llc_occupancy_bytes");

        // Validate row contents
        assert_eq!(batch.num_rows(), 1);
        let ns = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let name = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let uid = batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let grp = batch
            .column(4)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let llc = batch
            .column(5)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();

        assert_eq!(ns.value(0), "ns");
        assert_eq!(name.value(0), "p");
        assert_eq!(uid.value(0), "u1");
        assert_eq!(grp.value(0), "/g1");
        assert_eq!(llc.value(0), 1234);
    }

    #[tokio::test]
    async fn l0b_missing_metadata_path() {
        let this = ResctrlCollector::new();
        let (tx, mut rx) = mpsc::channel(4);
        let cfg = ResctrlCollectorConfig::default();
        let mut st = ResctrlCollectorState::new(this.clone(), tx, &cfg);

        st.handle_resctrl_event(PodResctrlEvent::AddOrUpdate(PodResctrlAddOrUpdate {
            pod_uid: "u2".into(),
            group_state: ResctrlGroupState::Exists("/g2".into()),
            total_containers: 1,
            reconciled_containers: 1,
        }));
        let mut map = std::collections::HashMap::new();
        map.insert("/g2".to_string(), Ok(42u64));
        st.set_llc_reader_for_test(Box::new(FakeReader::new(map)));
        st.handle_sample_timer();
        let batch = drain_one_batch(&mut rx).expect("batch");
        let ns = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let name = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(ns.is_null(0));
        assert!(name.is_null(0));

        // Now add metadata and sample again
        let mut map2 = std::collections::HashMap::new();
        map2.insert("/g2".to_string(), Ok(10u64));
        st.set_llc_reader_for_test(Box::new(FakeReader::new(map2)));
        st.handle_metadata_event(MetadataMessage::Add(
            "c2".into(),
            Box::new(ContainerMetadata {
                container_id: "c2".into(),
                pod_name: "p2".into(),
                pod_namespace: "ns2".into(),
                pod_uid: "u2".into(),
                container_name: "n2".into(),
                cgroup_path: String::new(),
                pid: None,
                labels: Default::default(),
                annotations: Default::default(),
            }),
        ));
        st.handle_sample_timer();
        let batch2 = drain_one_batch(&mut rx).expect("batch2");
        let ns2 = batch2
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let name2 = batch2
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(ns2.value(0), "ns2");
        assert_eq!(name2.value(0), "p2");
    }

    #[tokio::test]
    async fn l0b_removal_lifecycle() {
        let this = ResctrlCollector::new();
        let (tx, mut rx) = mpsc::channel(4);
        let cfg = ResctrlCollectorConfig::default();
        let mut st = ResctrlCollectorState::new(this.clone(), tx, &cfg);

        st.handle_resctrl_event(PodResctrlEvent::AddOrUpdate(PodResctrlAddOrUpdate {
            pod_uid: "u3".into(),
            group_state: ResctrlGroupState::Exists("/g3".into()),
            total_containers: 1,
            reconciled_containers: 1,
        }));
        let mut map = std::collections::HashMap::new();
        map.insert("/g3".to_string(), Ok(1u64));
        st.set_llc_reader_for_test(Box::new(FakeReader::new(map)));
        st.handle_sample_timer();
        assert!(drain_one_batch(&mut rx).is_some());

        // Remove the pod and sample again → no rows
        st.handle_resctrl_event(PodResctrlEvent::Removed(PodResctrlRemoved {
            pod_uid: "u3".into(),
        }));
        st.handle_sample_timer();
        assert!(drain_one_batch(&mut rx).is_none());
    }

    #[tokio::test]
    async fn l0b_error_handling_read_failure() {
        let this = ResctrlCollector::new();
        let (tx, mut rx) = mpsc::channel(4);
        let cfg = ResctrlCollectorConfig::default();
        let mut st = ResctrlCollectorState::new(this.clone(), tx, &cfg);

        st.handle_resctrl_event(PodResctrlEvent::AddOrUpdate(PodResctrlAddOrUpdate {
            pod_uid: "u4".into(),
            group_state: ResctrlGroupState::Exists("/g4".into()),
            total_containers: 1,
            reconciled_containers: 1,
        }));
        let mut map = std::collections::HashMap::new();
        map.insert("/g4".to_string(), Err(()));
        st.set_llc_reader_for_test(Box::new(FakeReader::new(map)));
        st.handle_sample_timer();
        // No rows → no batch
        assert!(drain_one_batch(&mut rx).is_none());
    }

    #[tokio::test]
    async fn l0b_backpressure_drops() {
        let this = ResctrlCollector::new();
        let (tx, mut rx) = mpsc::channel(1); // small capacity to force drop
        let cfg = ResctrlCollectorConfig::default();
        let mut st = ResctrlCollectorState::new(this.clone(), tx, &cfg);

        st.handle_resctrl_event(PodResctrlEvent::AddOrUpdate(PodResctrlAddOrUpdate {
            pod_uid: "u5".into(),
            group_state: ResctrlGroupState::Exists("/g5".into()),
            total_containers: 1,
            reconciled_containers: 1,
        }));
        let mut map = std::collections::HashMap::new();
        map.insert("/g5".to_string(), Ok(77u64));
        st.set_llc_reader_for_test(Box::new(FakeReader::new(map)));

        // Two samples without draining → second should be dropped due to capacity=1
        st.handle_sample_timer();
        st.handle_sample_timer();
        // Drain at most one batch (the first)
        let first = drain_one_batch(&mut rx);
        assert!(first.is_some());
        // No second batch should be present
        assert!(drain_one_batch(&mut rx).is_none());
    }

    #[test]
    fn l0b_health_counts() {
        let this = ResctrlCollector::new();
        let (tx, _rx) = mpsc::channel(4);
        let cfg = ResctrlCollectorConfig::default();
        let mut st = ResctrlCollectorState::new(this, tx, &cfg);

        // One failed (no group), one unreconciled, one healthy
        st.pods.insert(
            "uA".into(),
            PodState {
                group_path: None,
                total_containers: 1,
                reconciled_containers: 0,
            },
        );
        st.pods.insert(
            "uB".into(),
            PodState {
                group_path: Some("/gB".into()),
                total_containers: 2,
                reconciled_containers: 1,
            },
        );
        st.pods.insert(
            "uC".into(),
            PodState {
                group_path: Some("/gC".into()),
                total_containers: 1,
                reconciled_containers: 1,
            },
        );

        let (failed, not_reconciled) = st.compute_health_counts();
        assert_eq!(failed, 1);
        assert_eq!(not_reconciled, 2); // uA and uB
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn l2_run_smoke_ticks_and_shutdown() -> anyhow::Result<()> {
        use tokio::time::{advance, Duration};
        let this = ResctrlCollector::new();
        let (tx, mut _rx) = tokio::sync::mpsc::channel(4);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let cfg = ResctrlCollectorConfig {
            sample_interval: Duration::from_millis(10),
            retry_interval: Duration::from_millis(10),
            health_interval: Duration::from_millis(10),
            channel_capacity: 4,
            mountpoint: "/does/not/exist".into(),
        };
        let jh = tokio::spawn(run(this.clone(), tx, shutdown.clone(), cfg));
        advance(Duration::from_millis(10)).await; // sample
        advance(Duration::from_millis(10)).await; // health
        assert!(!this.ready()); // no events yet
        shutdown.cancel();
        jh.await??;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn l2_ready_gating_with_events() -> anyhow::Result<()> {
        use tokio::time::{advance, Duration};
        let this = ResctrlCollector::new();
        let (batch_tx, mut _batch_rx) = tokio::sync::mpsc::channel(4);
        let (resctrl_tx, resctrl_rx) = tokio::sync::mpsc::channel(8);
        let (meta_tx, meta_rx) = tokio::sync::mpsc::channel(8);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let cfg = ResctrlCollectorConfig::default();
        let jh = tokio::spawn(run_with_injected_receivers(
            this.clone(),
            batch_tx,
            shutdown.clone(),
            cfg,
            resctrl_rx,
            meta_rx,
        ));
        assert!(!this.ready());
        resctrl_tx
            .send(PodResctrlEvent::AddOrUpdate(PodResctrlAddOrUpdate {
                pod_uid: "u1".into(),
                group_state: ResctrlGroupState::Exists("g1".into()),
                total_containers: 1,
                reconciled_containers: 1,
            }))
            .await
            .unwrap();
        advance(Duration::from_millis(1)).await;
        assert!(!this.ready());
        meta_tx
            .send(MetadataMessage::Add(
                "c1".into(),
                Box::new(ContainerMetadata {
                    container_id: "c1".into(),
                    pod_name: "p".into(),
                    pod_namespace: "ns".into(),
                    pod_uid: "u1".into(),
                    container_name: "cn".into(),
                    cgroup_path: String::new(),
                    pid: None,
                    labels: Default::default(),
                    annotations: Default::default(),
                }),
            ))
            .await
            .unwrap();
        advance(Duration::from_millis(1)).await;
        assert!(this.ready());
        shutdown.cancel();
        jh.await??;
        Ok(())
    }
}
