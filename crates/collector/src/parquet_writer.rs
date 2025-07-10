use std::sync::Arc;

use anyhow::{anyhow, Result};
use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use chrono::Utc;
use log::{debug, info};
use object_store::{path::Path, ObjectStore};
use parquet::arrow::arrow_writer::ArrowWriterOptions;
use parquet::arrow::async_writer::{AsyncArrowWriter, ParquetObjectWriter};
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use uuid::Uuid;

/// Configuration for the parquet writer
pub struct ParquetWriterConfig {
    /// Path prefix to use within the storage location
    /// This will be directly prepended to filenames without adding separators
    /// Include any needed separators (like "/" or "-") at the end if desired
    pub storage_prefix: String,
    /// Maximum buffer size before flushing to storage (bytes)
    pub buffer_size: usize,
    /// Maximum file size before rotation (bytes)
    pub file_size_limit: usize,
    /// Maximum row group size (number of rows)
    pub max_row_group_size: usize,
    /// Optional total storage quota (bytes)
    pub storage_quota: Option<usize>,
}

impl Default for ParquetWriterConfig {
    fn default() -> Self {
        Self {
            storage_prefix: "metrics-".to_string(),
            buffer_size: 100 * 1024 * 1024,      // 100MB
            file_size_limit: 1024 * 1024 * 1024, // 1GB
            max_row_group_size: 1024 * 1024,     // Default max row group size
            storage_quota: None,
        }
    }
}

/// Handles writing record batches to parquet files in object storage
pub struct ParquetWriter {
    store: Arc<dyn ObjectStore>,
    schema: SchemaRef,
    current_writer: Option<AsyncArrowWriter<ParquetObjectWriter>>,
    current_file_path: Option<Path>,

    // Size tracking
    closed_files_size: usize,
    flushed_row_groups_size: usize,
    flushed_row_groups_count: usize,
    in_memory_size: usize,

    config: ParquetWriterConfig,
}

impl ParquetWriter {
    /// Creates a new ParquetWriter with the provided object store, schema, and config
    pub fn new(
        store: Arc<dyn ObjectStore>,
        schema: SchemaRef,
        config: ParquetWriterConfig,
    ) -> Result<Self> {
        let mut writer = Self {
            store,
            schema,
            current_writer: None,
            current_file_path: None,
            closed_files_size: 0,
            flushed_row_groups_size: 0,
            flushed_row_groups_count: 0,
            in_memory_size: 0,
            config,
        };

        // Create initial file
        writer.create_new_file()?;

        Ok(writer)
    }

    /// Generate a new file path with timestamp and UUID
    fn generate_file_path(&self) -> Path {
        let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let uuid = Uuid::new_v4()
            .to_string()
            .chars()
            .take(8)
            .collect::<String>();

        // Include the prefix from config directly in the filename
        let filename = format!(
            "{}{}-{}.parquet",
            self.config.storage_prefix, timestamp, uuid
        );

        Path::from(filename)
    }

    /// Create a new file and writer
    fn create_new_file(&mut self) -> Result<()> {
        // Close the current writer if it exists
        if self.current_writer.is_some() {
            // error if we try to create a new file while there is an open writer
            return Err(anyhow!(
                "Cannot create new file while there is an open writer"
            ));
        }

        // Check quota before creating a new file
        if !self.is_below_quota() {
            debug!("Not creating new file: storage quota reached");
            return Ok(());
        }

        // Generate new file path
        let path = self.generate_file_path();

        // Create writer properties with Snappy compression
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_max_row_group_size(self.config.max_row_group_size)
            .build();

        let object_writer = ParquetObjectWriter::new(self.store.clone(), path.clone());

        let options = ArrowWriterOptions::new().with_properties(props);
        let writer =
            AsyncArrowWriter::try_new_with_options(object_writer, self.schema.clone(), options)?;

        // Store the writer and path
        self.current_writer = Some(writer);
        self.current_file_path = Some(path.clone());

        debug!("Created new parquet writer for path: {}", path);

        // Reset size tracking for the new file
        self.update_current_writer_size()?;

        Ok(())
    }

    /// Checks if we've exceeded our storage quota
    fn is_below_quota(&self) -> bool {
        if let Some(quota) = self.config.storage_quota {
            let total_size =
                self.closed_files_size + self.flushed_row_groups_size + self.in_memory_size;
            if total_size >= quota {
                return false;
            }
        }
        true
    }

    /// Update the size tracking from the current writer
    fn update_current_writer_size(&mut self) -> Result<()> {
        if let Some(writer) = &self.current_writer {
            // Get the current number of flushed row groups
            let current_flushed_groups = writer.flushed_row_groups().len();

            // Only recalculate flushed size if the count has changed
            if current_flushed_groups != self.flushed_row_groups_count {
                // Get the size of all flushed row groups
                let flushed_size: i64 = writer
                    .flushed_row_groups()
                    .iter()
                    .map(|rg| rg.compressed_size())
                    .sum();

                // Update size tracking
                self.flushed_row_groups_size = flushed_size as usize;
                self.flushed_row_groups_count = current_flushed_groups;
            }

            // Update in-memory size from writer
            self.in_memory_size = writer.in_progress_size();
        } else {
            // No writer, reset all sizes
            self.flushed_row_groups_size = 0;
            self.flushed_row_groups_count = 0;
            self.in_memory_size = 0;
        }
        Ok(())
    }

    /// Check if we should rotate the file based on size
    async fn maybe_rotate_file(&mut self) -> Result<()> {
        let current_file_size = self.flushed_row_groups_size + self.in_memory_size;

        if current_file_size >= self.config.file_size_limit {
            info!(
                "Rotating file due to size limit: current size: {} ({} in {} row groups, {} in memory), limit: {}",
                current_file_size,
                self.flushed_row_groups_size,
                self.flushed_row_groups_count,
                self.in_memory_size,
                self.config.file_size_limit
            );
            self.close_writer().await?;
            self.create_new_file()?;
        }

        Ok(())
    }

    /// Write a record batch to the parquet file
    pub async fn write(&mut self, batch: RecordBatch) -> Result<()> {
        // Skip writing if we've exceeded quota
        if !self.is_below_quota() {
            return Ok(());
        }

        if let Some(writer) = &mut self.current_writer {
            // Write the batch
            writer.write(&batch).await?;

            // Update size tracking
            self.update_current_writer_size()?;

            // did we exceed the quota?
            if !self.is_below_quota() {
                info!("Exceeded storage quota, stopping writes");
                // close the writer
                self.close_writer().await?;

                // the actual written size might be a bit less than the quota, but now this triggered, we're done writing.
                // force the sizes to be equal to the quota so is_below_quota returns false
                if let Some(quota) = self.config.storage_quota {
                    self.closed_files_size = quota;
                }
                return Ok(());
            }

            // Check if we need to flush based on buffer size
            if self.in_memory_size >= self.config.buffer_size {
                info!("Flushing due to buffer size: {}, buffer size limit: {} (previously flushed {} in {} row groups)", self.in_memory_size, self.config.buffer_size, self.flushed_row_groups_size, self.flushed_row_groups_count);
                self.flush().await?;
            }

            // Check if we need to rotate the file
            self.maybe_rotate_file().await?;
        } else {
            return Err(anyhow!("No writer available"));
        }

        Ok(())
    }

    /// Flush any pending data
    pub async fn flush(&mut self) -> Result<()> {
        if let Some(writer) = &mut self.current_writer {
            writer.flush().await?;
            self.update_current_writer_size()?;
        }
        Ok(())
    }

    /// Close the writer, finishing the Parquet file
    pub async fn close(mut self) -> Result<()> {
        debug!("Closing ParquetWriter instance");
        self.close_writer().await
    }

    /// Close the writer, finishing the Parquet file
    async fn close_writer(&mut self) -> Result<()> {
        if let Some(writer) = self.current_writer.take() {
            let metadata = writer.close().await?;

            // Log the metadata details
            debug!(
                "Closed parquet file at path '{}' with {} row groups, {} rows",
                self.current_file_path
                    .as_ref()
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
                metadata.row_groups.len(),
                metadata
                    .row_groups
                    .iter()
                    .map(|rg| rg.num_rows)
                    .sum::<i64>()
            );

            // Update closed files size from the metadata
            for row_group in &metadata.row_groups {
                if let Some(size) = row_group.total_compressed_size {
                    self.closed_files_size += size as usize;
                }
            }
        }

        self.update_current_writer_size()?;

        Ok(())
    }

    /// Rotate the current parquet file, closing the current one and creating a new one
    pub async fn rotate(&mut self) -> Result<()> {
        debug!("Rotating parquet file");
        // Close the current writer
        self.close_writer().await?;
        // Create a new file (this will check quota)
        self.create_new_file()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use arrow_array::{builder::{BooleanBuilder, Float64Builder, Int32Builder, StringBuilder}, ArrayRef};
    use arrow_schema::{DataType, Field, Schema};
    use futures::StreamExt;
    use object_store::memory::InMemory;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use super::*;

    /// Create a simple test schema with multiple data types
    fn create_test_schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("value", DataType::Float64, false),
            Field::new("active", DataType::Boolean, false),
        ]))
    }

    /// Create a test record batch with known data
    fn create_test_batch(schema: SchemaRef) -> Result<RecordBatch> {
        let mut id_builder = Int32Builder::with_capacity(2);
        let mut name_builder = StringBuilder::with_capacity(2, 20);
        let mut value_builder = Float64Builder::with_capacity(2);
        let mut active_builder = BooleanBuilder::with_capacity(2);

        // First row
        id_builder.append_value(101);
        name_builder.append_value("alice");
        value_builder.append_value(12.34);
        active_builder.append_value(true);

        // Second row
        id_builder.append_value(202);
        name_builder.append_value("bob");
        value_builder.append_value(56.78);
        active_builder.append_value(false);

        let arrays: Vec<ArrayRef> = vec![
            Arc::new(id_builder.finish()),
            Arc::new(name_builder.finish()),
            Arc::new(value_builder.finish()),
            Arc::new(active_builder.finish()),
        ];

        RecordBatch::try_new(schema, arrays)
            .map_err(|e| anyhow!("Failed to create test RecordBatch: {}", e))
    }

    #[tokio::test]
    async fn test_parquet_write_and_read() {
        // Create test schema and data
        let schema = create_test_schema();
        let test_batch = create_test_batch(schema.clone()).unwrap();

        // Create a test writer using in-memory storage
        let memory_storage = Arc::new(InMemory::new());
        let mut writer = ParquetWriter::new(
            memory_storage.clone(),
            schema.clone(),
            ParquetWriterConfig::default(),
        )
        .unwrap();

        // Write the batch
        writer.write(test_batch.clone()).await.unwrap();
        writer.close().await.unwrap();

        // Verify files were created
        let list_stream = memory_storage.list(None);
        let files: Vec<_> = list_stream.collect().await;
        assert_eq!(files.len(), 1, "Expected exactly one parquet file");

        let file_path = &files[0].as_ref().unwrap().location;

        // Read back the parquet file and verify contents
        let file_data = memory_storage.get(file_path).await.unwrap();
        let bytes = file_data.bytes().await.unwrap();

        // Create parquet reader
        let reader_builder = ParquetRecordBatchReaderBuilder::try_new(bytes).unwrap();
        let mut reader = reader_builder.build().unwrap();

        // Read all batches
        let mut all_batches = Vec::new();
        while let Some(batch) = reader.next() {
            all_batches.push(batch.unwrap());
        }

        // Verify we got exactly one batch back
        assert_eq!(all_batches.len(), 1, "Expected exactly one batch");
        let read_batch = &all_batches[0];

        // Verify schema matches
        assert_eq!(read_batch.schema(), schema);

        // Verify structure
        assert_eq!(read_batch.num_rows(), 2);
        assert_eq!(read_batch.num_columns(), 4);

        // Verify content - extract arrays and check values
        use arrow_array::{BooleanArray, Float64Array, Int32Array, StringArray};

        // Check id column
        let id_array = read_batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(id_array.value(0), 101);
        assert_eq!(id_array.value(1), 202);

        // Check name column
        let name_array = read_batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(name_array.value(0), "alice");
        assert_eq!(name_array.value(1), "bob");

        // Check value column
        let value_array = read_batch
            .column(2)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert!((value_array.value(0) - 12.34).abs() < f64::EPSILON);
        assert!((value_array.value(1) - 56.78).abs() < f64::EPSILON);

        // Check active column
        let active_array = read_batch
            .column(3)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert_eq!(active_array.value(0), true);
        assert_eq!(active_array.value(1), false);
    }

    #[tokio::test]
    async fn test_file_rotation() {
        // Create test schema
        let schema = create_test_schema();

        // Create a test writer with a small file size limit to force rotation
        let memory_storage = Arc::new(InMemory::new());
        let config = ParquetWriterConfig {
            storage_prefix: "test-".to_string(),
            file_size_limit: 10_000, // Very small limit to force rotation
            buffer_size: 1_000,      // Small buffer to force frequent flushes
            max_row_group_size: 10,  // Small row group size
            storage_quota: None,
        };

        let mut writer =
            ParquetWriter::new(memory_storage.clone(), schema.clone(), config).unwrap();

        // Create one large batch with 100 rows (similar to original test)
        let mut id_builder = Int32Builder::with_capacity(100);
        let mut name_builder = StringBuilder::with_capacity(100, 1600); // 16 chars per name * 100
        let mut value_builder = Float64Builder::with_capacity(100);
        let mut active_builder = BooleanBuilder::with_capacity(100);

        // Create 100 rows per batch to match original test data volume
        for i in 0..100 {
            id_builder.append_value(i);
            name_builder.append_value(&format!("user_{}", i));
            value_builder.append_value(i as f64 * 1.5);
            active_builder.append_value(i % 2 == 0);
        }

        let arrays: Vec<ArrayRef> = vec![
            Arc::new(id_builder.finish()),
            Arc::new(name_builder.finish()),
            Arc::new(value_builder.finish()),
            Arc::new(active_builder.finish()),
        ];

        let large_batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();

        // Write the large batch multiple times to exceed the file size limit (like original test)
        for _ in 0..50 {
            writer.write(large_batch.clone()).await.unwrap();
            writer.flush().await.unwrap();
        }

        // Close the writer
        writer.close().await.unwrap();

        // Check that multiple files were created (rotation occurred)
        let list_stream = memory_storage.list(None);
        let files: Vec<_> = list_stream.collect().await;

        // We should have at least 2 files due to rotation
        assert!(
            files.len() > 1,
            "Expected multiple files due to rotation, but found {}",
            files.len()
        );

        // Verify all files have the correct prefix
        for file in &files {
            let path_str = file.as_ref().unwrap().location.to_string();
            assert!(
                path_str.starts_with("test-"),
                "File path should start with the configured prefix"
            );
        }

        // Verify all files have content
        for file in &files {
            let size = file.as_ref().unwrap().size;
            assert!(size > 0, "File should have content");
        }
    }
}
