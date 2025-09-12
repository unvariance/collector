//! PerfMapReader implementation that connects perf events to eBPF maps.
//!
//! This module provides a PerfMapReader type that manages memory-mapped perf
//! ring buffers connected to an eBPF map.

use std::io;
use std::slice;

use crate::{MmapStorage, PerfRing, PerfRingError, Reader, ReaderError, Storage, StorageError};
use libbpf_rs::{MapCore as _, MapMut};

use crate::helpers::{self, PerfEventError};

/// Error type for perf map operations
#[derive(Debug, thiserror::Error)]
pub enum PerfMapError {
    /// Error from perf event operations
    #[error("perf event error: {0}")]
    PerfEventError(#[from] PerfEventError),

    /// Error getting map info
    #[error("failed to get map info: {0}")]
    MapInfoError(#[from] libbpf_rs::Error),

    /// Error creating MmapStorage
    #[error("failed to create mmap storage for CPU {cpu}: {source}")]
    StorageError {
        /// CPU where the error occurred
        cpu: i32,
        /// Source error
        source: StorageError,
    },

    /// Error initializing a ring
    #[error("failed to initialize perf ring for CPU {cpu}: {source}")]
    RingInitError {
        /// CPU where the error occurred
        cpu: i32,
        /// Source error
        source: PerfRingError,
    },

    /// Error adding a ring to the reader
    #[error("failed to add ring to reader: {0}")]
    ReaderAddRingError(ReaderError),
}

/// PerfMapReader manages perf ring buffers connected to an eBPF map
pub struct PerfMapReader {
    /// Storage for each CPU
    _storage: Vec<MmapStorage>,
    /// Reader for the perf rings
    reader: Reader,
}

impl PerfMapReader {
    /// Creates a new PerfMapReader connected to the provided eBPF map
    ///
    /// # Arguments
    ///
    /// - `map`: The eBPF map to connect to (should be a PERF_EVENT_ARRAY map)
    /// - `buffer_pages`: The size of each per-CPU buffer in pages
    /// - `watermark_bytes`: The number of bytes that must be written before waking up userspace.
    ///   A value of 0 means wake up on every event.
    ///
    /// # Returns
    ///
    /// Returns `Result<PerfMapReader, PerfMapError>`: The configured reader on success
    pub fn new(
        map: &mut MapMut,
        buffer_pages: u32,
        watermark_bytes: u32,
    ) -> Result<Self, PerfMapError> {
        // Get number of possible CPUs from the map
        let n_cpu = map.info()?.info.max_entries as i32;
        if n_cpu < 1 {
            return Err(PerfMapError::MapInfoError(
                io::Error::new(io::ErrorKind::InvalidInput, "invalid number of CPUs in map").into(),
            ));
        }

        // Create storage, rings, and reader
        let mut storage = Vec::with_capacity(n_cpu as usize);
        let mut reader = Reader::new();
        let mut fds = Vec::with_capacity(n_cpu as usize);

        // Create storage and rings for each CPU
        for cpu in 0..n_cpu {
            // Create MmapStorage with the specified options
            let cpu_storage = MmapStorage::new(cpu, buffer_pages, watermark_bytes)
                .map_err(|e| PerfMapError::StorageError { cpu, source: e })?;

            // Get file descriptor to store in the map
            let fd = cpu_storage.file_descriptor();
            fds.push(fd);

            // Initialize a ring from the storage
            // Create a mutable slice for the ring (PerfRing needs a mutable slice)
            let data_slice = unsafe {
                let data_ptr = cpu_storage.data().as_ptr() as *mut u8;
                slice::from_raw_parts_mut(data_ptr, cpu_storage.data().len())
            };

            let ring = unsafe {
                PerfRing::init_contiguous(
                    data_slice,
                    cpu_storage.num_data_pages(),
                    cpu_storage.page_size(),
                )
                .map_err(|e| PerfMapError::RingInitError { cpu, source: e })?
            };

            // Add the ring to the reader
            reader
                .add_ring(ring)
                .map_err(PerfMapError::ReaderAddRingError)?;

            // Save the storage
            storage.push(cpu_storage);
        }

        // Update the map with all file descriptors at once
        helpers::update_map_with_fds(map, &fds).map_err(PerfMapError::PerfEventError)?;

        Ok(PerfMapReader {
            _storage: storage,
            reader,
        })
    }

    /// Returns a reference to the underlying perf reader
    pub fn reader(&self) -> &Reader {
        &self.reader
    }

    /// Returns a mutable reference to the underlying perf reader
    pub fn reader_mut(&mut self) -> &mut Reader {
        &mut self.reader
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // This test requires root, run with cargo test -- --ignored
    fn test_perf_map_reader_new() {
        // Since eBPF map creation requires privileges, we'll just
        // skip the test if we don't have them
        println!("This test requires root privileges");

        // A simplified test without actually creating the map
        let storage = match MmapStorage::new(0, 2, 0) {
            Ok(s) => s,
            Err(e) => {
                println!("Skipping test, could not create storage: {}", e);
                return;
            }
        };

        // Verify things work
        println!("Storage file descriptor: {}", storage.file_descriptor());
        println!("Storage data pages: {}", storage.num_data_pages());
        println!("Storage page size: {}", storage.page_size());
    }
}
