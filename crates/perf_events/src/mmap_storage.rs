#![allow(clippy::field_reassign_with_default)]

use std::fs::File;
use std::io;
use std::os::unix::io::{FromRawFd, RawFd};
use std::ptr;
use std::slice;

use libc::{c_void, mmap, munmap, MAP_SHARED, PROT_READ, PROT_WRITE};
use perf_event_open_sys as sys;

use crate::{Storage, StorageError};

/// Memory-mapped ring storage implementation using Linux perf_event_open
///
/// This implementation is only available on Linux platforms.
pub struct MmapStorage {
    data: *mut u8,
    data_len: usize,
    n_data_pages: u32,
    page_size: u64,
    fd: RawFd,
    // Track ownership of the file descriptor
    _file: Option<File>,
}

impl MmapStorage {
    /// Create a new mmap-based ring storage
    ///
    /// # Arguments
    ///
    /// * `cpu` - The CPU to monitor (-1 for any CPU)
    /// * `n_pages` - Number of data pages in the ring buffer
    /// * `n_watermark_bytes` - Number of bytes to wait before waking up. If 0, wake up on every event.
    pub fn new(cpu: i32, n_pages: u32, n_watermark_bytes: u32) -> Result<Self, StorageError> {
        let page_size = page_size::get() as u64;

        // Configure perf event attributes
        let mut attr = sys::bindings::perf_event_attr::default();
        attr.size = std::mem::size_of::<sys::bindings::perf_event_attr>() as u32;
        attr.type_ = sys::bindings::PERF_TYPE_SOFTWARE;
        attr.config = sys::bindings::PERF_COUNT_SW_BPF_OUTPUT as u64;
        #[allow(clippy::unnecessary_cast)]
        {
            attr.sample_type = sys::bindings::PERF_SAMPLE_RAW as u64;
        }

        // Configure watermark behavior
        if n_watermark_bytes > 0 {
            attr.set_watermark(1);
            attr.__bindgen_anon_2.wakeup_watermark = n_watermark_bytes;
        } else {
            attr.__bindgen_anon_2.wakeup_events = 1; // Wake up on every event
        }

        // Open perf event
        let fd = unsafe {
            sys::perf_event_open(
                &mut attr,
                -1, // pid (all threads)
                cpu,
                -1, // group_fd
                sys::bindings::PERF_FLAG_FD_CLOEXEC as u64,
            )
        };

        if fd < 0 {
            return Err(StorageError::OsError(io::Error::last_os_error()));
        }

        // Take ownership of the file descriptor
        let file = unsafe { Some(File::from_raw_fd(fd)) };

        // Calculate total size and mmap the buffer
        let total_size = (page_size * (1 + u64::from(n_pages))) as usize; // 1 metadata page + data pages
        let data_ptr = unsafe {
            mmap(
                ptr::null_mut(),
                total_size,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                0,
            )
        };

        if data_ptr == libc::MAP_FAILED {
            return Err(StorageError::OsError(io::Error::last_os_error()));
        }

        Ok(MmapStorage {
            data: data_ptr as *mut u8,
            data_len: total_size,
            n_data_pages: n_pages,
            page_size,
            fd,
            _file: file,
        })
    }
}

impl Storage for MmapStorage {
    fn data(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.data, self.data_len) }
    }

    fn num_data_pages(&self) -> u32 {
        self.n_data_pages
    }

    fn page_size(&self) -> u64 {
        self.page_size
    }

    fn file_descriptor(&self) -> RawFd {
        self.fd
    }
}

impl Drop for MmapStorage {
    fn drop(&mut self) {
        if !self.data.is_null() {
            unsafe {
                let _ = munmap(self.data as *mut c_void, self.data_len);
            }
            self.data = ptr::null_mut();
        }

        // The fd will be closed when the File is dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mmap_ring_storage() {
        let n_pages = 2;
        let storage = match MmapStorage::new(0, n_pages, 0) {
            Ok(s) => s,
            Err(e) => {
                // If test is run on a platform that doesn't support perf_event_open,
                // skip the test instead of failing
                println!("Skipping test due to error: {}", e);
                return;
            }
        };

        // Check basic properties
        assert_eq!(storage.num_data_pages(), n_pages);
        assert_eq!(storage.page_size(), page_size::get() as u64);

        let expected_size = storage.page_size() * (1 + u64::from(n_pages));
        assert_eq!(storage.data().len() as u64, expected_size);

        assert!(storage.file_descriptor() > 0);
    }

    #[test]
    fn test_mmap_ring_storage_watermark() {
        // Test with wake up on every event
        if let Err(e) = MmapStorage::new(0, 2, 0) {
            println!("Skipping watermark test (every event) due to error: {}", e);
            return;
        }

        // Test with wake up after 4096 bytes
        if let Err(e) = MmapStorage::new(0, 2, 4096) {
            println!("Skipping watermark test (4096 bytes) due to error: {}", e);
            // function ends here
        }
    }
}
