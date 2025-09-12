use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use arrow_array::builder::StringBuilder;
use arrow_array::{ArrayRef, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use log::{debug, info, warn};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use nri::metadata::{ContainerMetadata, MetadataMessage, MetadataPlugin};
use nri::NRI;

/// Fields appended by the NRI enrichment task
const ENRICH_FIELDS: &[(&str, DataType)] = &[
    ("pod_name", DataType::Utf8),
    ("pod_namespace", DataType::Utf8),
    ("pod_uid", DataType::Utf8),
    ("container_name", DataType::Utf8),
    ("container_id", DataType::Utf8),
];

/// Attempt to resolve a cgroup path to an inode number (cgroup id)
///
/// Assumes `cgroup_path` is an absolute path under `/sys/fs/cgroup` and attempts to
/// resolve its inode number via `stat`.
fn resolve_cgroup_inode(cgroup_path: &str) -> Result<u64> {
    let path = Path::new(cgroup_path);
    let metadata = fs::metadata(path)
        .with_context(|| format!("Failed to get metadata for cgroup path: {:?}", path))?;
    Ok(metadata.ino())
}

/// Task that enriches incoming RecordBatches with container metadata based on cgroup_id
pub struct NRIEnrichRecordBatchTask {
    // Schemas
    output_schema: SchemaRef,

    // Mapping structures
    container_to_inode: HashMap<String, u64>,
    inode_to_metadata: HashMap<u64, ContainerMetadata>,
}

impl NRIEnrichRecordBatchTask {
    /// Create a new enrichment task with channels and input schema
    pub fn new(input_schema: SchemaRef) -> Self {
        // Build output schema (input + appended nullable columns)
        let mut fields: Vec<Field> = input_schema
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect();
        for (name, dt) in ENRICH_FIELDS.iter() {
            fields.push(Field::new(*name, dt.clone(), true));
        }
        let output_schema = Arc::new(Schema::new(fields));

        Self {
            output_schema,
            container_to_inode: HashMap::new(),
            inode_to_metadata: HashMap::new(),
        }
    }

    /// Return the output schema (input + enrichment columns)
    pub fn schema(&self) -> SchemaRef {
        self.output_schema.clone()
    }

    /// Initialize NRI plugin and connection using the provided metadata sender. Returns an
    /// active NRI instance and join handle when connected, or Ok(None) when best-effort disabled.
    async fn init_nri_with_sender(
        metadata_tx: mpsc::Sender<MetadataMessage>,
    ) -> Result<Option<(NRI, tokio::task::JoinHandle<Result<()>>)>> {
        let plugin = MetadataPlugin::new(metadata_tx);

        // Determine socket path
        let socket_path = std::env::var("NRI_SOCKET_PATH")
            .unwrap_or_else(|_| "/var/run/nri/nri.sock".to_string());

        // Try to connect
        match tokio::net::UnixStream::connect(&socket_path).await {
            Ok(stream) => {
                info!("Connecting to NRI socket at {}", socket_path);
                let (nri, join_handle) =
                    NRI::new(stream, plugin, "collector-metadata", "10").await?;

                // Register plugin
                if let Err(e) = nri.register().await {
                    warn!(
                        "NRI registration failed (best-effort mode, continuing without enrichment): {}",
                        e
                    );
                    return Ok(None); // Best-effort: remain without an active NRI
                }

                info!("NRI plugin registered successfully");
                Ok(Some((nri, join_handle)))
            }
            Err(e) => {
                warn!(
                    "NRI socket not available at {} (best-effort mode, continuing without enrichment): {}",
                    socket_path, e
                );
                // Best-effort: keep nri as None; enrichment will produce nulls
                Ok(None)
            }
        }
    }

    /// Process a single metadata message: update or remove mappings
    fn process_metadata_message(&mut self, msg: MetadataMessage) {
        match msg {
            MetadataMessage::Add(container_id, metadata) => {
                if metadata.cgroup_path.is_empty() {
                    debug!(
                        "Received metadata without cgroup_path for container {}, ignoring",
                        container_id
                    );
                    return;
                }
                match resolve_cgroup_inode(&metadata.cgroup_path) {
                    Ok(inode) => {
                        // Update both maps
                        self.container_to_inode.insert(container_id.clone(), inode);
                        self.inode_to_metadata.insert(inode, *metadata);
                    }
                    Err(e) => {
                        warn!(
                            "Failed to resolve cgroup inode for container {}: {}",
                            container_id, e
                        );
                    }
                }
            }
            MetadataMessage::Remove(container_id) => {
                if let Some(inode) = self.container_to_inode.remove(&container_id) {
                    self.inode_to_metadata.remove(&inode);
                }
            }
        }
    }

    /// Enrich a RecordBatch by appending enrichment columns. Best-effort: nulls when missing.
    fn enrich_batch(&self, batch: &RecordBatch) -> Result<RecordBatch> {
        // Find cgroup_id column index and ensure type is Int64
        let cgroup_idx = batch
            .schema()
            .fields()
            .iter()
            .position(|f| f.name() == "cgroup_id")
            .ok_or_else(|| anyhow!("cgroup_id column not found in input batch schema"))?;

        let cgroup_array = batch.column(cgroup_idx);
        let cgroup_ids = cgroup_array
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| anyhow!("cgroup_id column is not Int64"))?;

        let num_rows = batch.num_rows();

        // Builders for enrichment columns
        // Note: append_value may extend the StringBuilders if they need more space. We preallocate
        // ~16 bytes per row on average to balance memory usage with the overhead of reallocations;
        // not every row may have a value and the actual string sizes can be smaller overall.
        let mut pod_name_b = StringBuilder::with_capacity(num_rows, num_rows * 16);
        let mut pod_ns_b = StringBuilder::with_capacity(num_rows, num_rows * 16);
        let mut pod_uid_b = StringBuilder::with_capacity(num_rows, num_rows * 16);
        let mut container_name_b = StringBuilder::with_capacity(num_rows, num_rows * 16);
        let mut container_id_b = StringBuilder::with_capacity(num_rows, num_rows * 16);

        for i in 0..num_rows {
            let inode = cgroup_ids.value(i) as u64;
            if let Some(meta) = self.inode_to_metadata.get(&inode) {
                pod_name_b.append_value(meta.pod_name.as_str());
                pod_ns_b.append_value(meta.pod_namespace.as_str());
                pod_uid_b.append_value(meta.pod_uid.as_str());
                container_name_b.append_value(meta.container_name.as_str());
                container_id_b.append_value(meta.container_id.as_str());
            } else {
                pod_name_b.append_null();
                pod_ns_b.append_null();
                pod_uid_b.append_null();
                container_name_b.append_null();
                container_id_b.append_null();
            }
        }

        // Create new arrays vector: original columns + enrichment columns
        let mut arrays: Vec<ArrayRef> = batch.columns().to_vec();
        arrays.push(Arc::new(pod_name_b.finish()));
        arrays.push(Arc::new(pod_ns_b.finish()));
        arrays.push(Arc::new(pod_uid_b.finish()));
        arrays.push(Arc::new(container_name_b.finish()));
        arrays.push(Arc::new(container_id_b.finish()));

        RecordBatch::try_new(self.output_schema.clone(), arrays)
            .map_err(|e| anyhow!("Failed to create enriched RecordBatch: {}", e))
    }

    /// Run the enrichment task: read metadata and batches, output enriched batches
    pub async fn run(
        mut self,
        mut batch_receiver: mpsc::Receiver<RecordBatch>,
        batch_sender: mpsc::Sender<RecordBatch>,
        shutdown_token: CancellationToken,
    ) -> Result<()> {
        // Internal tracker for ancillary tasks (e.g., NRI plugin lifecycle)
        let task_tracker = TaskTracker::new();
        // Metadata channel for NRI plugin
        let (metadata_tx, mut metadata_rx) = mpsc::channel::<MetadataMessage>(1000);

        // Try initializing NRI (best-effort)
        let mut nri_opt: Option<NRI> = None;
        let mut nri_active = false;
        match Self::init_nri_with_sender(metadata_tx).await {
            Ok(Some((nri, join_handle))) => {
                // Monitor NRI lifecycle using the common task completion handler
                nri_active = true;
                nri_opt = Some(nri);
                let token = shutdown_token.clone();
                task_tracker.spawn(crate::task_completion_handler::task_completion_handler::<
                    _,
                    (),
                    anyhow::Error,
                >(
                    async move {
                        join_handle
                            .await
                            .map_err(|e| anyhow!("NRI join error: {}", e))?
                    },
                    token,
                    "NRIPlugin",
                ));
            }
            Ok(None) => { /* best-effort without NRI */ }
            Err(e) => {
                warn!(
                    "Failed to initialize NRI (best-effort mode, continuing without enrichment): {}",
                    e
                );
            }
        }

        // No more internal tasks will be spawned from here on
        task_tracker.close();

        // Default to success; if any error occurs, set this to Err and break.
        let mut res: Result<()> = Ok(());

        loop {
            tokio::select! {
                // Handle input record batches
                maybe_batch = batch_receiver.recv() => {
                    match maybe_batch {
                        Some(batch) => {
                            match self.enrich_batch(&batch) {
                                Ok(enriched) => {
                                    if let Err(e) = batch_sender.send(enriched).await {
                                        // Shutdown should initiate from upstream; downstream closed is an error
                                        debug!("Downstream batch channel closed unexpectedly: {}", e);
                                        res = Err(anyhow!("downstream batch channel closed: {}", e));
                                        break;
                                    }
                                },
                                Err(e) => {
                                    // Treat enrichment failure as fatal
                                    debug!("Failed to enrich batch: {}", e);
                                    res = Err(e);
                                    break;
                                }
                            }
                        },
                        None => {
                            // Input closed; shut down
                            debug!("Input batch channel closed, shutting down");
                            // Graceful shutdown: keep res as Ok
                            break;
                        }
                    }
                }

                // Handle metadata messages to update mapping
                maybe_msg = metadata_rx.recv(), if nri_active => {
                    if let Some(msg) = maybe_msg {
                        self.process_metadata_message(msg);
                    } else {
                        // If the metadata channel closed unexpectedly, treat as error
                        debug!("NRI metadata channel closed unexpectedly");
                        res = Err(anyhow!("nri metadata channel closed"));
                        break;
                    }
                }
            }
        }

        // Cleanup NRI on any exit path
        if let Some(nri) = &nri_opt {
            let _ = nri.close().await;
        }
        // Wait for internal tasks (e.g., NRI plugin) to complete
        task_tracker.wait().await;
        // Output channel closes when sender is dropped (self goes out of scope here)

        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::{Int32Builder, Int64Builder};
    use arrow_array::Array;
    use arrow_schema::Field;

    fn make_input_schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("start_time", DataType::Int64, false),
            Field::new("pid", DataType::Int32, false),
            Field::new("process_name", DataType::Utf8, true),
            Field::new("cgroup_id", DataType::Int64, false),
        ]))
    }

    fn make_simple_batch(schema: SchemaRef, cgroups: &[i64]) -> RecordBatch {
        let mut start_b = Int64Builder::with_capacity(cgroups.len());
        let mut pid_b = Int32Builder::with_capacity(cgroups.len());
        let mut proc_b = StringBuilder::with_capacity(cgroups.len(), cgroups.len() * 8);
        let mut cg_b = Int64Builder::with_capacity(cgroups.len());
        for (i, cg) in cgroups.iter().enumerate() {
            start_b.append_value(123);
            pid_b.append_value(i as i32);
            proc_b.append_value(format!("p{}", i));
            cg_b.append_value(*cg);
        }
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(start_b.finish()),
            Arc::new(pid_b.finish()),
            Arc::new(proc_b.finish()),
            Arc::new(cg_b.finish()),
        ];
        RecordBatch::try_new(schema, arrays).unwrap()
    }

    #[test]
    fn test_schema_appended_fields() {
        let schema = make_input_schema();
        let task = NRIEnrichRecordBatchTask::new(schema.clone());
        let out = task.schema();
        assert_eq!(
            out.fields().len(),
            schema.fields().len() + ENRICH_FIELDS.len()
        );
        let names: Vec<_> = out.fields().iter().map(|f| f.name().clone()).collect();
        assert!(names.ends_with(&[
            "pod_name".to_string(),
            "pod_namespace".to_string(),
            "pod_uid".to_string(),
            "container_name".to_string(),
            "container_id".to_string(),
        ]));
        for name in [
            "pod_name",
            "pod_namespace",
            "pod_uid",
            "container_name",
            "container_id",
        ] {
            let f = out.field(
                out.fields().len() - ENRICH_FIELDS.len()
                    + names.iter().rev().position(|n| n == name).unwrap(),
            );
            assert!(f.is_nullable());
        }
    }

    #[test]
    fn test_process_metadata_map_updates() {
        let schema = make_input_schema();
        let mut task = NRIEnrichRecordBatchTask::new(schema);

        // Use a known directory for inode: root "/" should exist
        let inode = fs::metadata("/").unwrap().ino();
        let meta = ContainerMetadata {
            container_id: "abc".into(),
            pod_name: "p".into(),
            pod_namespace: "ns".into(),
            pod_uid: "uid".into(),
            container_name: "c".into(),
            cgroup_path: "/".into(),
            pid: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
        };

        task.process_metadata_message(MetadataMessage::Add("abc".into(), Box::new(meta.clone())));
        assert_eq!(task.container_to_inode.get("abc").copied(), Some(inode));
        assert!(task.inode_to_metadata.contains_key(&inode));

        task.process_metadata_message(MetadataMessage::Remove("abc".into()));
        assert!(!task.container_to_inode.contains_key("abc"));
        assert!(!task.inode_to_metadata.contains_key(&inode));
    }

    #[test]
    fn test_enrich_batch_known_unknown() {
        let schema = make_input_schema();
        let mut task = NRIEnrichRecordBatchTask::new(schema.clone());

        // Prepare mapping for inode 42
        let cm = ContainerMetadata {
            container_id: "cont-1".into(),
            pod_name: "pod-a".into(),
            pod_namespace: "ns-a".into(),
            pod_uid: "uid-a".into(),
            container_name: "c-a".into(),
            cgroup_path: "x".into(),
            pid: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
        };
        task.inode_to_metadata.insert(42, cm);

        // Build batch with cgroup ids [42, 7]
        let batch = make_simple_batch(schema, &[42, 7]);
        let enriched = task.enrich_batch(&batch).unwrap();
        assert_eq!(
            enriched.num_columns(),
            batch.num_columns() + ENRICH_FIELDS.len()
        );
        assert_eq!(enriched.num_rows(), 2);

        use arrow_array::StringArray;
        let pod_name = enriched
            .column(enriched.num_columns() - ENRICH_FIELDS.len())
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let pod_ns = enriched
            .column(enriched.num_columns() - ENRICH_FIELDS.len() + 1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let pod_uid = enriched
            .column(enriched.num_columns() - ENRICH_FIELDS.len() + 2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let container_name = enriched
            .column(enriched.num_columns() - ENRICH_FIELDS.len() + 3)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let container_id = enriched
            .column(enriched.num_columns() - ENRICH_FIELDS.len() + 4)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();

        // Row 0 has metadata
        assert_eq!(pod_name.value(0), "pod-a");
        assert_eq!(pod_ns.value(0), "ns-a");
        assert_eq!(pod_uid.value(0), "uid-a");
        assert_eq!(container_name.value(0), "c-a");
        assert_eq!(container_id.value(0), "cont-1");

        // Row 1 should be nulls
        assert!(pod_name.is_null(1));
        assert!(pod_ns.is_null(1));
        assert!(pod_uid.is_null(1));
        assert!(container_name.is_null(1));
        assert!(container_id.is_null(1));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_resolve_cgroup_inode_best_effort() {
        // Try to resolve current process cgroup path if possible
        let contents = fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let mut cg_path: Option<String> = None;
        for line in contents.lines() {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 3 && parts[0] == "0" {
                cg_path = Some(parts[2].to_string());
                break;
            }
        }
        if let Some(path) = cg_path {
            // Construct absolute cgroup path under /sys/fs/cgroup
            let abs_path = if path.starts_with('/') {
                format!("/sys/fs/cgroup{}", path)
            } else {
                format!("/sys/fs/cgroup/{}", path)
            };
            let _ = resolve_cgroup_inode(&abs_path);
        }
    }
}
