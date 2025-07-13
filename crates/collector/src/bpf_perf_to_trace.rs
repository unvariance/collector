use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use arrow_array::builder::{BooleanBuilder, Int32Builder, Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use log::error;
use tokio::sync::mpsc;

use bpf::{msg_type, BpfLoader, PerfMeasurementMsg};
use plain;

use crate::bpf_task_tracker::BpfTaskTracker;
use crate::bpf_timeslot_tracker::BpfTimeslotTracker;

/// Create the schema for trace record batches
pub fn create_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::Int64, false),
        Field::new("pid", DataType::Int32, false),
        Field::new("process_name", DataType::Utf8, true),
        Field::new("cgroup_id", DataType::Int64, false),
        Field::new("cpu_id", DataType::Int32, false),
        Field::new("cycles", DataType::Int64, false),
        Field::new("instructions", DataType::Int64, false),
        Field::new("llc_misses", DataType::Int64, false),
        Field::new("cache_references", DataType::Int64, false),
        Field::new("is_context_switch", DataType::Boolean, false),
        Field::new("next_tgid", DataType::Int32, true),
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
    cpu_id_builder: Int32Builder,
    cycles_builder: Int64Builder,
    instructions_builder: Int64Builder,
    llc_misses_builder: Int64Builder,
    cache_references_builder: Int64Builder,
    is_context_switch_builder: BooleanBuilder,
    next_tgid_builder: Int32Builder,
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
        timeslot_tracker: Rc<RefCell<BpfTimeslotTracker>>,
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
            cpu_id_builder: Int32Builder::with_capacity(capacity),
            cycles_builder: Int64Builder::with_capacity(capacity),
            instructions_builder: Int64Builder::with_capacity(capacity),
            llc_misses_builder: Int64Builder::with_capacity(capacity),
            cache_references_builder: Int64Builder::with_capacity(capacity),
            is_context_switch_builder: BooleanBuilder::with_capacity(capacity),
            next_tgid_builder: Int32Builder::with_capacity(capacity),
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

        // Subscribe to timeslot events for time-based flushing
        timeslot_tracker
            .borrow_mut()
            .subscribe_method(processor.clone(), BpfPerfToTrace::on_new_timeslot);

        processor
    }

    /// Handle performance measurement events
    fn handle_perf_measurement(&mut self, ring_index: usize, data: &[u8]) {
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

        // Add CPU ID from ring index (ring index corresponds to CPU ID)
        self.cpu_id_builder.append_value(ring_index as i32);

        // Add performance counter deltas
        self.cycles_builder.append_value(event.cycles_delta as i64);
        self.instructions_builder
            .append_value(event.instructions_delta as i64);
        self.llc_misses_builder
            .append_value(event.llc_misses_delta as i64);
        self.cache_references_builder
            .append_value(event.cache_references_delta as i64);

        // Add event type indication from BPF message
        self.is_context_switch_builder
            .append_value(event.is_context_switch != 0);

        // Add next TGID field - only valid for context switch events, null for timer events
        if event.is_context_switch != 0 {
            self.next_tgid_builder.append_value(event.next_tgid as i32);
        } else {
            self.next_tgid_builder.append_null();
        }

        self.current_rows += 1;

        // Check if we should flush
        let should_flush = self.current_rows >= self.capacity;

        if should_flush {
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
            Arc::new(self.cpu_id_builder.finish()),
            Arc::new(self.cycles_builder.finish()),
            Arc::new(self.instructions_builder.finish()),
            Arc::new(self.llc_misses_builder.finish()),
            Arc::new(self.cache_references_builder.finish()),
            Arc::new(self.is_context_switch_builder.finish()),
            Arc::new(self.next_tgid_builder.finish()),
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
        self.cpu_id_builder = Int32Builder::with_capacity(self.capacity);
        self.cycles_builder = Int64Builder::with_capacity(self.capacity);
        self.instructions_builder = Int64Builder::with_capacity(self.capacity);
        self.llc_misses_builder = Int64Builder::with_capacity(self.capacity);
        self.cache_references_builder = Int64Builder::with_capacity(self.capacity);
        self.is_context_switch_builder = BooleanBuilder::with_capacity(self.capacity);
        self.next_tgid_builder = Int32Builder::with_capacity(self.capacity);
        self.current_rows = 0;
        self.last_flush = Instant::now();

        Ok(())
    }

    /// Handle new timeslot events - triggers time-based flush check
    fn on_new_timeslot(&mut self, _old_timeslot: u64, _new_timeslot: u64) {
        // Check if we should flush based on time elapsed
        if self.last_flush.elapsed().as_secs() >= 1 {
            if let Err(e) = self.flush_batch() {
                error!("Failed to flush trace batch on timeslot: {}", e);
            }
        }
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
