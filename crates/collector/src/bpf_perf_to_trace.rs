use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use arrow_array::builder::{Int32Builder, Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use log::error;
use tokio::sync::mpsc;

use bpf::{msg_type, BpfLoader, PerfMeasurementMsg};
use plain;

use crate::bpf_task_tracker::BpfTaskTracker;

/// Create the schema for trace record batches
pub fn create_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::Int64, false),
        Field::new("pid", DataType::Int32, false),
        Field::new("process_name", DataType::Utf8, true),
        Field::new("cgroup_id", DataType::Int64, false),
        Field::new("cycles_delta", DataType::Int64, false),
        Field::new("instructions_delta", DataType::Int64, false),
        Field::new("llc_misses_delta", DataType::Int64, false),
        Field::new("cache_references_delta", DataType::Int64, false),
        Field::new("time_delta_ns", DataType::Int64, false),
    ]))
}

/// Handles BPF performance measurements and outputs individual trace events
pub struct BpfPerfToTrace {
    // Schema for trace records
    schema: SchemaRef,
    // Array builders for each column
    timestamp_builder: Int64Builder,
    pid_builder: Int32Builder,
    process_name_builder: StringBuilder,
    cgroup_id_builder: Int64Builder,
    cycles_builder: Int64Builder,
    instructions_builder: Int64Builder,
    llc_misses_builder: Int64Builder,
    cache_references_builder: Int64Builder,
    time_delta_builder: Int64Builder,
    // Channel for sending completed record batches
    batch_tx: Option<mpsc::Sender<RecordBatch>>,
    // Task tracker for metadata lookup
    task_tracker: Rc<RefCell<BpfTaskTracker>>,
    // Timing for periodic flushes
    last_flush: Instant,
    // Capacity tracking
    capacity: usize,
    current_rows: usize,
}

impl BpfPerfToTrace {
    /// Create a new BpfPerfToTrace processor
    pub fn new(
        bpf_loader: &mut BpfLoader,
        task_tracker: Rc<RefCell<BpfTaskTracker>>,
        batch_tx: mpsc::Sender<RecordBatch>,
        capacity: usize,
    ) -> Rc<RefCell<Self>> {
        let schema = create_schema();

        let processor = Rc::new(RefCell::new(Self {
            schema: schema.clone(),
            timestamp_builder: Int64Builder::with_capacity(capacity),
            pid_builder: Int32Builder::with_capacity(capacity),
            process_name_builder: StringBuilder::with_capacity(capacity, capacity * 16),
            cgroup_id_builder: Int64Builder::with_capacity(capacity),
            cycles_builder: Int64Builder::with_capacity(capacity),
            instructions_builder: Int64Builder::with_capacity(capacity),
            llc_misses_builder: Int64Builder::with_capacity(capacity),
            cache_references_builder: Int64Builder::with_capacity(capacity),
            time_delta_builder: Int64Builder::with_capacity(capacity),
            batch_tx: Some(batch_tx),
            task_tracker,
            last_flush: Instant::now(),
            capacity,
            current_rows: 0,
        }));

        // Set up BPF event subscriptions
        {
            let dispatcher = bpf_loader.dispatcher_mut();

            dispatcher.subscribe_method(
                msg_type::MSG_TYPE_PERF_MEASUREMENT as u32,
                processor.clone(),
                BpfPerfToTrace::handle_perf_measurement,
            );
        }

        processor
    }

    /// Handle performance measurement events
    fn handle_perf_measurement(&mut self, _ring_index: usize, data: &[u8]) {
        let event: &PerfMeasurementMsg = match plain::from_bytes(data) {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to parse perf measurement event: {:?}", e);
                return;
            }
        };

        // Add event data to builders
        self.timestamp_builder
            .append_value(event.header.timestamp as i64);
        self.pid_builder.append_value(event.pid as i32);

        // Look up task metadata for process name and cgroup_id
        if let Some(metadata) = self.task_tracker.borrow().lookup(event.pid) {
            // Convert bytes to string, trimming null bytes
            let comm = std::str::from_utf8(&metadata.comm)
                .unwrap_or("<invalid utf8>")
                .trim_end_matches(char::from(0))
                .to_string();
            self.process_name_builder.append_value(comm);
            self.cgroup_id_builder
                .append_value(metadata.cgroup_id as i64);
        } else {
            self.process_name_builder.append_null();
            self.cgroup_id_builder.append_value(0); // Default value when no metadata available
        }

        // Add performance counter deltas
        self.cycles_builder.append_value(event.cycles_delta as i64);
        self.instructions_builder
            .append_value(event.instructions_delta as i64);
        self.llc_misses_builder
            .append_value(event.llc_misses_delta as i64);
        self.cache_references_builder
            .append_value(event.cache_references_delta as i64);
        self.time_delta_builder
            .append_value(event.time_delta_ns as i64);

        self.current_rows += 1;

        // Check if we should flush
        let should_flush_capacity = self.current_rows >= self.capacity;
        let should_flush_time = self.last_flush.elapsed().as_secs() >= 1;

        if should_flush_capacity || should_flush_time {
            if let Err(e) = self.flush_batch() {
                error!("Failed to flush trace batch: {}", e);
            }
        }
    }

    /// Flush current batch and send it
    fn flush_batch(&mut self) -> Result<()> {
        if self.current_rows == 0 {
            return Ok(()); // Nothing to flush
        }

        // Finish building arrays
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(self.timestamp_builder.finish()),
            Arc::new(self.pid_builder.finish()),
            Arc::new(self.process_name_builder.finish()),
            Arc::new(self.cgroup_id_builder.finish()),
            Arc::new(self.cycles_builder.finish()),
            Arc::new(self.instructions_builder.finish()),
            Arc::new(self.llc_misses_builder.finish()),
            Arc::new(self.cache_references_builder.finish()),
            Arc::new(self.time_delta_builder.finish()),
        ];

        // Create record batch
        let batch = RecordBatch::try_new(self.schema.clone(), arrays)
            .map_err(|e| anyhow!("Failed to create trace RecordBatch: {}", e))?;

        // Send the batch
        if let Some(ref sender) = self.batch_tx {
            if let Err(_) = sender.try_send(batch) {
                error!("Failed to send trace batch: channel full or closed");
            }
        }

        // Reset builders and counters
        self.timestamp_builder = Int64Builder::with_capacity(self.capacity);
        self.pid_builder = Int32Builder::with_capacity(self.capacity);
        self.process_name_builder = StringBuilder::with_capacity(self.capacity, self.capacity * 16);
        self.cgroup_id_builder = Int64Builder::with_capacity(self.capacity);
        self.cycles_builder = Int64Builder::with_capacity(self.capacity);
        self.instructions_builder = Int64Builder::with_capacity(self.capacity);
        self.llc_misses_builder = Int64Builder::with_capacity(self.capacity);
        self.cache_references_builder = Int64Builder::with_capacity(self.capacity);
        self.time_delta_builder = Int64Builder::with_capacity(self.capacity);
        self.current_rows = 0;
        self.last_flush = Instant::now();

        Ok(())
    }

    /// Shutdown the processor and close the batch channel
    pub fn shutdown(&mut self) {
        // Flush any remaining data
        if let Err(e) = self.flush_batch() {
            error!("Failed to flush final trace batch during shutdown: {}", e);
        }

        // Extract and drop the sender to close the channel
        if let Some(sender) = self.batch_tx.take() {
            drop(sender);
        }
    }
}
