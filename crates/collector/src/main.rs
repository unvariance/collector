use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use env_logger;
use log::{debug, error, info};
use object_store::ObjectStore;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;
use tokio::time::sleep;
use uuid::Uuid;

// Import the perf_events crate components

// Import the bpf crate components
use bpf::{msg_type, BpfLoader, PerfMeasurementMsg, TaskFreeMsg, TaskMetadataMsg};

// Import local modules
mod bpf_error_handler;
mod bpf_timeslot_tracker;
mod metrics;
mod parquet_writer;
mod parquet_writer_task;
mod task_metadata;
mod timeslot_data;

// Re-export the Metric struct
use bpf_error_handler::BpfErrorHandler;
use bpf_timeslot_tracker::BpfTimeslotTracker;
pub use metrics::Metric;
use parquet_writer::{ParquetWriter, ParquetWriterConfig};
use parquet_writer_task::ParquetWriterTask;
use task_metadata::{TaskCollection, TaskMetadata};
use timeslot_data::TimeslotData;

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
}

// Application state containing task collection and timer tracking
struct PerfEventProcessor {
    task_collection: TaskCollection,
    current_timeslot: TimeslotData,
    // Callback for completed timeslots
    on_timeslot_complete: Box<dyn Fn(TimeslotData)>,
    // BPF timeslot tracker
    _timeslot_tracker: Rc<RefCell<BpfTimeslotTracker>>,
    // BPF error handler
    _error_handler: Rc<RefCell<BpfErrorHandler>>,
}

impl PerfEventProcessor {
    // Create a new PerfEventProcessor with a callback for completed timeslots
    fn new(
        bpf_loader: &mut BpfLoader,
        num_cpus: usize,
        on_timeslot_complete: impl Fn(TimeslotData) + 'static,
    ) -> Rc<RefCell<Self>> {
        // Create BpfTimeslotTracker
        let timeslot_tracker = BpfTimeslotTracker::new(bpf_loader, num_cpus);

        // Create BpfErrorHandler
        let error_handler = BpfErrorHandler::new(bpf_loader);

        let processor = Rc::new(RefCell::new(Self {
            task_collection: TaskCollection::new(),
            current_timeslot: TimeslotData::new(0), // Start with timestamp 0
            on_timeslot_complete: Box::new(on_timeslot_complete),
            _timeslot_tracker: timeslot_tracker.clone(),
            _error_handler: error_handler,
        }));

        // Set up timeslot event subscriptions
        {
            let processor_clone = processor.clone();
            timeslot_tracker
                .borrow_mut()
                .subscribe(move |old_timeslot, new_timeslot| {
                    processor_clone
                        .borrow_mut()
                        .handle_new_timeslot(old_timeslot, new_timeslot);
                });

            let processor_clone = processor.clone();
            timeslot_tracker
                .borrow_mut()
                .subscribe(move |old_timeslot, new_timeslot| {
                    processor_clone
                        .borrow_mut()
                        .handle_task_collection_flush(old_timeslot, new_timeslot);
                });
        }

        // Set up BPF event subscriptions
        {
            let dispatcher = bpf_loader.dispatcher_mut();

            // Helper function to create subscription closures
            let mut subscribe_handler =
                |msg_type: u32, handler: fn(&mut PerfEventProcessor, usize, &[u8])| {
                    let processor_clone = processor.clone();
                    dispatcher.subscribe(msg_type, move |ring_index, data| {
                        handler(&mut processor_clone.borrow_mut(), ring_index, data);
                    });
                };

            // Register handlers for each message type
            subscribe_handler(
                msg_type::MSG_TYPE_TASK_METADATA as u32,
                PerfEventProcessor::handle_task_metadata,
            );
            subscribe_handler(
                msg_type::MSG_TYPE_TASK_FREE as u32,
                PerfEventProcessor::handle_task_free,
            );
            subscribe_handler(
                msg_type::MSG_TYPE_PERF_MEASUREMENT as u32,
                PerfEventProcessor::handle_perf_measurement,
            );
        }

        processor
    }

    // Handle task metadata events
    fn handle_task_metadata(&mut self, _ring_index: usize, data: &[u8]) {
        let event: &TaskMetadataMsg = match plain::from_bytes(data) {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to parse task metadata event: {:?}", e);
                return;
            }
        };

        // Create task metadata and add to collection
        let metadata = TaskMetadata::new(event.pid, event.comm, event.cgroup_id);
        self.task_collection.add(metadata);
    }

    // Handle task free events
    fn handle_task_free(&mut self, _ring_index: usize, data: &[u8]) {
        let event: &TaskFreeMsg = match plain::from_bytes(data) {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to parse task free event: {:?}", e);
                return;
            }
        };

        // Queue the task for removal
        self.task_collection.queue_removal(event.pid);
    }

    // Handle performance measurement events
    fn handle_perf_measurement(&mut self, _ring_index: usize, data: &[u8]) {
        let event: &PerfMeasurementMsg = match plain::from_bytes(data) {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to parse perf measurement event: {:?}", e);
                return;
            }
        };

        // Create metric from the performance measurements
        let metric = Metric::from_deltas(
            event.cycles_delta,
            event.instructions_delta,
            event.llc_misses_delta,
            event.cache_references_delta,
            event.time_delta_ns,
        );

        // Look up task metadata and update timeslot data
        let pid = event.pid;
        let metadata = self.task_collection.lookup(pid).cloned();
        self.current_timeslot.update(pid, metadata, metric);
    }

    // Handle new timeslot events
    fn handle_new_timeslot(&mut self, _old_timeslot: u64, new_timeslot: u64) {
        // Create a new empty timeslot with the new timestamp
        let new_timeslot_data = TimeslotData::new(new_timeslot);

        // Take ownership of the current timeslot, replacing it with the new one
        let completed_timeslot = std::mem::replace(&mut self.current_timeslot, new_timeslot_data);

        // Call the callback with the completed timeslot
        (self.on_timeslot_complete)(completed_timeslot);
    }

    // Handle task collection flush for new time slots
    fn handle_task_collection_flush(&mut self, _old_timeslot: u64, _new_timeslot: u64) {
        // End of time slot - flush queued removals
        self.task_collection.flush_removals();
    }
}

// Create object store based on storage type
fn create_object_storage(storage_type: &str) -> Result<Arc<dyn ObjectStore>> {
    match storage_type.to_lowercase().as_str() {
        "s3" => {
            debug!("Creating S3 object store from environment variables");
            let s3 = object_store::aws::AmazonS3Builder::from_env().build()?;
            Ok(Arc::new(s3))
        }
        "local" | _ => {
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

fn main() -> Result<()> {
    // Initialize env_logger
    env_logger::init();

    let opts = Command::parse();

    debug!("Starting collector with options: {:?}", opts);

    // Initialize tokio runtime for async operations
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Get node identity for file path
    let node_id = get_node_identity();

    // Create object store based on storage type
    let store = create_object_storage(&opts.storage_type)?;

    // Compose storage prefix with node identity
    let storage_prefix = format!("{}{}", opts.prefix, node_id);

    // Create ParquetWriterConfig with the storage prefix
    let config = ParquetWriterConfig {
        storage_prefix,
        buffer_size: opts.parquet_buffer_size,
        file_size_limit: opts.parquet_file_size,
        max_row_group_size: opts.max_row_group_size,
        storage_quota: opts.storage_quota,
    };

    // Create the ParquetWriter with the store and config
    debug!(
        "Writing metrics to {} storage with prefix: {}",
        &opts.storage_type, &config.storage_prefix
    );
    let writer = ParquetWriter::new(store, config)?;

    // Create ParquetWriterTask with a buffer of 1000 items
    let mut writer_task = runtime.block_on(async { ParquetWriterTask::new(writer, 1000) });

    debug!("Parquet writer task initialized and ready to receive data");

    // Get sender from the writer task
    let object_writer_sender = writer_task.sender();

    // Create a BPF loader with the specified verbosity
    let mut bpf_loader = BpfLoader::new()?;

    // Initialize the sync timer
    bpf_loader.start_sync_timer()?;

    // Determine the number of available CPUs
    let num_cpus = libbpf_rs::num_possible_cpus()?;

    // Track errors for batched reporting
    let error_counter = Rc::new(RefCell::new(0u64));
    let last_error_report = Rc::new(RefCell::new(std::time::Instant::now()));

    // Create callback for handling completed timeslots
    let timeslot_callback = {
        let error_counter = error_counter.clone();
        let last_error_report = last_error_report.clone();

        move |timeslot: TimeslotData| {
            if let Err(_) = object_writer_sender.try_send(timeslot) {
                // Increment error count instead of printing immediately
                *error_counter.borrow_mut() += 1;

                // Check if it's time to report errors (every 1 second)
                let now = std::time::Instant::now();
                let mut last_report = last_error_report.borrow_mut();
                if now.duration_since(*last_report).as_secs() >= 1 {
                    // Report accumulated errors
                    if *error_counter.borrow() > 0 {
                        error!("Error sending timeslots to object writer: {} errors in the last 1 seconds", *error_counter.borrow());
                        *error_counter.borrow_mut() = 0;
                    }
                    *last_report = now;
                }
            }
        }
    };

    // Create PerfEventProcessor with the callback and BPF loader
    let _processor = PerfEventProcessor::new(&mut bpf_loader, num_cpus, timeslot_callback);

    // Attach BPF programs
    bpf_loader.attach()?;

    info!("Successfully started! Tracing and aggregating task performance...");

    // Create a channel for BPF error communication and shutdown signaling
    let (bpf_error_tx, mut bpf_error_rx) = oneshot::channel();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    // Spawn monitoring task to watch for signals and timeout
    let monitoring_handle = runtime.spawn(async move {
        let duration = Duration::from_secs(opts.duration);
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigusr1 = signal(SignalKind::user_defined1())?;

        // Run until we receive a signal to terminate
        loop {
            // Select between different completion scenarios
            tokio::select! {
                // Duration timeout (if specified)
                _ = async {
                    if duration.as_secs() > 0 {
                        sleep(duration).await;
                        true
                    } else {
                        // This future never completes for unlimited duration
                        std::future::pending::<bool>().await
                    }
                } => {
                    debug!("Duration timeout reached");
                    break;
                },

                // SIGTERM received
                _ = sigterm.recv() => {
                    debug!("Received SIGTERM");
                    break;
                },

                // SIGINT received
                _ = sigint.recv() => {
                    debug!("Received SIGINT");
                    break;
                },

                // SIGUSR1 received - trigger file rotation
                _ = sigusr1.recv() => {
                    debug!("Received SIGUSR1, rotating parquet file");
                    if let Err(e) = writer_task.rotate().await {
                        error!("Failed to rotate parquet file: {}", e);
                    }
                    // Continue running, don't break
                },

                // BPF polling error
                error = &mut bpf_error_rx => {
                    match error {
                        Ok(error_msg) => {
                            error!("{}", error_msg);
                        },
                        Err(_) => {
                            error!("BPF polling channel closed unexpectedly");
                        }
                    }
                    break;
                },

                // Parquet writer task completed
                result = writer_task.join_handle() => {
                    let shutdown_reason = match result {
                        Ok(Ok(_)) => "Writer task returned unexpectedly",
                        Ok(Err(e)) => {
                            error!("Writer task error: {}", e);
                            "Writer task failed with error"
                        },
                        Err(e) => {
                            error!("Writer task panicked: {}", e);
                            "Writer task panicked"
                        }
                    };
                    let _ = shutdown_tx.send(());
                    return Result::<_>::Err(anyhow::anyhow!("{}", shutdown_reason));
                }
            };
        }

        debug!("Shutting down...");

        // Signal the main thread to shutdown BPF polling
        let _ = shutdown_tx.send(());

        debug!("Waiting for writer task to complete...");
        let writer_task_result = writer_task.shutdown().await;
        if let Err(e) = writer_task_result {
            error!("Writer task error: {}", e);
            return Result::<_>::Err(anyhow::anyhow!("Writer task error: {}", e));
        }

        Result::<_>::Ok(())
    });

    // Run BPF polling in the main thread until signaled to stop
    loop {
        // Check if we should shutdown
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        // Poll for events with a 10ms timeout
        if let Err(e) = bpf_loader.poll_events(10) {
            // Send error to the monitoring task
            let _ = bpf_error_tx.send(format!("BPF polling error: {}", e));
            break;
        }

        // Drive the tokio runtime forward
        runtime.block_on(async {
            tokio::task::yield_now().await;
        });
    }

    // Clean up: wait for monitoring task to complete
    if let Err(e) = runtime.block_on(monitoring_handle) {
        error!("Error in monitoring task: {:?}", e);
    }

    info!("Shutdown complete");
    Ok(())
}
