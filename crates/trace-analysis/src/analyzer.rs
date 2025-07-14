use anyhow::{Context, Result};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{Field, Schema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

const READER_BATCH_SIZE: usize = 32 * 1024; // 32k rows per batch

/// Trait for analysis modules that process record batches and add new columns
pub trait Analysis {
    /// Process a record batch and return new columns to be added
    fn process_record_batch(&mut self, batch: &RecordBatch) -> Result<Vec<ArrayRef>>;

    /// Return the schema for the new columns this analysis adds
    fn new_columns_schema(&self) -> Vec<Arc<Field>>;
}

/// Analyzer that runs analysis functions on Parquet files
pub struct Analyzer {
    output_filename: PathBuf,
}

impl Analyzer {
    /// Create a new analyzer
    pub fn new(output_filename: PathBuf) -> Self {
        Self { output_filename }
    }

    /// Process a Parquet file with the given analysis
    pub fn process_parquet_file<A: Analysis>(
        &self,
        builder: ParquetRecordBatchReaderBuilder<File>,
        mut analysis: A,
    ) -> Result<()> {
        let input_schema = builder.schema().clone();
        let mut arrow_reader = builder
            .with_batch_size(READER_BATCH_SIZE)
            .build()
            .with_context(|| "Failed to build Arrow reader")?;

        // Create output schema with additional columns from analysis
        let output_schema = self.create_output_schema(&input_schema, &analysis)?;

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
            let augmented_batch =
                self.process_record_batch(&batch, &mut analysis, &output_schema)?;
            writer
                .write(&augmented_batch)
                .with_context(|| "Failed to write augmented batch")?;
        }

        writer.close().with_context(|| "Failed to close writer")?;
        Ok(())
    }

    /// Create output schema by combining input schema with analysis columns
    fn create_output_schema<A: Analysis>(
        &self,
        input_schema: &Schema,
        analysis: &A,
    ) -> Result<Schema> {
        let mut fields: Vec<Arc<Field>> = input_schema.fields().iter().cloned().collect();

        // Add new columns from analysis
        fields.extend(analysis.new_columns_schema());

        Ok(Schema::new(fields))
    }

    /// Process a record batch by running analysis and combining results
    fn process_record_batch<A: Analysis>(
        &self,
        batch: &RecordBatch,
        analysis: &mut A,
        output_schema: &Schema,
    ) -> Result<RecordBatch> {
        // Get new columns from analysis
        let new_columns = analysis.process_record_batch(batch)?;

        // Combine original columns with new columns
        let mut output_columns: Vec<ArrayRef> = batch.columns().to_vec();
        output_columns.extend(new_columns);

        RecordBatch::try_new(Arc::new(output_schema.clone()), output_columns)
            .with_context(|| "Failed to create output record batch")
    }
}
