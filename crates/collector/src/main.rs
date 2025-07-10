use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use anyhow::Result;
use clap::Parser;
use env_logger;
use log::{debug, error, info};
use object_store::ObjectStore;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// Import the perf_events crate components

// Import the bpf crate components
use bpf::{msg_type, BpfLoader, PerfMeasurementMsg};

// Import local modules
mod bpf_error_handler;
mod bpf_task_tracker;
mod bpf_timeslot_tracker;
mod metrics;
mod parquet_writer;
mod parquet_writer_task;
mod task_metadata;
mod timeslot_data;

// Re-export the Metric struct
use bpf_error_handler::BpfErrorHandler;
use bpf_task_tracker::BpfTaskTracker;
use bpf_timeslot_tracker::BpfTimeslotTracker;
pub use metrics::Metric;
use parquet_writer::{ParquetWriter, ParquetWriterConfig};
use parquet_writer_task::ParquetWriterTask;
use timeslot_data::TimeslotData;

/// Completion wrapper that handles errors, successful exits, and panics
/// Cancels the token when the task completes for any reason
async fn completion_wrapper<F, T, E>(future: F, token: CancellationToken, task_name: &str)
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static + std::fmt::Debug,
{
    let handle = tokio::spawn(future);

    match handle.await {
        Ok(Ok(_)) => {
            // Task completed successfully
            debug!("{} completed successfully", task_name);
        }
        Ok(Err(error)) => {
            // Task completed but returned an error
            error!("{} failed with error: {:?}", task_name, error);
        }
        Err(join_error) => {
            // Task panicked or was cancelled
            error!("{} panicked or was cancelled: {:?}", task_name, join_error);
        }
    }

    // Always cancel the token when task completes for any reason
    token.cancel();
}

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
    current_timeslot: TimeslotData,
    // Channel for sending completed timeslots
    timeslot_tx: Option<mpsc::Sender<TimeslotData>>,
    // Error tracking for batched reporting
    error_counter: u64,
    last_error_report: std::time::Instant,
    // BPF timeslot tracker
    _timeslot_tracker: Rc<RefCell<BpfTimeslotTracker>>,
    // BPF error handler
    _error_handler: Rc<RefCell<BpfErrorHandler>>,
    // BPF task tracker
    task_tracker: Rc<RefCell<BpfTaskTracker>>,
}

impl PerfEventProcessor {
    // Create a new PerfEventProcessor with a timeslot sender
    fn new(
        bpf_loader: &mut BpfLoader,
        num_cpus: usize,
        timeslot_tx: mpsc::Sender<TimeslotData>,
    ) -> Rc<RefCell<Self>> {
        // Create BpfTimeslotTracker
        let timeslot_tracker = BpfTimeslotTracker::new(bpf_loader, num_cpus);

        // Create BpfErrorHandler
        let error_handler = BpfErrorHandler::new(bpf_loader);

        // Create BpfTaskTracker
        let task_tracker = BpfTaskTracker::new(bpf_loader);

        let processor = Rc::new(RefCell::new(Self {
            current_timeslot: TimeslotData::new(0), // Start with timestamp 0
            timeslot_tx: Some(timeslot_tx),
            error_counter: 0u64,
            last_error_report: std::time::Instant::now(),
            _timeslot_tracker: timeslot_tracker.clone(),
            _error_handler: error_handler,
            task_tracker: task_tracker.clone(),
        }));

        // Set up timeslot event subscription
        {
            let processor_clone = processor.clone();
            let task_tracker_clone = task_tracker.clone();
            timeslot_tracker
                .borrow_mut()
                .subscribe(move |old_timeslot, new_timeslot| {
                    processor_clone
                        .borrow_mut()
                        .on_new_timeslot(old_timeslot, new_timeslot);
                    task_tracker_clone.borrow_mut().flush_removals();
                });
        }

        // Set up BPF event subscriptions
        {
            let dispatcher = bpf_loader.dispatcher_mut();

            dispatcher.subscribe_method(
                msg_type::MSG_TYPE_PERF_MEASUREMENT as u32,
                processor.clone(),
                PerfEventProcessor::handle_perf_measurement,
            );
        }

        processor
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
        let metadata = self.task_tracker.borrow().lookup(pid).cloned();
        self.current_timeslot.update(pid, metadata, metric);
    }

    // Handle new timeslot events
    fn on_new_timeslot(&mut self, _old_timeslot: u64, new_timeslot: u64) {
        // Create a new empty timeslot with the new timestamp
        let new_timeslot_data = TimeslotData::new(new_timeslot);

        // Take ownership of the current timeslot, replacing it with the new one
        let completed_timeslot = std::mem::replace(&mut self.current_timeslot, new_timeslot_data);

        // Try to send the completed timeslot to the writer
        if let Some(ref sender) = self.timeslot_tx {
            if let Err(_) = sender.try_send(completed_timeslot) {
                // Increment error count instead of printing immediately
                self.error_counter += 1;

                // Check if it's time to report errors (every 1 second)
                let now = std::time::Instant::now();
                if now.duration_since(self.last_error_report).as_secs() >= 1 {
                    // Report accumulated errors
                    if self.error_counter > 0 {
                        error!(
                            "Error sending timeslots to object writer: {} errors in the last 1 seconds",
                            self.error_counter
                        );
                        self.error_counter = 0;
                    }
                    self.last_error_report = now;
                }
            }
        }
    }

    // Shutdown the processor and close the timeslot channel
    pub fn shutdown(&mut self) {
        // Extract and drop the sender to close the channel
        if let Some(sender) = self.timeslot_tx.take() {
            drop(sender);
        }
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

    // Create channels for the ParquetWriterTask
    let (timeslot_sender, timeslot_receiver) = mpsc::channel::<TimeslotData>(1000);
    let (rotate_sender, rotate_receiver) = mpsc::channel::<()>(1);

    // Create shutdown token
    let shutdown_token = CancellationToken::new();

    // Create ParquetWriterTask with pre-configured channels
    let writer_task = ParquetWriterTask::new(writer, timeslot_receiver, rotate_receiver);

    // Spawn the writer task with completion wrapper
    let writer_task_handle = runtime.spawn(completion_wrapper(
        writer_task.run(),
        shutdown_token.clone(),
        "ParquetWriterTask",
    ));

    debug!("Parquet writer task initialized and ready to receive data");

    // Create a BPF loader with the specified verbosity
    let mut bpf_loader = BpfLoader::new()?;

    // Initialize the sync timer
    bpf_loader.start_sync_timer()?;

    // Determine the number of available CPUs
    let num_cpus = libbpf_rs::num_possible_cpus()?;

    // Create PerfEventProcessor with the timeslot sender and BPF loader
    let _processor = PerfEventProcessor::new(&mut bpf_loader, num_cpus, timeslot_sender);

    // Attach BPF programs
    bpf_loader.attach()?;

    info!("Successfully started! Tracing and aggregating task performance...");

    // Create a channel for BPF error communication and cancellation token for shutdown signaling
    let (bpf_error_tx, mut bpf_error_rx) = oneshot::channel();
    let shutdown_token_clone = shutdown_token.clone();

    // Spawn monitoring task to watch for signals and timeout
    let monitoring_handle = runtime.spawn(async move {
        let writer_task_handle = writer_task_handle;
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
                    if let Err(e) = rotate_sender.send(()).await {
                        error!("Failed to send rotation signal: {}", e);
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

                // Shutdown token cancelled (by completion wrapper or other failure)
                _ = shutdown_token_clone.cancelled() => {
                    debug!("Shutdown token cancelled");
                    break;
                }
            };
        }

        debug!("Shutting down...");

        // Signal the main thread to shutdown BPF polling
        shutdown_token_clone.cancel();

        debug!("Waiting for writer task to complete...");
        // Writer task completion wrapper handles its own errors and logs them
        let _ = writer_task_handle.await;

        debug!("Monitoring task shutting down...");

        Result::<_>::Ok(())
    });

    // Run BPF polling in the main thread until signaled to stop
    loop {
        // Check if we should shutdown
        if shutdown_token.is_cancelled() {
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

    // Clean up: shutdown the processor
    _processor.borrow_mut().shutdown();

    // Clean up: wait for monitoring task to complete
    if let Err(e) = runtime.block_on(monitoring_handle) {
        error!("Error in monitoring task: {:?}", e);
    }

    info!("Shutdown complete");
    Ok(())
}
