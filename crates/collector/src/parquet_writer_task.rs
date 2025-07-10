use anyhow::Result;
use arrow_array::RecordBatch;
use tokio::sync::mpsc;

use crate::parquet_writer::ParquetWriter;

/// Worker task for processing record batches and writing them to parquet
pub struct ParquetWriterTask {
    batch_receiver: mpsc::Receiver<RecordBatch>,
    writer: ParquetWriter,
    rotate_receiver: mpsc::Receiver<()>,
}

impl ParquetWriterTask {
    /// Create a new ParquetWriterTask with pre-configured channels
    pub fn new(
        writer: ParquetWriter,
        batch_receiver: mpsc::Receiver<RecordBatch>,
        rotate_receiver: mpsc::Receiver<()>,
    ) -> Self {
        Self {
            batch_receiver,
            writer,
            rotate_receiver,
        }
    }

    /// Run the task, processing record batches until the channel is closed
    pub async fn run(mut self) -> Result<()> {
        loop {
            tokio::select! {
                batch_result = self.batch_receiver.recv() => {
                    match batch_result {
                        Some(batch) => {
                            // Write the batch
                            self.writer.write(batch).await?;
                        }
                        None => {
                            // Channel closed - pipeline shutting down
                            log::debug!("Batch channel closed, shutting down writer task");
                            break;
                        }
                    }
                }
                Some(_) = self.rotate_receiver.recv() => {
                    // Rotation signal received
                    if let Err(e) = self.writer.rotate().await {
                        log::warn!("Failed to rotate parquet file: {}", e);
                    } else {
                        log::info!("Parquet file rotated successfully");
                    }
                }
            }
        }

        // Close writer on shutdown
        log::debug!("Closing parquet writer");
        self.writer.close().await
    }
}
