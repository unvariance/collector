use anyhow::{Context, Result};
use std::path::PathBuf;
use std::fs::File;
use std::sync::Arc;

use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use arrow_array::{RecordBatch, Int64Array, Int32Array, BooleanArray, ArrayRef};
use arrow_schema::{Schema, Field, DataType};

#[derive(Debug, Clone)]
struct CpuState {
    current_pid: i32,
    last_counter_update: i64,
    ns_peer_same_process: i64,
    ns_peer_different_process: i64,
    ns_peer_kernel: i64,
}

impl CpuState {
    fn new() -> Self {
        Self {
            current_pid: 0,
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
        let time_since_a = event_timestamp - self.cpu_states[cpu_a].last_counter_update;
        let time_since_b = event_timestamp - self.cpu_states[cpu_b].last_counter_update;
        
        // Update counters for CPU A based on CPU B's state
        let peer_b_pid = self.cpu_states[cpu_b].current_pid;
        if peer_b_pid == 0 {
            self.cpu_states[cpu_a].ns_peer_kernel += time_since_a;
        } else if peer_b_pid == self.cpu_states[cpu_a].current_pid {
            self.cpu_states[cpu_a].ns_peer_same_process += time_since_a;
        } else {
            self.cpu_states[cpu_a].ns_peer_different_process += time_since_a;
        }
        
        // Update counters for CPU B based on CPU A's state  
        let peer_a_pid = self.cpu_states[cpu_a].current_pid;
        if peer_a_pid == 0 {
            self.cpu_states[cpu_b].ns_peer_kernel += time_since_b;
        } else if peer_a_pid == self.cpu_states[cpu_b].current_pid {
            self.cpu_states[cpu_b].ns_peer_same_process += time_since_b;
        } else {
            self.cpu_states[cpu_b].ns_peer_different_process += time_since_b;
        }
        
        // Update timestamps
        self.cpu_states[cpu_a].last_counter_update = event_timestamp;
        self.cpu_states[cpu_b].last_counter_update = event_timestamp;
    }
    
    pub fn process_parquet_file(&mut self, builder: ParquetRecordBatchReaderBuilder<File>) -> Result<()> {
        let input_schema = builder.schema().clone();
        let mut arrow_reader = builder.build()
            .with_context(|| "Failed to build Arrow reader")?;
        
        // Create output schema with additional hyperthread columns
        let output_schema = self.create_output_schema(&input_schema)?;
        
        // Create Arrow writer
        let output_file = File::create(&self.output_filename)
            .with_context(|| format!("Failed to create output file: {}", self.output_filename.display()))?;
        
        let mut writer = ArrowWriter::try_new(output_file, Arc::new(output_schema.clone()), None)
            .with_context(|| "Failed to create Arrow writer")?;
        
        // Process record batches
        while let Some(batch) = arrow_reader.next() {
            let batch = batch.with_context(|| "Failed to read record batch")?;
            let augmented_batch = self.process_record_batch(&batch, &output_schema)?;
            writer.write(&augmented_batch)
                .with_context(|| "Failed to write augmented batch")?;
        }
        
        writer.close()
            .with_context(|| "Failed to close writer")?;
        
        Ok(())
    }
    
    fn create_output_schema(&self, input_schema: &Schema) -> Result<Schema> {
        let mut fields: Vec<Arc<Field>> = input_schema.fields().iter().cloned().collect();
        
        // Add hyperthread counter fields
        fields.push(Arc::new(Field::new("ns_peer_same_process", DataType::Int64, false)));
        fields.push(Arc::new(Field::new("ns_peer_different_process", DataType::Int64, false)));
        fields.push(Arc::new(Field::new("ns_peer_kernel", DataType::Int64, false)));
        
        Ok(Schema::new(fields))
    }
    
    fn process_record_batch(&mut self, batch: &RecordBatch, output_schema: &Schema) -> Result<RecordBatch> {
        let num_rows = batch.num_rows();
        
        // Extract required columns
        let timestamp_col = batch.column_by_name("timestamp")
            .ok_or_else(|| anyhow::anyhow!("timestamp column not found"))?
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| anyhow::anyhow!("timestamp column is not Int64Array"))?;
            
        let cpu_id_col = batch.column_by_name("cpu_id")
            .ok_or_else(|| anyhow::anyhow!("cpu_id column not found"))?
            .as_any()
            .downcast_ref::<Int32Array>()
            .ok_or_else(|| anyhow::anyhow!("cpu_id column is not Int32Array"))?;
            
        let pid_col = batch.column_by_name("pid")
            .ok_or_else(|| anyhow::anyhow!("pid column not found"))?
            .as_any()
            .downcast_ref::<Int32Array>()
            .ok_or_else(|| anyhow::anyhow!("pid column is not Int32Array"))?;
            
        let is_context_switch_col = batch.column_by_name("is_context_switch")
            .ok_or_else(|| anyhow::anyhow!("is_context_switch column not found"))?
            .as_any()
            .downcast_ref::<BooleanArray>()
            .ok_or_else(|| anyhow::anyhow!("is_context_switch column is not BooleanArray"))?;
        
        // Prepare output arrays for hyperthread counters
        let mut ns_peer_same_process = Vec::with_capacity(num_rows);
        let mut ns_peer_different_process = Vec::with_capacity(num_rows);
        let mut ns_peer_kernel = Vec::with_capacity(num_rows);
        
        // Process each row
        for i in 0..num_rows {
            let timestamp = timestamp_col.value(i);
            let cpu_id = cpu_id_col.value(i) as usize;
            let pid = pid_col.value(i);
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
                self.cpu_states[cpu_id].current_pid = pid;
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