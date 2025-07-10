use anyhow::Result;
use tokio::sync::mpsc;

use crate::parquet_writer::ParquetWriter;
use crate::timeslot_data::TimeslotData;

/// Worker task for processing timeslots and writing them to parquet
pub struct ParquetWriterTask {
    timeslot_receiver: mpsc::Receiver<TimeslotData>,
    writer: ParquetWriter,
    rotate_receiver: mpsc::Receiver<()>,
}

impl ParquetWriterTask {
    /// Create a new ParquetWriterTask with pre-configured channels
    pub fn new(
        writer: ParquetWriter,
        timeslot_receiver: mpsc::Receiver<TimeslotData>,
        rotate_receiver: mpsc::Receiver<()>,
    ) -> Self {
        Self {
            timeslot_receiver,
            writer,
            rotate_receiver,
        }
    }

    /// Run the task, processing timeslots until the channel is closed
    pub async fn run(mut self) -> Result<()> {
        loop {
            tokio::select! {
                timeslot_result = self.timeslot_receiver.recv() => {
                    match timeslot_result {
                        Some(timeslot) => {
                            // Convert timeslot to a batch
                            let batch = self.writer.timeslot_to_batch(timeslot)?;

                            // Write the batch
                            self.writer.write(batch).await?;
                        }
                        None => {
                            // Channel closed - pipeline shutting down
                            log::debug!("Timeslot channel closed, shutting down writer task");
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
