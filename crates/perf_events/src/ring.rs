use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

/// Errors that can occur when using the perf ring buffer
#[derive(Error, Debug)]
pub enum PerfRingError {
    #[error("buffer length must be a power of 2 and at least 8 bytes")]
    InvalidBufferLength,

    #[error("data buffer cannot be nil")]
    NilBuffer,

    #[error("buffer full")]
    NoSpace,

    #[error("buffer empty")]
    BufferEmpty,

    #[error("data too large for buffer")]
    CannotFit,

    #[error("cannot write empty data")]
    EmptyWrite,

    #[error("requested read larger than data")]
    SizeExceeded,
}

/// PerfEventHeader represents the header of a perf event
#[repr(C, packed)]
pub struct PerfEventHeader {
    pub type_: u32,
    pub misc: u16,
    pub size: u16,
}

/// Shared metadata page for perf ring buffer
#[repr(C)]
pub struct PerfEventMmapPage {
    pub version: u32,
    pub compat_version: u32,
    pad1: [u8; 1024 - 8],
    pub data_head: AtomicU64,
    pub data_tail: AtomicU64,
    pub data_offset: u64,
    pub data_size: u64,
    pub aux_offset: u64,
    pub aux_size: u64,
}

/// Type constants for perf events
pub const PERF_RECORD_SAMPLE: u32 = 9;
pub const PERF_RECORD_LOST: u32 = 2;

/// PerfRing represents a perf ring buffer with shared metadata and data pages
pub struct PerfRing {
    // Shared metadata page
    meta: NonNull<PerfEventMmapPage>,
    // Data buffer
    data: *mut u8,
    // Data buffer length
    data_len: usize,
    // Mask for quick modulo operations (buffer size - 1)
    buf_mask: u64,
    // Current head position for reading
    head: u64,
    // Current tail position for writing
    tail: u64,
}

// Safety: PerfRing needs to be Send+Sync because it's shared between threads
// We're manually ensuring thread safety with atomics
unsafe impl Send for PerfRing {}
unsafe impl Sync for PerfRing {}

impl PerfRing {
    /// Initializes a PerfRing using contiguous memory
    ///
    /// # Safety
    ///
    /// This function is unsafe because it works with raw pointers and assumes the
    /// provided slice will outlive the PerfRing.
    pub unsafe fn init_contiguous(
        data: &mut [u8],
        n_pages: u32,
        page_size: u64,
    ) -> Result<Self, PerfRingError> {
        if data.is_empty() {
            return Err(PerfRingError::NilBuffer);
        }

        let buf_len = u64::from(n_pages) * page_size;
        if (buf_len & (buf_len - 1)) != 0 || buf_len < 8 {
            return Err(PerfRingError::InvalidBufferLength);
        }

        // First page is metadata, rest is data
        let meta_ptr = data.as_mut_ptr() as *mut PerfEventMmapPage;
        let meta = NonNull::new(meta_ptr).unwrap();

        // If data_offset is not given (older kernels), we need to skip a full page,
        // otherwise we skip data_offset bytes
        let data_start = if (*meta_ptr).data_offset == 0 {
            page_size
        } else {
            (*meta_ptr).data_offset
        };

        let data_ptr = data.as_mut_ptr().add(data_start as usize);
        let data_tail = (*meta_ptr).data_tail.load(Ordering::Acquire);
        let data_head = (*meta_ptr).data_head.load(Ordering::Acquire);

        Ok(PerfRing {
            meta,
            data: data_ptr,
            data_len: buf_len as usize,
            buf_mask: buf_len - 1,
            head: data_tail,
            tail: data_head,
        })
    }

    /// Starts a write batch operation
    pub fn start_write_batch(&mut self) {
        // Get the current tail position from shared memory using atomic load
        unsafe {
            self.head = self.meta.as_ref().data_tail.load(Ordering::Acquire);
        }
    }

    /// Writes data to the ring buffer with the given type
    pub fn write(&mut self, data: &[u8], event_type: u32) -> Result<usize, PerfRingError> {
        if data.is_empty() {
            return Err(PerfRingError::EmptyWrite);
        }

        let mut unaligned_len = data.len() as u32 + std::mem::size_of::<PerfEventHeader>() as u32;

        if event_type == PERF_RECORD_SAMPLE {
            unaligned_len += 4; // add the u32 size field
        }

        // Calculate total size including header, aligned to 8 bytes
        let aligned_len = (unaligned_len + 7) & !7;
        if aligned_len > self.buf_mask as u32 {
            return Err(PerfRingError::CannotFit);
        }

        // Check if there's enough space
        if self.tail + u64::from(aligned_len) - self.head > self.buf_mask + 1 {
            return Err(PerfRingError::NoSpace);
        }

        unsafe {
            // Write header
            let header = PerfEventHeader {
                type_: event_type,
                misc: 0,
                size: aligned_len as u16,
            };
            let header_pos = (self.tail & self.buf_mask) as usize;
            ptr::write(self.data.add(header_pos) as *mut PerfEventHeader, header);

            // Write data
            let header_size = std::mem::size_of::<PerfEventHeader>();
            let mut data_pos = (self.tail + header_size as u64) & self.buf_mask;

            if event_type == PERF_RECORD_SAMPLE {
                // write the u32 size field
                let size_value = ((data.len() + 4 + 7) & !7) as u32;
                ptr::write(self.data.add(data_pos as usize) as *mut u32, size_value);
                data_pos = (data_pos + 4) & self.buf_mask;
            }

            if data_pos as usize + data.len() <= self.data_len {
                // Data fits without wrapping
                ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    self.data.add(data_pos as usize),
                    data.len(),
                );
            } else {
                // Data wraps around buffer end
                let first_part = self.data_len - data_pos as usize;
                ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    self.data.add(data_pos as usize),
                    first_part,
                );
                ptr::copy_nonoverlapping(
                    data.as_ptr().add(first_part),
                    self.data,
                    data.len() - first_part,
                );
            }

            self.tail += aligned_len as u64;
            Ok(data_pos as usize)
        }
    }

    /// Finishes a write batch operation
    pub fn finish_write_batch(&mut self) {
        // Ensure all writes are visible before updating tail using atomic store
        unsafe {
            self.meta
                .as_ref()
                .data_head
                .store(self.tail, Ordering::Release);
        }
    }

    /// Starts a read batch operation
    pub fn start_read_batch(&mut self) {
        // Get the current head position from shared memory using atomic load
        unsafe {
            self.tail = self.meta.as_ref().data_head.load(Ordering::Acquire);
        }
    }

    /// Returns the size of the next event in the ring buffer
    pub fn peek_size(&self) -> Result<usize, PerfRingError> {
        if self.tail == self.head {
            return Err(PerfRingError::BufferEmpty);
        }

        unsafe {
            let header =
                &*(self.data.add((self.head & self.buf_mask) as usize) as *const PerfEventHeader);
            Ok(header.size as usize - std::mem::size_of::<PerfEventHeader>())
        }
    }

    /// Returns the type of the next event
    pub fn peek_type(&self) -> u32 {
        unsafe {
            let header =
                &*(self.data.add((self.head & self.buf_mask) as usize) as *const PerfEventHeader);
            header.type_
        }
    }

    /// Copies data from the ring buffer without consuming it
    pub fn peek_copy(&self, buf: &mut [u8], offset: u16) -> Result<(), PerfRingError> {
        let size = self.peek_size()?;

        if buf.len() > size {
            return Err(PerfRingError::SizeExceeded);
        }

        unsafe {
            let header_size = std::mem::size_of::<PerfEventHeader>();
            let start_pos = (self.head + header_size as u64 + u64::from(offset)) & self.buf_mask;
            let end_pos = (start_pos + buf.len() as u64 - 1) & self.buf_mask;

            if end_pos < start_pos {
                // Data wraps around buffer end
                let first_len = self.data_len - start_pos as usize;
                ptr::copy_nonoverlapping(
                    self.data.add(start_pos as usize),
                    buf.as_mut_ptr(),
                    first_len,
                );
                ptr::copy_nonoverlapping(
                    self.data,
                    buf.as_mut_ptr().add(first_len),
                    buf.len() - first_len,
                );
            } else {
                // Data is contiguous
                ptr::copy_nonoverlapping(
                    self.data.add(start_pos as usize),
                    buf.as_mut_ptr(),
                    buf.len(),
                );
            }
        }

        Ok(())
    }

    /// Consumes the current event
    pub fn pop(&mut self) -> Result<(), PerfRingError> {
        if self.tail == self.head {
            return Err(PerfRingError::BufferEmpty);
        }

        unsafe {
            let header =
                &*(self.data.add((self.head & self.buf_mask) as usize) as *const PerfEventHeader);
            self.head += u64::from(header.size);
        }

        Ok(())
    }

    /// Finishes a read batch operation
    pub fn finish_read_batch(&mut self) {
        // Update tail position using atomic store
        unsafe {
            self.meta
                .as_ref()
                .data_tail
                .store(self.head, Ordering::Release);
        }
    }

    /// Returns the number of bytes available to read
    pub fn bytes_remaining(&self) -> u32 {
        ((self.tail - self.head) & self.buf_mask) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn test_init_contiguous() {
        let page_size = 4096u64;
        let n_pages = 2u32;
        let mut data = vec![0u8; (page_size * (1 + u64::from(n_pages))) as usize];

        // Valid initialization
        unsafe {
            let result = PerfRing::init_contiguous(&mut data, n_pages, page_size);
            assert!(result.is_ok());
        }

        // Invalid buffer size
        let mut small_data = vec![0u8; 7];
        unsafe {
            let result = PerfRing::init_contiguous(&mut small_data, 1, 7);
            assert!(result.is_err());
            match result {
                Err(PerfRingError::InvalidBufferLength) => {}
                _ => panic!("Expected InvalidBufferLength error"),
            }
        }

        // Nil buffer
        let mut empty_data = vec![];
        unsafe {
            let result = PerfRing::init_contiguous(&mut empty_data, n_pages, page_size);
            assert!(result.is_err());
            match result {
                Err(PerfRingError::NilBuffer) => {}
                _ => panic!("Expected NilBuffer error"),
            }
        }
    }

    #[test]
    fn test_write_and_read() {
        let page_size = 4096u64;
        let n_pages = 2u32;
        let mut data = vec![0u8; (page_size * (1 + u64::from(n_pages))) as usize];

        let mut ring = unsafe { PerfRing::init_contiguous(&mut data, n_pages, page_size).unwrap() };

        let test_data = b"test data";
        let event_type = 1u32;

        // Start write batch
        ring.start_write_batch();

        // Write data
        let offset = ring.write(test_data, event_type).unwrap();

        // Verify offset is within buffer bounds
        assert!(offset < (page_size * u64::from(n_pages)) as usize);

        // Finish write batch
        ring.finish_write_batch();

        // Start read batch
        ring.start_read_batch();

        // Check size
        let size = ring.peek_size().unwrap();
        let expected_size = 8 * (test_data.len().div_ceil(8));
        assert_eq!(size, expected_size);

        // Check type
        assert_eq!(ring.peek_type(), event_type);

        // Read data
        let mut read_buf = vec![0u8; size];
        ring.peek_copy(&mut read_buf, 0).unwrap();

        // Compare data (only the actual data part, not the padding)
        assert_eq!(&read_buf[..test_data.len()], test_data);

        // Pop the event
        ring.pop().unwrap();

        // Check remaining bytes (should be 0)
        assert_eq!(ring.bytes_remaining(), 0);

        // Finish read batch
        ring.finish_read_batch();
    }

    #[test]
    fn test_bytes_remaining() {
        let page_size = 4096u64;
        let n_pages = 2u32;
        let mut data = vec![0u8; (page_size * (1 + u64::from(n_pages))) as usize];

        let ring = unsafe { PerfRing::init_contiguous(&mut data, n_pages, page_size).unwrap() };

        let remaining = ring.bytes_remaining();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn test_wraparound() {
        let page_size = 4096u64;
        let n_pages = 2u32;
        let mut data = vec![0u8; (page_size * (1 + u64::from(n_pages))) as usize];

        let meta_ptr = data.as_mut_ptr() as *mut PerfEventMmapPage;
        unsafe {
            (*meta_ptr).data_offset = page_size;
            (*meta_ptr).data_size = page_size * u64::from(n_pages);
        }

        let mut ring = unsafe { PerfRing::init_contiguous(&mut data, n_pages, page_size).unwrap() };

        // Create test data that will wrap around the buffer
        let data_size = page_size as usize - size_of::<PerfEventHeader>() - 10;
        let mut test_data = vec![0u8; data_size];
        for (i, b) in test_data.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }

        ring.start_write_batch();

        // Write first chunk
        ring.write(&test_data, 1).unwrap();

        // Write second chunk
        ring.write(&test_data, 2).unwrap();

        ring.finish_write_batch();

        // Read and verify both chunks
        ring.start_read_batch();

        // Read first chunk
        let mut read_buf = vec![0u8; data_size];
        ring.peek_copy(&mut read_buf, 0).unwrap();
        assert_eq!(&read_buf[..], &test_data[..]);
        ring.pop().unwrap();

        ring.finish_read_batch();

        // There should now be space for one more event, that would wrap around the buffer. Write it.
        ring.start_write_batch();
        ring.write(&test_data, 3).unwrap();
        ring.finish_write_batch();

        // Now read the second and third chunks and verify they are correct
        ring.start_read_batch();

        // Read second chunk
        ring.peek_copy(&mut read_buf, 0).unwrap();
        assert_eq!(&read_buf[..], &test_data[..]);
        ring.pop().unwrap();

        // Read third chunk
        ring.peek_copy(&mut read_buf, 0).unwrap();
        assert_eq!(&read_buf[..], &test_data[..]);
        ring.pop().unwrap();

        ring.finish_read_batch();

        // Ring should be empty now
        assert_eq!(ring.bytes_remaining(), 0);
    }
}
