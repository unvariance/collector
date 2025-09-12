use std::sync::Arc;

use anyhow::{anyhow, Result};
use arrow_array::builder::{Int32Builder, Int64Builder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use tokio::sync::mpsc;

use crate::timeslot_data::TimeslotData;

/// Create the schema for timeslot record batches
pub fn create_timeslot_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("start_time", DataType::Int64, false),
        Field::new("pid", DataType::Int32, false),
        Field::new("process_name", DataType::Utf8, true),
        Field::new("cgroup_id", DataType::Int64, false),
        Field::new("cycles", DataType::Int64, false),
        Field::new("instructions", DataType::Int64, false),
        Field::new("llc_misses", DataType::Int64, false),
        Field::new("cache_references", DataType::Int64, false),
        Field::new("duration", DataType::Int64, false),
    ]))
}

/// Convert a TimeslotData to an Arrow RecordBatch
pub fn timeslot_to_batch(timeslot: TimeslotData, schema: SchemaRef) -> Result<RecordBatch> {
    // Get the task count to preallocate builders
    let task_count = timeslot.task_count();

    // Create array builders for each column
    let mut start_time_builder = Int64Builder::with_capacity(task_count);
    let mut pid_builder = Int32Builder::with_capacity(task_count);
    // For StringBuilder, we need both item capacity and estimated data capacity
    // Estimate 16 bytes per string for process names
    let mut process_name_builder = StringBuilder::with_capacity(task_count, task_count * 16);
    let mut cgroup_id_builder = Int64Builder::with_capacity(task_count);
    let mut cycles_builder = Int64Builder::with_capacity(task_count);
    let mut instructions_builder = Int64Builder::with_capacity(task_count);
    let mut llc_misses_builder = Int64Builder::with_capacity(task_count);
    let mut cache_references_builder = Int64Builder::with_capacity(task_count);
    let mut duration_builder = Int64Builder::with_capacity(task_count);

    // Convert timeslot data to arrays
    for (pid, task_data) in timeslot.iter_tasks() {
        // Add start timestamp (common for all tasks in this timeslot)
        start_time_builder.append_value(timeslot.start_timestamp as i64);

        // Add PID
        pid_builder.append_value(*pid as i32);

        // Add process name and cgroup_id (from metadata if available)
        if let Some(ref metadata) = task_data.metadata {
            // Convert bytes to string, trimming null bytes
            let comm = std::str::from_utf8(&metadata.comm)
                .unwrap_or("<invalid utf8>")
                .trim_end_matches(char::from(0))
                .to_string();
            process_name_builder.append_value(comm);
            cgroup_id_builder.append_value(metadata.cgroup_id as i64);
        } else {
            process_name_builder.append_null();
            cgroup_id_builder.append_value(0); // Default value when no metadata available
        }

        // Add metrics
        cycles_builder.append_value(task_data.metrics.cycles as i64);
        instructions_builder.append_value(task_data.metrics.instructions as i64);
        llc_misses_builder.append_value(task_data.metrics.llc_misses as i64);
        cache_references_builder.append_value(task_data.metrics.cache_references as i64);
        duration_builder.append_value(task_data.metrics.time_ns as i64);
    }

    // Finish building arrays
    let arrays: Vec<ArrayRef> = vec![
        Arc::new(start_time_builder.finish()),
        Arc::new(pid_builder.finish()),
        Arc::new(process_name_builder.finish()),
        Arc::new(cgroup_id_builder.finish()),
        Arc::new(cycles_builder.finish()),
        Arc::new(instructions_builder.finish()),
        Arc::new(llc_misses_builder.finish()),
        Arc::new(cache_references_builder.finish()),
        Arc::new(duration_builder.finish()),
    ];

    // Create and return the RecordBatch
    RecordBatch::try_new(schema, arrays).map_err(|e| anyhow!("Failed to create RecordBatch: {}", e))
}

/// Worker task for converting timeslots to record batches
pub struct TimeslotToRecordBatchTask {
    timeslot_receiver: mpsc::Receiver<TimeslotData>,
    batch_sender: mpsc::Sender<RecordBatch>,
    schema: SchemaRef,
}

impl TimeslotToRecordBatchTask {
    /// Create a new TimeslotToRecordBatchTask with pre-configured channels
    pub fn new(
        timeslot_receiver: mpsc::Receiver<TimeslotData>,
        batch_sender: mpsc::Sender<RecordBatch>,
    ) -> Self {
        let schema = create_timeslot_schema();
        Self {
            timeslot_receiver,
            batch_sender,
            schema,
        }
    }

    /// Get the schema for the record batches this task produces
    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// Run the task, processing timeslots until the input channel is closed
    pub async fn run(mut self) -> Result<()> {
        loop {
            match self.timeslot_receiver.recv().await {
                Some(timeslot) => {
                    // Convert timeslot to a batch
                    let batch = timeslot_to_batch(timeslot, self.schema.clone())?;

                    // Send the batch to the output channel
                    if self.batch_sender.send(batch).await.is_err() {
                        // Receiver dropped, pipeline shutting down
                        log::debug!("Batch receiver dropped, shutting down conversion task");
                        break;
                    }
                }
                None => {
                    // Input channel closed - pipeline shutting down
                    log::debug!("Timeslot channel closed, shutting down conversion task");
                    break;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metric;
    use crate::task_metadata::TaskMetadata;
    use crate::timeslot_data::TimeslotData;

    #[test]
    fn test_timeslot_to_batch_conversion() {
        // Create a test timeslot
        let mut timeslot = TimeslotData::new(1500000);

        // Create first task with specific values
        let mut comm1 = [0u8; 16];
        let test_name1 = b"proc_one";
        comm1[..test_name1.len()].copy_from_slice(test_name1);
        let metadata1 = Some(TaskMetadata::new(101, comm1, 11111));
        let metrics1 = Metric::from_deltas(1000, 2000, 30, 500, 100000);
        timeslot.update(101, metadata1, metrics1);

        // Create second task with different values
        let mut comm2 = [0u8; 16];
        let test_name2 = b"proc_two";
        comm2[..test_name2.len()].copy_from_slice(test_name2);
        let metadata2 = Some(TaskMetadata::new(202, comm2, 22222));
        let metrics2 = Metric::from_deltas(3000, 4000, 60, 800, 200000);
        timeslot.update(202, metadata2, metrics2);

        // Convert to batch
        let schema = create_timeslot_schema();
        let batch = timeslot_to_batch(timeslot, schema).unwrap();

        // Verify batch structure
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.num_columns(), 9);

        // Verify content - extract arrays and check values (accounting for unordered timeslot iteration)
        use arrow_array::{Int32Array, Int64Array, StringArray};

        let start_time_array = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let pid_array = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        let process_name_array = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let cgroup_id_array = batch
            .column(3)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let cycles_array = batch
            .column(4)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let instructions_array = batch
            .column(5)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let llc_misses_array = batch
            .column(6)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let cache_references_array = batch
            .column(7)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let duration_array = batch
            .column(8)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();

        // Find which row corresponds to which process by process name
        let mut proc_one_row = None;
        let mut proc_two_row = None;

        for i in 0..batch.num_rows() {
            let process_name = process_name_array.value(i);
            if process_name == "proc_one" {
                proc_one_row = Some(i);
            } else if process_name == "proc_two" {
                proc_two_row = Some(i);
            }
        }

        let proc_one_idx = proc_one_row.expect("proc_one not found in batch");
        let proc_two_idx = proc_two_row.expect("proc_two not found in batch");

        // Verify proc_one values
        assert_eq!(start_time_array.value(proc_one_idx), 1500000);
        assert_eq!(pid_array.value(proc_one_idx), 101);
        assert_eq!(cgroup_id_array.value(proc_one_idx), 11111);
        assert_eq!(cycles_array.value(proc_one_idx), 1000);
        assert_eq!(instructions_array.value(proc_one_idx), 2000);
        assert_eq!(llc_misses_array.value(proc_one_idx), 30);
        assert_eq!(cache_references_array.value(proc_one_idx), 500);
        assert_eq!(duration_array.value(proc_one_idx), 100000);

        // Verify proc_two values
        assert_eq!(start_time_array.value(proc_two_idx), 1500000);
        assert_eq!(pid_array.value(proc_two_idx), 202);
        assert_eq!(cgroup_id_array.value(proc_two_idx), 22222);
        assert_eq!(cycles_array.value(proc_two_idx), 3000);
        assert_eq!(instructions_array.value(proc_two_idx), 4000);
        assert_eq!(llc_misses_array.value(proc_two_idx), 60);
        assert_eq!(cache_references_array.value(proc_two_idx), 800);
        assert_eq!(duration_array.value(proc_two_idx), 200000);
    }

    #[tokio::test]
    async fn test_conversion_task() {
        // Create channels
        let (timeslot_sender, timeslot_receiver) = mpsc::channel::<TimeslotData>(10);
        let (batch_sender, mut batch_receiver) = mpsc::channel::<RecordBatch>(10);

        // Create task
        let task = TimeslotToRecordBatchTask::new(timeslot_receiver, batch_sender);
        let schema = task.schema();

        // Start the task
        let task_handle = tokio::spawn(task.run());

        // Send a test timeslot with two tasks
        let mut timeslot = TimeslotData::new(2500000);

        // First task
        let mut comm1 = [0u8; 16];
        let test_name1 = b"task_alpha";
        comm1[..test_name1.len()].copy_from_slice(test_name1);
        let metadata1 = Some(TaskMetadata::new(301, comm1, 33333));
        let metrics1 = Metric::from_deltas(5000, 6000, 90, 1200, 300000);
        timeslot.update(301, metadata1, metrics1);

        // Second task
        let mut comm2 = [0u8; 16];
        let test_name2 = b"task_beta";
        comm2[..test_name2.len()].copy_from_slice(test_name2);
        let metadata2 = Some(TaskMetadata::new(302, comm2, 44444));
        let metrics2 = Metric::from_deltas(7000, 8000, 120, 1600, 400000);
        timeslot.update(302, metadata2, metrics2);

        timeslot_sender.send(timeslot).await.unwrap();

        // Receive the converted batch
        let batch = batch_receiver.recv().await.unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.schema(), schema);

        // Verify content integrity - extract arrays and check values (accounting for unordered timeslot iteration)
        use arrow_array::{Int32Array, Int64Array, StringArray};

        let start_time_array = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let pid_array = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        let process_name_array = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let cgroup_id_array = batch
            .column(3)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let cycles_array = batch
            .column(4)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let instructions_array = batch
            .column(5)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let duration_array = batch
            .column(8)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();

        // Find which row corresponds to which process by process name
        let mut task_alpha_row = None;
        let mut task_beta_row = None;

        for i in 0..batch.num_rows() {
            let process_name = process_name_array.value(i);
            if process_name == "task_alpha" {
                task_alpha_row = Some(i);
            } else if process_name == "task_beta" {
                task_beta_row = Some(i);
            }
        }

        let task_alpha_idx = task_alpha_row.expect("task_alpha not found in batch");
        let task_beta_idx = task_beta_row.expect("task_beta not found in batch");

        // Verify task_alpha values
        assert_eq!(start_time_array.value(task_alpha_idx), 2500000);
        assert_eq!(pid_array.value(task_alpha_idx), 301);
        assert_eq!(cgroup_id_array.value(task_alpha_idx), 33333);
        assert_eq!(cycles_array.value(task_alpha_idx), 5000);
        assert_eq!(instructions_array.value(task_alpha_idx), 6000);
        assert_eq!(duration_array.value(task_alpha_idx), 300000);

        // Verify task_beta values
        assert_eq!(start_time_array.value(task_beta_idx), 2500000);
        assert_eq!(pid_array.value(task_beta_idx), 302);
        assert_eq!(cgroup_id_array.value(task_beta_idx), 44444);
        assert_eq!(cycles_array.value(task_beta_idx), 7000);
        assert_eq!(instructions_array.value(task_beta_idx), 8000);
        assert_eq!(duration_array.value(task_beta_idx), 400000);

        // Close the sender to trigger task shutdown
        drop(timeslot_sender);

        // Wait for task to complete
        task_handle.await.unwrap().unwrap();
    }
}
