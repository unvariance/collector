use anyhow::Result;
use arrow_array::{ArrayRef, Int64Array, RecordBatch};
use arrow_schema::Field;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use crate::analyzer::Analysis;

pub struct MonotonicityAnalysis {
    csv_writer: csv::Writer<File>,
    total_row_count: usize,
    last_timestamp: Option<i64>,
}

impl MonotonicityAnalysis {
    pub fn new(csv_output_path: PathBuf) -> Result<Self> {
        let file = File::create(&csv_output_path)?;
        let mut csv_writer = csv::Writer::from_writer(file);

        // Write CSV header
        csv_writer.write_record(&[
            "row_number",
            "event_type",
            "first_timestamp",
            "second_timestamp",
            "difference",
        ])?;

        Ok(Self {
            csv_writer,
            total_row_count: 0,
            last_timestamp: None,
        })
    }
}

impl Analysis for MonotonicityAnalysis {
    fn process_record_batch(&mut self, batch: &RecordBatch) -> Result<Vec<ArrayRef>> {
        let num_rows = batch.num_rows();

        // Extract timestamp column
        let timestamp_col = batch
            .column_by_name("timestamp")
            .ok_or_else(|| anyhow::anyhow!("timestamp column not found"))?
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| anyhow::anyhow!("timestamp column is not Int64Array"))?;

        // Write new record batch event with last timestamp from previous batch and first timestamp of current batch
        let first_timestamp_current = if num_rows > 0 {
            timestamp_col.value(0).to_string()
        } else {
            "".to_string()
        };
        let last_timestamp_prev = self
            .last_timestamp
            .map(|ts| ts.to_string())
            .unwrap_or_else(|| "".to_string());

        self.csv_writer.write_record(&[
            &self.total_row_count.to_string(),
            "new_record_batch",
            &last_timestamp_prev,
            &first_timestamp_current,
            "",
        ])?;

        // Process each row in the batch
        for i in 0..num_rows {
            let current_timestamp = timestamp_col.value(i);

            // Check for monotonicity break
            if let Some(last_ts) = self.last_timestamp {
                if current_timestamp < last_ts {
                    // Found a break in monotonicity
                    let difference = current_timestamp - last_ts; // This will be negative

                    self.csv_writer.write_record(&[
                        &self.total_row_count.to_string(),
                        "break_in_monotonicity",
                        &last_ts.to_string(),
                        &current_timestamp.to_string(),
                        &difference.to_string(),
                    ])?;
                }
            }

            self.last_timestamp = Some(current_timestamp);
            self.total_row_count += 1;
        }

        self.csv_writer.flush()?;

        // Return empty vector since we don't add any columns
        Ok(vec![])
    }

    fn new_columns_schema(&self) -> Vec<Arc<Field>> {
        // Return empty vector since we don't add any columns
        vec![]
    }
}
