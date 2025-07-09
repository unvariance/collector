# Pipeline Implementation Guide

This document describes how the collector implements streaming data pipelines using Rust's async ecosystem, focusing on channel communication, error handling, and graceful shutdown patterns.

## Channel Communication Patterns

### Channel Types and Creation

The pipeline creates and configures all channels, transferring them to individual stages rather than having stages create their own channels. The pipeline drops its references to senders so that receiver stages can detect when all senders are closed, and transfers receiver ownership to the consuming stages.

**Bounded channels provide the foundation for backpressure-aware streaming pipelines**. We use bounded channels to prevent memory exhaustion under high load conditions, with the channel itself serving as the buffer mechanism.

### MPSC Channels for Pipeline Connectivity

**MPSC (Multi-Producer, Single-Consumer) channels** are our primary choice for connecting pipeline stages. While we typically use single producers, MPSC is the standard Tokio implementation we rely on. For system measurement data, we typically use a capacity of 1000-10000 messages based on data frequency and processing latency requirements.

### Backpressure Handling

**Backpressure handling becomes critical when processing high-frequency eBPF measurements**. Our strategy focuses on time slot dropping with logging:

- **Drop time slots** when downstream stages cannot keep up
- **Log the number of dropped time slots** and report counts every second
- **Avoid blocking** to ensure forward progress

#### Rate-Limited Console Logging

Since we process thousands of time slots per second, we need rate limiting mechanisms to avoid console spam. The pattern is for each processing loop to maintain its own drop counter and use `tokio::select!` with a timer for periodic logging.

```rust
use tokio::time::{interval, Duration};

async fn processing_loop_with_drop_logging(
    mut input_rx: mpsc::Receiver<TimeslotData>,
    output_tx: mpsc::Sender<ProcessedData>,
) -> Result<(), ProcessingError> {
    let mut drop_count = 0u64;
    let mut log_timer = interval(Duration::from_secs(1));
    
    loop {
        tokio::select! {
            result = input_rx.recv() => {
                match result {
                    Some(timeslot) => {
                        let processed = process_timeslot(timeslot).await?;
                        
                        // Try to send without blocking
                        match output_tx.try_send(processed) {
                            Ok(_) => {
                                // Successfully sent
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // Channel full - drop time slot
                                drop_count += 1;
                                metrics::counter!("timeslots_dropped").increment(1);
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                // Receiver dropped - pipeline shutting down
                                break;
                            }
                        }
                    }
                    None => {
                        // Input channel closed - pipeline shutting down
                        break;
                    }
                }
            }
            
            _ = log_timer.tick() => {
                // Log drops every second
                if drop_count > 0 {
                    log::warn!("Dropped {} time slots in last second", drop_count);
                    drop_count = 0;
                }
            }
        }
    }
    
    Ok(())
}
```

## Error Handling and Supervision

### Error Coordination with Supervision Tasks

**Error channels communicate failures to monitoring and supervision tasks** rather than between processing stages directly. Each pipeline stage receives an error sender to report failures to a centralized monitoring task.

```rust
use tokio_util::sync::CancellationToken;
use tokio::sync::mpsc;

struct PipelineStage {
    error_tx: mpsc::UnboundedSender<PipelineError>,
    shutdown_token: CancellationToken,
}

impl PipelineStage {
    async fn run(&self) -> Result<(), PipelineError> {
        let shutdown_token = self.shutdown_token.clone();
        
        tokio::select! {
            _ = shutdown_token.cancelled() => {
                tracing::info!("Stage shutting down gracefully");
                Ok(())
            }
            result = self.execute_stage_logic() => {
                match result {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        tracing::error!("Stage failed: {:?}", e);
                        self.error_tx.send(e.clone()).ok();
                        Err(e)
                    }
                }
            }
        }
    }
}
```

### Error Handling Strategy

**Errors in our system tend to be fatal**. Rather than implementing complex restart strategies, we:

- Report errors to the supervision task
- Supervision task triggers graceful shutdown with timeout
- Force shutdown if graceful shutdown timeout expires
- Allow the collector pod to restart after shutdown
- Log comprehensive error information for debugging

Unlike signal-based shutdown where we can rely on Kubernetes' grace period, error-triggered shutdowns require their own timeout mechanism since Kubernetes isn't initiating the shutdown.

```rust
use tokio::time::{timeout, Duration};

async fn supervision_task(
    mut error_rx: mpsc::UnboundedReceiver<PipelineError>,
    shutdown_token: CancellationToken,
    graceful_shutdown_timeout: Duration,
) -> Result<(), SupervisionError> {
    while let Some(error) = error_rx.recv().await {
        tracing::error!("Pipeline error received: {:?}", error);
        
        // Trigger graceful shutdown
        tracing::info!("Initiating graceful shutdown due to error");
        shutdown_token.cancel();
        
        // Wait for graceful shutdown with timeout
        match timeout(graceful_shutdown_timeout, wait_for_pipeline_shutdown()).await {
            Ok(_) => {
                tracing::info!("Graceful shutdown completed successfully");
                break;
            }
            Err(_) => {
                tracing::error!("Graceful shutdown timeout expired, forcing shutdown");
                // Force shutdown mechanisms here (e.g., std::process::exit)
                std::process::exit(1);
            }
        }
    }
    
    Ok(())
}
```

This approach attempts graceful shutdown first with a reasonable timeout, then falls back to forced shutdown to prevent hanging on fatal errors.

## Graceful Shutdown Patterns

### Shutdown Coordination Strategy

**Cancellation tokens coordinate shutdown across pipeline stages**. However, stages that only read from channels don't need to monitor the cancellation token directly—they should drain their inputs until no more data is available, then close their outputs and terminate.

**Only stages that don't follow this pattern need explicit shutdown signaling**, such as stages reading from eBPF rings that need to stop reading and perform cleanup.

```rust
use tokio::signal;
use tokio_util::sync::CancellationToken;

async fn channel_draining_stage(
    mut input_rx: mpsc::Receiver<MeasurementData>,
    output_tx: mpsc::Sender<ProcessedData>,
) -> Result<(), ProcessingError> {
    // Drain input until channel closes
    while let Some(measurement) = input_rx.recv().await {
        let processed = process_measurement(measurement).await?;
        
        // If output channel is closed, we're shutting down
        if output_tx.send(processed).await.is_err() {
            break;
        }
    }
    
    // Close output channel to signal downstream stages
    drop(output_tx);
    Ok(())
}

async fn ebpf_reading_stage(
    shutdown_token: CancellationToken,
    output_tx: mpsc::Sender<MeasurementData>,
) -> Result<(), ProcessingError> {
    let mut ebpf_reader = EbpfReader::new().await?;
    
    tokio::select! {
        _ = shutdown_token.cancelled() => {
            tracing::info!("eBPF stage shutting down");
            ebpf_reader.cleanup().await?;
            drop(output_tx);
            Ok(())
        }
        result = ebpf_reader.read_loop(&output_tx) => {
            result
        }
    }
}
```

### Signal Handling

**Signal handling with proper cleanup coordination** ensures data integrity during system shutdown. We use an isolated signal monitor task that listens for shutdown signals and coordinates with the cancellation token.

```rust
use tokio::signal;
use tokio_util::sync::CancellationToken;

struct ProductionPipeline {
    shutdown_token: CancellationToken,
}

impl ProductionPipeline {
    async fn spawn_signal_monitor(&self) {
        let shutdown_token = self.shutdown_token.clone();
        
        tokio::spawn(async move {
            let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate()).unwrap();
            let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt()).unwrap();
            
            tokio::select! {
                _ = sigterm.recv() => {
                    tracing::info!("Received SIGTERM, initiating graceful shutdown");
                    shutdown_token.cancel();
                }
                _ = sigint.recv() => {
                    tracing::info!("Received SIGINT, initiating graceful shutdown");
                    shutdown_token.cancel();
                }
                _ = shutdown_token.cancelled() => {
                    tracing::info!("Shutdown token cancelled, signal monitor terminating");
                }
            }
        });
    }
    
    async fn run_pipeline(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Start signal monitoring
        self.spawn_signal_monitor().await;
        
        // Start pipeline stages
        self.start_all_stages().await?;
        
        // Wait for shutdown signal
        self.shutdown_token.cancelled().await;
        
        // Initiate graceful shutdown
        self.graceful_shutdown().await?;
        
        Ok(())
    }
    
    async fn graceful_shutdown(&self) -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!("Initiating graceful shutdown...");
        
        // Signal shutdown to stages that need explicit notification
        self.shutdown_token.cancel();
        
        // Wait for all stages to complete (no timeout - let Kubernetes handle this)
        self.wait_for_all_stages().await?;
        
        tracing::info!("Graceful shutdown complete");
        Ok(())
    }
}
```

## Key Design Principles

1. **Pipeline creates channels**: Stages receive pre-configured channels rather than creating their own
2. **Bounded channels prevent memory exhaustion**: Use appropriate buffer sizes based on data frequency
3. **Drop time slots under backpressure**: Maintain forward progress with rate-limited logging
4. **Cascade shutdowns through channel closure**: Most stages can shut down by draining inputs
5. **Explicit shutdown signaling only when needed**: Reserve cancellation tokens for stages that require cleanup
6. **Leverage Kubernetes grace periods**: Allow natural shutdown timing rather than imposing artificial timeouts
7. **Isolated signal monitoring**: Use dedicated task for signal handling with cancellation token coordination

This architecture provides robust, high-performance streaming pipelines that handle millions of measurements per second while maintaining operational reliability and clear error reporting.