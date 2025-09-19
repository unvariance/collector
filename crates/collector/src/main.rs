use anyhow::{anyhow, Result};
use arrow_array::RecordBatch;
use bpf::BpfLoader;
use bpf_sync_timer::SyncTimer;
use clap::Parser;
use log::{debug, error, info};
use object_store::ObjectStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

// Import local modules
mod bpf_error_handler;
mod bpf_perf_to_timeslot;
mod bpf_perf_to_trace;
mod bpf_task_tracker;
mod bpf_timeslot_tracker;
mod health_server;
mod metrics;
mod nri_enrich_recordbatch_task;
mod parquet_writer;
mod parquet_writer_task;
mod perf_event_processor;
mod task_metadata;
mod timeslot_data;
mod timeslot_to_recordbatch_task;

use nri_enrich_recordbatch_task::NRIEnrichRecordBatchTask;
use parquet_writer::{ParquetWriter, ParquetWriterConfig};
use parquet_writer_task::ParquetWriterTask;
use perf_event_processor::{PerfEventProcessor, ProcessorMode};
use timeslot_data::TimeslotData;
use timeslot_to_recordbatch_task::TimeslotToRecordBatchTask;
use tokio_utils::task_completion_handler;

/// Number of perf ring buffer pages for timeslot mode
const TIMESLOT_PERF_RING_PAGES: u32 = 32;

/// Number of perf ring buffer pages for trace mode (needs more buffering)
const TRACE_PERF_RING_PAGES: u32 = 256;
/// Interval for sync timer ticks (1 ms)
const SYNC_TIMER_INTERVAL_NS: u64 = 1_000_000;

/// Linux process monitoring tool
#[derive(Debug, Parser)]
struct Command {
    /// Verbose debug output
    #[arg(short, long)]
    verbose: bool,

    /// Track duration in seconds (0 = unlimited)
    #[arg(short, long, default_value = "0")]
    duration: u64,

    /// Storage type (local or s3)
    #[arg(long, default_value = "local")]
    storage_type: String,

    /// Prefix for storage path
    #[arg(short, long, default_value = "unvariance-metrics-")]
    prefix: String,

    /// Maximum memory buffer size before flushing (bytes)
    #[arg(long, default_value = "104857600")] // 100MB
    parquet_buffer_size: usize,

    /// Maximum size for each Parquet file before rotation (bytes)
    #[arg(long, default_value = "1073741824")] // 1GB
    parquet_file_size: usize,

    /// Maximum row group size (number of rows) in a Parquet Row Group
    #[arg(long, default_value = "1048576")]
    max_row_group_size: usize,

    /// Maximum total bytes to write to object store
    #[arg(long)]
    storage_quota: Option<usize>,

    /// Enable trace mode (outputs individual events instead of aggregated timeslots)
    #[arg(long, default_value = "false")]
    trace: bool,

    /// Enable resctrl LLC occupancy collection (1 Hz)
    #[arg(long, default_value = "false")]
    enable_resctrl: bool,

    /// Storage filename prefix for resctrl occupancy parquet files
    #[arg(long, default_value = "resctrl-occupancy-")]
    resctrl_prefix: String,

    /// Address to bind the health HTTP server (for readiness/liveness)
    #[arg(long, default_value = "0.0.0.0:8080")]
    health_addr: String,
}

/// Duration timeout handler - exits when duration completes or cancellation token is triggered
async fn duration_timeout_handler(
    duration: Duration,
    cancellation_token: CancellationToken,
) -> Result<()> {
    // Wait for either duration timeout or cancellation
    tokio::select! {
        _ = tokio::time::sleep(duration) => {
            debug!("Duration timeout reached");
        }
        _ = cancellation_token.cancelled() => {
            debug!("Duration timeout handler cancelled");
        }
    }
    Ok(())
}

/// Signal handler for SIGTERM and SIGINT - triggers cancellation when received
async fn signal_handler(cancellation_token: CancellationToken) -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => {
            debug!("Received SIGTERM, triggering shutdown");
            cancellation_token.cancel();
        }
        _ = sigint.recv() => {
            debug!("Received SIGINT, triggering shutdown");
            cancellation_token.cancel();
        }
        _ = cancellation_token.cancelled() => {
            debug!("Signal handler cancelled");
        }
    }
    Ok(())
}

/// SIGUSR1 rotation handler - sends rotation signals when SIGUSR1 is received
async fn rotation_handler(
    rotate_sender: mpsc::Sender<()>,
    cancellation_token: CancellationToken,
) -> Result<()> {
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    loop {
        tokio::select! {
            _ = sigusr1.recv() => {
                debug!("Received SIGUSR1, rotating parquet file");
                if let Err(e) = rotate_sender.send(()).await {
                    error!("Failed to send rotation signal: {}", e);
                    // If rotation channel is closed, we can exit
                    break;
                }
            }
            _ = cancellation_token.cancelled() => {
                debug!("Rotation handler cancelled");
                break;
            }
        }
    }
    Ok(())
}

// Create object store based on storage type
fn create_object_storage(storage_type: &str) -> Result<Arc<dyn ObjectStore>> {
    match storage_type.to_lowercase().as_str() {
        "s3" => {
            debug!("Creating S3 object store from environment variables");
            let s3 = object_store::aws::AmazonS3Builder::from_env().build()?;
            Ok(Arc::new(s3))
        }
        _ => {
            debug!("Creating local filesystem object store");
            let local = object_store::local::LocalFileSystem::new();
            Ok(Arc::new(local))
        }
    }
}

/// Find node identity for file path construction
fn get_node_identity() -> String {
    // Try to get hostname
    if let Ok(name) = hostname::get() {
        if let Ok(name_str) = name.into_string() {
            return name_str;
        }
    }

    // Fallback to a UUID if hostname is not available
    Uuid::new_v4().to_string().chars().take(8).collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize env_logger
    env_logger::init();

    let opts = Command::parse();

    debug!("Starting collector with options: {:?}", opts);

    // Get node identity for file path
    let node_id = get_node_identity();

    // Create object store based on storage type
    let store = create_object_storage(&opts.storage_type)?;

    // Determine the number of available CPUs
    let num_cpus = libbpf_rs::num_possible_cpus()?;

    // Compose storage prefix with node identity for main stream
    let storage_prefix = format!("{}{}", opts.prefix, node_id);

    // Create CPU count metadata for parquet files
    let cpu_metadata = vec![parquet::file::metadata::KeyValue {
        key: "num_cpus".to_string(),
        value: Some(num_cpus.to_string()),
    }];

    // Create ParquetWriterConfig with the storage prefix and metadata
    let config = ParquetWriterConfig {
        storage_prefix,
        buffer_size: opts.parquet_buffer_size,
        file_size_limit: opts.parquet_file_size,
        max_row_group_size: opts.max_row_group_size,
        storage_quota: opts.storage_quota,
        key_value_metadata: Some(cpu_metadata.clone()),
    };

    // Create channels for the pipeline
    // Upstream processors -> Enricher
    let (pre_enrich_sender, pre_enrich_receiver) = mpsc::channel::<RecordBatch>(1000);
    // Enricher -> Writer
    let (batch_sender, batch_receiver) = mpsc::channel::<RecordBatch>(1000);
    let (rotate_sender, rotate_receiver) = mpsc::channel::<()>(1);

    // Create shutdown token and task tracker
    let shutdown_token = CancellationToken::new();
    let task_tracker = TaskTracker::new();

    // Configure processor mode and schema based on trace flag
    let (processor_mode, input_schema) = if opts.trace {
        // Trace mode: direct RecordBatch output
        let schema = crate::bpf_perf_to_trace::create_schema();
        (ProcessorMode::Trace(pre_enrich_sender), schema)
    } else {
        // Timeslot mode: aggregated output with conversion
        let (timeslot_sender, timeslot_receiver) = mpsc::channel::<TimeslotData>(1000);

        // Create the conversion task and get schema
        let conversion_task = TimeslotToRecordBatchTask::new(timeslot_receiver, pre_enrich_sender);
        let schema = conversion_task.schema();

        // Spawn the conversion task
        task_tracker.spawn(task_completion_handler(
            conversion_task.run(),
            shutdown_token.clone(),
            "TimeslotToRecordBatchTask",
        ));

        (ProcessorMode::Timeslot(timeslot_sender), schema)
    };

    // Create the NRI enrichment task between conversion/trace and the writer
    let enrich_task = NRIEnrichRecordBatchTask::new(input_schema.clone());
    let schema = enrich_task.schema();

    // Spawn the enrichment task
    task_tracker.spawn(task_completion_handler(
        enrich_task.run(pre_enrich_receiver, batch_sender, shutdown_token.clone()),
        shutdown_token.clone(),
        "NRIEnrichRecordBatchTask",
    ));

    // Create the ParquetWriter with the appropriate schema
    debug!(
        "Writing {} data to {} storage with prefix: {}",
        if opts.trace { "trace" } else { "timeslot" },
        &opts.storage_type,
        &config.storage_prefix
    );
    let writer = ParquetWriter::new(store.clone(), schema, config)?;

    // Create ParquetWriterTask with pre-configured channels
    let writer_task = ParquetWriterTask::new(writer, batch_receiver, rotate_receiver);

    // Spawn the writer task with completion handler using task tracker
    task_tracker.spawn(task_completion_handler(
        writer_task.run(),
        shutdown_token.clone(),
        "ParquetWriterTask",
    ));

    debug!("Parquet writer task initialized and ready to receive data");

    // Readiness provider for health server
    let mut ready_provider: Option<Arc<dyn Fn() -> bool + Send + Sync>> = None;

    // Optionally enable resctrl occupancy collection with a dedicated writer
    if opts.enable_resctrl {
        // Schema for occupancy
        let occ_schema = resctrl_collector::create_schema();

        // (delayed) writer + channel setup follows after config parsing

        // Spawn the resctrl-collector loop with config from env
        let rcfg = resctrl_collector::ResctrlCollectorConfig::from_env();
        // Create writer and channels for occupancy using parsed config
        // Use a separate prefix for resctrl outputs to avoid mixing files
        let occ_prefix = format!("{}{}", opts.resctrl_prefix, node_id);
        let occ_config = ParquetWriterConfig {
            storage_prefix: occ_prefix,
            buffer_size: opts.parquet_buffer_size,
            file_size_limit: opts.parquet_file_size,
            max_row_group_size: opts.max_row_group_size,
            storage_quota: opts.storage_quota,
            key_value_metadata: Some(cpu_metadata.clone()),
        };
        let (occ_sender, occ_receiver) = mpsc::channel::<RecordBatch>(64);
        let (occ_rotate_tx, occ_rotate_rx) = mpsc::channel::<()>(1);
        let occ_writer = ParquetWriter::new(store.clone(), occ_schema, occ_config)?;
        let occ_writer_task = ParquetWriterTask::new(occ_writer, occ_receiver, occ_rotate_rx);

        // Spawn writer task
        task_tracker.spawn(task_completion_handler(
            occ_writer_task.run(),
            shutdown_token.clone(),
            "ResctrlParquetWriterTask",
        ));

        // Spawn rotation handler for occupancy writer (separate signal stream)
        task_tracker.spawn(task_completion_handler(
            rotation_handler(occ_rotate_tx.clone(), shutdown_token.clone()),
            shutdown_token.clone(),
            "ResctrlRotationHandler",
        ));

        let rc_instance = resctrl_collector::ResctrlCollector::new();
        // Set ready provider based on collector readiness
        ready_provider = Some({
            let rc_clone = rc_instance.clone();
            Arc::new(move || rc_clone.ready())
        });
        task_tracker.spawn(task_completion_handler(
            resctrl_collector::run(rc_instance, occ_sender, shutdown_token.clone(), rcfg),
            shutdown_token.clone(),
            "ResctrlCollector",
        ));
    }

    // If resctrl not enabled, default readiness is true
    if ready_provider.is_none() {
        ready_provider = Some(Arc::new(|| true));
    }

    // Spawn duration timeout handler only if duration is non-zero
    if opts.duration > 0 {
        let duration = Duration::from_secs(opts.duration);
        task_tracker.spawn(task_completion_handler(
            duration_timeout_handler(duration, shutdown_token.clone()),
            shutdown_token.clone(),
            "DurationTimeoutHandler",
        ));
    }

    // Spawn signal handler for SIGTERM/SIGINT
    task_tracker.spawn(task_completion_handler(
        signal_handler(shutdown_token.clone()),
        shutdown_token.clone(),
        "SignalHandler",
    ));

    // Spawn rotation handler for SIGUSR1
    task_tracker.spawn(task_completion_handler(
        rotation_handler(rotate_sender.clone(), shutdown_token.clone()),
        shutdown_token.clone(),
        "RotationHandler",
    ));

    // Spawn health HTTP server (readiness/liveness)
    {
        let addr = opts.health_addr.clone();
        let ready_fn = ready_provider.expect("ready provider");
        task_tracker.spawn(task_completion_handler(
            health_server::run(addr, ready_fn, shutdown_token.clone()),
            shutdown_token.clone(),
            "HealthServer",
        ));
    }

    // Create a BPF loader with the specified verbosity and appropriate buffer size
    let perf_ring_pages = if opts.trace {
        TRACE_PERF_RING_PAGES
    } else {
        TIMESLOT_PERF_RING_PAGES
    };
    let mut sync_timer = SyncTimer::start(SYNC_TIMER_INTERVAL_NS)
        .map_err(|e| anyhow!("failed to start sync timer: {}", e))?;

    let mut bpf_loader = BpfLoader::new(perf_ring_pages, &mut sync_timer)?;

    // Create PerfEventProcessor with the appropriate mode
    let processor = PerfEventProcessor::new(&mut bpf_loader, num_cpus, processor_mode);

    // Spawn error reporting task
    let error_receiver = processor
        .borrow_mut()
        .take_error_receiver()
        .ok_or_else(|| anyhow::anyhow!("Failed to take error receiver from BpfErrorHandler"))?;

    task_tracker.spawn(async move {
        PerfEventProcessor::run_error_reporting(error_receiver).await;
    });

    // Close the tracker after all tasks have been spawned
    task_tracker.close();

    // Attach BPF programs
    bpf_loader.attach()?;

    info!("Collection started.");

    // Run BPF polling in the main thread until signaled to stop
    loop {
        // Check if we should shutdown
        if shutdown_token.is_cancelled() {
            break;
        }

        // Poll for events with a 10ms timeout
        if let Err(e) = bpf_loader.poll_events(10) {
            // Log error directly and cancel shutdown token
            error!("BPF polling error: {}", e);
            shutdown_token.cancel();
            break;
        }

        // Drive the tokio runtime forward
        tokio::task::yield_now().await;
    }

    // Clean up: shutdown the processor
    processor.borrow_mut().shutdown();

    // Clean up: wait for all tasks to complete
    debug!("Waiting for all tasks to complete...");
    task_tracker.wait().await;

    info!("Shutdown complete");
    Ok(())
}
