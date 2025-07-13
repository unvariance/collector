use anyhow::{Context, Result};
use arrow_array::{Array, ArrayRef, Float64Array, RecordBatch};
use arrow_schema::{DataType, Field};
use std::collections::HashMap;
use std::sync::Arc;

use crate::analyzer::Analysis;

/// CPU time counter for tracking aggregate CPU time and thread counts
#[derive(Debug, Clone)]
pub struct CpuTimeCounter {
    aggregate_cpu_time: u64,
    last_update_timestamp: u64,
    pub current_thread_count: u32,
}

impl CpuTimeCounter {
    /// Create a new CPU time counter
    pub fn new() -> Self {
        Self {
            aggregate_cpu_time: 0,
            last_update_timestamp: 0,
            current_thread_count: 0,
        }
    }

    /// Increment the current thread count
    pub fn increase(&mut self) {
        self.current_thread_count += 1;
    }

    /// Decrement the current thread count
    pub fn decrease(&mut self) {
        if self.current_thread_count == 0 {
            panic!("Current thread count is 0");
        }
        self.current_thread_count -= 1;
    }

    /// Update aggregate CPU time with elapsed time since last update
    pub fn update(&mut self, timestamp: u64) {
        if self.last_update_timestamp > 0 {
            if timestamp < self.last_update_timestamp {
                panic!(
                    "Timestamp regression detected: {} < {}",
                    timestamp, self.last_update_timestamp
                );
            }
            let elapsed_time = timestamp - self.last_update_timestamp;
            self.aggregate_cpu_time += elapsed_time * (self.current_thread_count as u64);
        }
        self.last_update_timestamp = timestamp;
    }

    /// Get current aggregate CPU time in nanoseconds
    pub fn get_ns(&self) -> u64 {
        self.aggregate_cpu_time
    }
}

/// Per-CPU state for storing aggregate CPU time readings
#[derive(Debug)]
struct PerCpuState {
    last_timestamp: u64,
    start_total_cpu_time: u64,
    start_same_process_cpu_time: u64,
    context_switch_count: u64,
}

impl PerCpuState {
    fn new() -> Self {
        Self {
            last_timestamp: 0,
            start_total_cpu_time: 0,
            start_same_process_cpu_time: 0,
            context_switch_count: 0,
        }
    }
}

/// Main concurrency analysis processor
pub struct ConcurrencyAnalysis {
    num_cpus: usize,

    // State tracking
    per_pid_counters: HashMap<u32, CpuTimeCounter>,
    total_counter: CpuTimeCounter,
    per_cpu_state: Vec<PerCpuState>,
}

impl ConcurrencyAnalysis {
    /// Create a new concurrency analysis processor
    pub fn new(num_cpus: usize) -> Result<Self> {
        Ok(Self {
            num_cpus,
            per_pid_counters: HashMap::new(),
            total_counter: CpuTimeCounter::new(),
            per_cpu_state: (0..num_cpus).map(|_| PerCpuState::new()).collect(),
        })
    }

    /// Check if a process ID represents a kernel thread
    fn is_kernel(pid: u32) -> bool {
        pid == 0
    }

    /// Process a single event
    fn process_event(
        &mut self,
        timestamp: u64,
        pid: u32,
        cpu_id: usize,
        is_context_switch: bool,
        next_tgid: Option<u32>,
    ) -> Result<(f64, f64)> {
        // Get or create current PID counter entry
        let current_pid_counter = self
            .per_pid_counters
            .entry(pid)
            .or_insert_with(CpuTimeCounter::new);

        // Get current aggregate readings before updates
        let start_total_cpu_time = self.per_cpu_state[cpu_id].start_total_cpu_time;
        let start_same_process_cpu_time = self.per_cpu_state[cpu_id].start_same_process_cpu_time;
        let last_cpu_timestamp = self.per_cpu_state[cpu_id].last_timestamp;

        // Update counters to current timestamp
        self.total_counter.update(timestamp);
        current_pid_counter.update(timestamp);

        // Get current aggregate readings after updates
        let end_total_cpu_time = self.total_counter.get_ns();
        let end_same_process_cpu_time = current_pid_counter.get_ns();

        // Handle context switches - only increment/decrement counters on context switches
        if is_context_switch {
            let next_pid =
                next_tgid.expect("next_tgid should always be present on context switches");

            // Identify kernel threads for counter management
            let is_kernel = Self::is_kernel(pid);
            let context_switch_count = self.per_cpu_state[cpu_id].context_switch_count;

            // Only decrease counters if we've seen a context switch before on this CPU
            if context_switch_count > 0 {
                // Decrease counter for outgoing process
                current_pid_counter.decrease();
                if !is_kernel {
                    self.total_counter.decrease();
                }
            }

            // Update next PID counter to timestamp before increasing it
            let next_is_kernel = Self::is_kernel(next_pid);
            let next_pid_counter = self
                .per_pid_counters
                .entry(next_pid)
                .or_insert_with(CpuTimeCounter::new);
            next_pid_counter.update(timestamp);
            next_pid_counter.increase();
            if !next_is_kernel {
                self.total_counter.increase();
            }

            // Increment context switch count for this CPU
            self.per_cpu_state[cpu_id].context_switch_count += 1;
        }

        // Calculate average concurrent threads only if we have a previous timestamp
        let time_interval = if last_cpu_timestamp > 0 {
            timestamp - last_cpu_timestamp
        } else {
            0
        };

        let avg_total_threads = if time_interval > 0 {
            (end_total_cpu_time - start_total_cpu_time) as f64 / time_interval as f64
        } else {
            0.0
        };

        let avg_same_process_threads = if time_interval > 0 {
            (end_same_process_cpu_time - start_same_process_cpu_time) as f64 / time_interval as f64
        } else {
            0.0
        };

        let next_tgid_same_process_cpu_time = if let Some(next_tgid) = next_tgid {
            self.per_pid_counters
                .get(&next_tgid)
                .expect("on context switch, we add a counter for next_tgid if it doesn't exist)")
                .get_ns()
        } else {
            end_same_process_cpu_time
        };

        // Update per-CPU state for next interval
        self.per_cpu_state[cpu_id].start_total_cpu_time = end_total_cpu_time;
        self.per_cpu_state[cpu_id].start_same_process_cpu_time = next_tgid_same_process_cpu_time;
        self.per_cpu_state[cpu_id].last_timestamp = timestamp;

        // Return computed concurrency metrics
        Ok((avg_total_threads, avg_same_process_threads))
    }
}

impl Analysis for ConcurrencyAnalysis {
    fn process_record_batch(&mut self, batch: &RecordBatch) -> Result<Vec<ArrayRef>> {
        let num_rows = batch.num_rows();

        // Extract required columns
        let timestamp_array = batch
            .column_by_name("timestamp")
            .context("Missing timestamp column")?
            .as_any()
            .downcast_ref::<arrow_array::Int64Array>()
            .context("Invalid timestamp column type")?;
        let pid_array = batch
            .column_by_name("pid")
            .context("Missing pid column")?
            .as_any()
            .downcast_ref::<arrow_array::Int32Array>()
            .context("Invalid pid column type")?;
        let cpu_id_array = batch
            .column_by_name("cpu_id")
            .context("Missing cpu_id column")?
            .as_any()
            .downcast_ref::<arrow_array::Int32Array>()
            .context("Invalid cpu_id column type")?;
        let is_context_switch_array = batch
            .column_by_name("is_context_switch")
            .context("Missing is_context_switch column")?
            .as_any()
            .downcast_ref::<arrow_array::BooleanArray>()
            .context("Invalid is_context_switch column type")?;
        let next_tgid_array = batch
            .column_by_name("next_tgid")
            .context("Missing next_tgid column")?
            .as_any()
            .downcast_ref::<arrow_array::Int32Array>()
            .context("Invalid next_tgid column type")?;

        // Prepare output arrays for concurrency metrics
        let mut avg_total_threads = Vec::with_capacity(num_rows);
        let mut avg_same_process_threads = Vec::with_capacity(num_rows);

        // Process each row
        for i in 0..num_rows {
            let timestamp = timestamp_array.value(i) as u64;
            let pid = pid_array.value(i) as u32;
            let cpu_id = cpu_id_array.value(i) as usize;
            let is_context_switch = is_context_switch_array.value(i);
            let next_tgid = if next_tgid_array.is_null(i) {
                None
            } else {
                Some(next_tgid_array.value(i) as u32)
            };

            if cpu_id >= self.num_cpus {
                return Err(anyhow::anyhow!("Invalid CPU ID: {}", cpu_id));
            }

            let (avg_total, avg_same_process) =
                self.process_event(timestamp, pid, cpu_id, is_context_switch, next_tgid)?;

            avg_total_threads.push(avg_total);
            avg_same_process_threads.push(avg_same_process);
        }

        // Return new columns as ArrayRef
        Ok(vec![
            Arc::new(Float64Array::from(avg_total_threads)),
            Arc::new(Float64Array::from(avg_same_process_threads)),
        ])
    }

    fn new_columns_schema(&self) -> Vec<Arc<Field>> {
        vec![
            Arc::new(Field::new("avg_total_threads", DataType::Float64, false)),
            Arc::new(Field::new(
                "avg_same_process_threads",
                DataType::Float64,
                false,
            )),
        ]
    }
}
