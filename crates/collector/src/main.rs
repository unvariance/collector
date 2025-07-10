use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use anyhow::Result;
use clap::Parser;
use env_logger;
use log::{debug, error, info};
use object_store::ObjectStore;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

// Import the perf_events crate components

// Import the bpf crate components
use bpf::BpfLoader;

// Import local modules
mod bpf_error_handler;
mod bpf_task_tracker;
mod bpf_timeslot_tracker;
mod metrics;
mod parquet_writer;
mod parquet_writer_task;
mod perf_event_processor;
mod task_completion_handler;
mod task_metadata;
mod timeslot_data;

use parquet_writer::{ParquetWriter, ParquetWriterConfig};
use parquet_writer_task::ParquetWriterTask;
use perf_event_processor::PerfEventProcessor;
use task_completion_handler::task_completion_handler;
use timeslot_data::TimeslotData;

/// Duration timeout handler - exits when duration completes or cancellation token is triggered
async fn duration_timeout_handler(
    duration: Duration,
    cancellation_token: CancellationToken,
) -> Result<()> {
    if duration.as_secs() == 0 {
        // Unlimited duration - just wait for cancellation
        cancellation_token.cancelled().await;
        debug!("Duration timeout handler cancelled (unlimited duration)");
    } else {
        // Wait for either duration timeout or cancellation
        tokio::select! {
            _ = tokio::time::sleep(duration) => {
                debug!("Duration timeout reached");
            }
            _ = cancellation_token.cancelled() => {
                debug!("Duration timeout handler cancelled");
            }
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

    // Create shutdown token and task tracker
    let shutdown_token = CancellationToken::new();
    let task_tracker = TaskTracker::new();

    // Create ParquetWriterTask with pre-configured channels
    let writer_task = ParquetWriterTask::new(writer, timeslot_receiver, rotate_receiver);

    // Spawn the writer task with completion handler using task tracker
    task_tracker.spawn(task_completion_handler(
        writer_task.run(),
        shutdown_token.clone(),
        "ParquetWriterTask",
    ));

    // Spawn duration timeout handler
    let duration = Duration::from_secs(opts.duration);
    task_tracker.spawn(task_completion_handler(
        duration_timeout_handler(duration, shutdown_token.clone()),
        shutdown_token.clone(),
        "DurationTimeoutHandler",
    ));

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

    // Close the tracker since we've added all tasks
    task_tracker.close();

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
    _processor.borrow_mut().shutdown();

    // Clean up: wait for all tasks to complete
    debug!("Waiting for all tasks to complete...");
    task_tracker.wait().await;

    info!("Shutdown complete");
    Ok(())
}
