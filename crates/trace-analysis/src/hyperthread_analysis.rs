use anyhow::{Context, Result};
use arrow_array::{Array, ArrayRef, BooleanArray, Int32Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
struct CpuState {
    current_pid: Option<i32>,
    last_counter_update: i64,
    ns_peer_same_process: i64,
    ns_peer_different_process: i64,
    ns_peer_kernel: i64,
}

impl CpuState {
    fn new() -> Self {
        Self {
            current_pid: None,
            last_counter_update: 0,
            ns_peer_same_process: 0,
            ns_peer_different_process: 0,
            ns_peer_kernel: 0,
        }
    }

    fn reset_counters(&mut self) {
        self.ns_peer_same_process = 0;
        self.ns_peer_different_process = 0;
        self.ns_peer_kernel = 0;
    }
}

pub struct HyperthreadAnalysis {
    num_cpus: usize,
    cpu_states: Vec<CpuState>,
    output_filename: PathBuf,
}

impl HyperthreadAnalysis {
    pub fn new(num_cpus: usize, output_filename: PathBuf) -> Result<Self> {
        let cpu_states = vec![CpuState::new(); num_cpus];

        Ok(Self {
            num_cpus,
            cpu_states,
            output_filename,
        })
    }

    fn get_hyperthread_peer(&self, cpu_id: usize) -> usize {
        if cpu_id < self.num_cpus / 2 {
            cpu_id + self.num_cpus / 2
        } else {
            cpu_id - self.num_cpus / 2
        }
    }

    fn update_hyperthread(&mut self, cpu_a: usize, cpu_b: usize, event_timestamp: i64) {
        // Only update if we have previous timestamps (skip initial state)
        if self.cpu_states[cpu_a].last_counter_update == 0
            || self.cpu_states[cpu_b].last_counter_update == 0
        {
            self.cpu_states[cpu_a].last_counter_update = event_timestamp;
            self.cpu_states[cpu_b].last_counter_update = event_timestamp;
            return;
        }

        // Skip updates if either CPU has unknown state (None)
        if self.cpu_states[cpu_a].current_pid.is_none()
            || self.cpu_states[cpu_b].current_pid.is_none()
        {
            self.cpu_states[cpu_a].last_counter_update = event_timestamp;
            self.cpu_states[cpu_b].last_counter_update = event_timestamp;
            return;
        }

        let time_since_a = event_timestamp - self.cpu_states[cpu_a].last_counter_update;
        let time_since_b = event_timestamp - self.cpu_states[cpu_b].last_counter_update;

        // Update counters for CPU A based on CPU B's state
        match self.cpu_states[cpu_b].current_pid {
            Some(0) => {
                self.cpu_states[cpu_a].ns_peer_kernel += time_since_a;
            }
            Some(peer_b_pid) => {
                if Some(peer_b_pid) == self.cpu_states[cpu_a].current_pid {
                    self.cpu_states[cpu_a].ns_peer_same_process += time_since_a;
                } else {
                    self.cpu_states[cpu_a].ns_peer_different_process += time_since_a;
                }
            }
            None => unreachable!("None case handled above"),
        }

        // Update counters for CPU B based on CPU A's state
        match self.cpu_states[cpu_a].current_pid {
            Some(0) => {
                self.cpu_states[cpu_b].ns_peer_kernel += time_since_b;
            }
            Some(peer_a_pid) => {
                if Some(peer_a_pid) == self.cpu_states[cpu_b].current_pid {
                    self.cpu_states[cpu_b].ns_peer_same_process += time_since_b;
                } else {
                    self.cpu_states[cpu_b].ns_peer_different_process += time_since_b;
                }
            }
            None => unreachable!("None case handled above"),
        }

        // Update timestamps
        self.cpu_states[cpu_a].last_counter_update = event_timestamp;
        self.cpu_states[cpu_b].last_counter_update = event_timestamp;
    }

    pub fn process_parquet_file(
        &mut self,
        builder: ParquetRecordBatchReaderBuilder<File>,
    ) -> Result<()> {
        let input_schema = builder.schema().clone();
        let mut arrow_reader = builder
            .build()
            .with_context(|| "Failed to build Arrow reader")?;

        // Create output schema with additional hyperthread columns
        let output_schema = self.create_output_schema(&input_schema)?;

        // Create Arrow writer
        let output_file = File::create(&self.output_filename).with_context(|| {
            format!(
                "Failed to create output file: {}",
                self.output_filename.display()
            )
        })?;

        let mut writer = ArrowWriter::try_new(output_file, Arc::new(output_schema.clone()), None)
            .with_context(|| "Failed to create Arrow writer")?;

        // Process record batches
        while let Some(batch) = arrow_reader.next() {
            let batch = batch.with_context(|| "Failed to read record batch")?;
            let augmented_batch = self.process_record_batch(&batch, &output_schema)?;
            writer
                .write(&augmented_batch)
                .with_context(|| "Failed to write augmented batch")?;
        }

        writer.close().with_context(|| "Failed to close writer")?;

        Ok(())
    }

    fn create_output_schema(&self, input_schema: &Schema) -> Result<Schema> {
        let mut fields: Vec<Arc<Field>> = input_schema.fields().iter().cloned().collect();

        // Add hyperthread counter fields
        fields.push(Arc::new(Field::new(
            "ns_peer_same_process",
            DataType::Int64,
            false,
        )));
        fields.push(Arc::new(Field::new(
            "ns_peer_different_process",
            DataType::Int64,
            false,
        )));
        fields.push(Arc::new(Field::new(
            "ns_peer_kernel",
            DataType::Int64,
            false,
        )));

        Ok(Schema::new(fields))
    }

    fn process_record_batch(
        &mut self,
        batch: &RecordBatch,
        output_schema: &Schema,
    ) -> Result<RecordBatch> {
        let num_rows = batch.num_rows();

        // Extract required columns
        let timestamp_col = batch
            .column_by_name("timestamp")
            .ok_or_else(|| anyhow::anyhow!("timestamp column not found"))?
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| anyhow::anyhow!("timestamp column is not Int64Array"))?;

        let cpu_id_col = batch
            .column_by_name("cpu_id")
            .ok_or_else(|| anyhow::anyhow!("cpu_id column not found"))?
            .as_any()
            .downcast_ref::<Int32Array>()
            .ok_or_else(|| anyhow::anyhow!("cpu_id column is not Int32Array"))?;

        let is_context_switch_col = batch
            .column_by_name("is_context_switch")
            .ok_or_else(|| anyhow::anyhow!("is_context_switch column not found"))?
            .as_any()
            .downcast_ref::<BooleanArray>()
            .ok_or_else(|| anyhow::anyhow!("is_context_switch column is not BooleanArray"))?;

        let next_tgid_col = batch
            .column_by_name("next_tgid")
            .ok_or_else(|| anyhow::anyhow!("next_tgid column not found"))?
            .as_any()
            .downcast_ref::<Int32Array>()
            .ok_or_else(|| anyhow::anyhow!("next_tgid column is not Int32Array"))?;

        // Prepare output arrays for hyperthread counters
        let mut ns_peer_same_process = Vec::with_capacity(num_rows);
        let mut ns_peer_different_process = Vec::with_capacity(num_rows);
        let mut ns_peer_kernel = Vec::with_capacity(num_rows);

        // Process each row
        for i in 0..num_rows {
            let timestamp = timestamp_col.value(i);
            let cpu_id = cpu_id_col.value(i) as usize;
            let is_context_switch = is_context_switch_col.value(i);

            if cpu_id >= self.num_cpus {
                return Err(anyhow::anyhow!("Invalid CPU ID: {}", cpu_id));
            }

            let peer_cpu = self.get_hyperthread_peer(cpu_id);

            // Update hyperthread counters
            self.update_hyperthread(cpu_id, peer_cpu, timestamp);

            // Get current counter values
            let same_process = self.cpu_states[cpu_id].ns_peer_same_process;
            let different_process = self.cpu_states[cpu_id].ns_peer_different_process;
            let kernel = self.cpu_states[cpu_id].ns_peer_kernel;

            // Store counter values
            ns_peer_same_process.push(same_process);
            ns_peer_different_process.push(different_process);
            ns_peer_kernel.push(kernel);

            // Update CPU state for context switches
            if is_context_switch {
                if next_tgid_col.is_null(i) {
                    return Err(anyhow::anyhow!(
                        "next_tgid is null for context switch at row {}",
                        i
                    ));
                }
                let next_tgid = next_tgid_col.value(i);
                self.cpu_states[cpu_id].current_pid = Some(next_tgid);
            }

            // Reset counters after recording
            self.cpu_states[cpu_id].reset_counters();
        }

        // Create output arrays
        let mut output_columns: Vec<ArrayRef> = batch.columns().to_vec();
        output_columns.push(Arc::new(Int64Array::from(ns_peer_same_process)));
        output_columns.push(Arc::new(Int64Array::from(ns_peer_different_process)));
        output_columns.push(Arc::new(Int64Array::from(ns_peer_kernel)));

        RecordBatch::try_new(Arc::new(output_schema.clone()), output_columns)
            .with_context(|| "Failed to create output record batch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{BooleanArray, Int32Array, Int64Array};
    use arrow_schema::{DataType, Field, Schema};
    use std::sync::Arc;

    fn create_test_schema() -> Schema {
        Schema::new(vec![
            Arc::new(Field::new("timestamp", DataType::Int64, false)),
            Arc::new(Field::new("cpu_id", DataType::Int32, false)),
            Arc::new(Field::new("is_context_switch", DataType::Boolean, false)),
            Arc::new(Field::new("next_tgid", DataType::Int32, true)), // nullable for non-context-switch events
        ])
    }

    fn create_test_batch(
        timestamps: Vec<i64>,
        cpu_ids: Vec<i32>,
        is_context_switches: Vec<bool>,
        next_tgids: Vec<Option<i32>>,
    ) -> RecordBatch {
        let schema = create_test_schema();

        let timestamp_array = Arc::new(Int64Array::from(timestamps));
        let cpu_id_array = Arc::new(Int32Array::from(cpu_ids));
        let is_context_switch_array = Arc::new(BooleanArray::from(is_context_switches));
        let next_tgid_array = Arc::new(Int32Array::from(next_tgids));

        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                timestamp_array,
                cpu_id_array,
                is_context_switch_array,
                next_tgid_array,
            ],
        )
        .unwrap()
    }

    #[test]
    fn test_initial_state_produces_zero_counters() {
        let mut analysis = HyperthreadAnalysis::new(4, PathBuf::from("/tmp/test.parquet")).unwrap();
        let input_schema = create_test_schema();
        let output_schema = analysis.create_output_schema(&input_schema).unwrap();

        // First event on CPU 0 - should have zero counters since no prior state
        let batch = create_test_batch(vec![1000], vec![0], vec![true], vec![Some(100)]);

        let result = analysis
            .process_record_batch(&batch, &output_schema)
            .unwrap();

        // Check that all counters are zero for the first event
        let same_process_col = result
            .column_by_name("ns_peer_same_process")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let different_process_col = result
            .column_by_name("ns_peer_different_process")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let kernel_col = result
            .column_by_name("ns_peer_kernel")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();

        assert_eq!(same_process_col.value(0), 0);
        assert_eq!(different_process_col.value(0), 0);
        assert_eq!(kernel_col.value(0), 0);
    }

    #[test]
    fn test_hyperthread_counter_logic() {
        let mut analysis = HyperthreadAnalysis::new(4, PathBuf::from("/tmp/test.parquet")).unwrap();
        let input_schema = create_test_schema();
        let output_schema = analysis.create_output_schema(&input_schema).unwrap();

        // Create a sequence of events to test counter logic
        // CPU 0 and CPU 2 are hyperthread peers (0 + 4/2 = 2)
        let batch = create_test_batch(
            vec![1000, 2000, 3000, 4000, 6000, 10000],
            vec![0, 2, 0, 2, 0, 2],
            vec![true, true, true, true, true, true],
            vec![Some(100), Some(200), Some(100), Some(0), Some(0), Some(0)], // Same process, different process, same process, kernel
        );

        let result = analysis
            .process_record_batch(&batch, &output_schema)
            .unwrap();

        let same_process_col = result
            .column_by_name("ns_peer_same_process")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let different_process_col = result
            .column_by_name("ns_peer_different_process")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let kernel_col = result
            .column_by_name("ns_peer_kernel")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();

        // Event 0 (t=1000, CPU 0): First event, all counters should be 0
        assert_eq!(same_process_col.value(0), 0);
        assert_eq!(different_process_col.value(0), 0);
        assert_eq!(kernel_col.value(0), 0);

        // Event 1 (t=2000, CPU 2): First event on CPU 2, all counters should be 0
        assert_eq!(same_process_col.value(1), 0);
        assert_eq!(different_process_col.value(1), 0);
        assert_eq!(kernel_col.value(1), 0);

        // Event 2 (t=3000, CPU 0): CPU 2 has PID 200, CPU 0 will have PID 100 -> different process
        // Time since last update on CPU 0: 3000 - 2000 = 1000ns (CPU 2 set timestamp to 2000)
        assert_eq!(different_process_col.value(2), 1000);
        assert_eq!(same_process_col.value(2), 0);
        assert_eq!(kernel_col.value(2), 0);

        // Event 3 (t=4000, CPU 2): CPU 0 has PID 100, CPU 2 still has PID 200 -> different process
        // Time since last update on CPU 2: 4000 - 2000 = 2000ns (last updated during event 1)
        assert_eq!(different_process_col.value(3), 2000);
        assert_eq!(same_process_col.value(3), 0);
        assert_eq!(kernel_col.value(3), 0);

        // Event 4 (t=6000, CPU 0): CPU 2 was different process 3000-4000 and kernel 4000-6000
        assert_eq!(different_process_col.value(4), 1000);
        assert_eq!(same_process_col.value(4), 0);
        assert_eq!(kernel_col.value(4), 2000);

        // Event 5 (t=10000, CPU 2): CPU 0 was a different process 4000-6000 and same process (which was the kernel) 6000-10000
        assert_eq!(different_process_col.value(5), 2000);
        assert_eq!(same_process_col.value(5), 0);
        assert_eq!(kernel_col.value(5), 4000);
    }

    #[test]
    fn test_same_process_detection() {
        let mut analysis = HyperthreadAnalysis::new(4, PathBuf::from("/tmp/test.parquet")).unwrap();
        let input_schema = create_test_schema();
        let output_schema = analysis.create_output_schema(&input_schema).unwrap();

        // Both CPUs 0 and 2 run the same process (PID 100)
        let batch = create_test_batch(
            vec![1000, 2000, 3000],
            vec![0, 2, 0],
            vec![true, true, true],
            vec![Some(100), Some(100), Some(100)],
        );

        let result = analysis
            .process_record_batch(&batch, &output_schema)
            .unwrap();

        let same_process_col = result
            .column_by_name("ns_peer_same_process")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();

        // Event 2 (t=3000, CPU 0): Both CPUs have PID 100 -> same process
        // Time since last update: 3000 - 2000 = 1000ns (CPU 2 set timestamp to 2000)
        assert_eq!(same_process_col.value(2), 1000);
    }

    #[test]
    fn test_null_next_tgid_on_context_switch_errors() {
        let mut analysis = HyperthreadAnalysis::new(4, PathBuf::from("/tmp/test.parquet")).unwrap();
        let input_schema = create_test_schema();
        let output_schema = analysis.create_output_schema(&input_schema).unwrap();

        // Context switch with null next_tgid should error
        let batch = create_test_batch(
            vec![1000],
            vec![0],
            vec![true],
            vec![None], // null next_tgid on context switch
        );

        let result = analysis.process_record_batch(&batch, &output_schema);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("next_tgid is null for context switch"));
    }

    #[test]
    fn test_non_context_switch_with_null_next_tgid() {
        let mut analysis = HyperthreadAnalysis::new(4, PathBuf::from("/tmp/test.parquet")).unwrap();
        let input_schema = create_test_schema();
        let output_schema = analysis.create_output_schema(&input_schema).unwrap();

        // Non-context switch with null next_tgid should be fine
        let batch = create_test_batch(
            vec![1000],
            vec![0],
            vec![false], // not a context switch
            vec![None],  // null next_tgid is OK for non-context switch
        );

        let result = analysis.process_record_batch(&batch, &output_schema);
        assert!(result.is_ok());
    }
}
