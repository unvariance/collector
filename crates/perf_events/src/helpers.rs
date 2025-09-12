//! Perf event utilities for eBPF maps.
//!
//! This module provides functions for opening perf events and
//! setting them up for use with eBPF maps.

use libbpf_rs::{MapCore as _, MapMut};
use perf_event_open_sys as sys;
use std::io;

/// Error type for perf event operations
#[derive(Debug, thiserror::Error)]
pub enum PerfEventError {
    /// Error opening perf event
    #[error("failed to open perf event on CPU {cpu}: {source}")]
    OpenError {
        /// CPU where the error occurred
        cpu: i32,
        /// Source error
        source: io::Error,
    },

    /// Error updating map
    #[error("failed to update map for CPU {cpu}: {source}")]
    MapUpdateError {
        /// CPU where the error occurred
        cpu: i32,
        /// Source error
        source: libbpf_rs::Error,
    },

    /// Error enabling perf event
    #[error("failed to enable perf event: {0}")]
    EnableError(io::Error),

    /// Error getting map info
    #[error("failed to get map info: {0}")]
    MapInfoError(libbpf_rs::Error),
}

/// Opens perf events for each CPU and returns a vector of file descriptors.
///
/// # Arguments
///
/// * `n_cpu` - Number of CPUs to open events for
/// * `attr` - Perf event attributes
///
/// # Returns
///
/// * `Ok(Vec<i32>)` - Vector of file descriptors on success
/// * `Err(PerfEventError)` on failure
///
pub fn open_perf_events(
    n_cpu: i32,
    attr: &mut sys::bindings::perf_event_attr,
) -> Result<Vec<i32>, PerfEventError> {
    let mut fds = Vec::with_capacity(n_cpu as usize);

    // Open perf events for each CPU
    for cpu in 0..n_cpu {
        // Open perf event
        let fd = unsafe {
            sys::perf_event_open(
                attr,
                -1, // pid (all threads)
                cpu,
                -1, // group_fd
                sys::bindings::PERF_FLAG_FD_CLOEXEC as u64,
            )
        };

        if fd < 0 {
            // Close any already opened file descriptors
            for &open_fd in &fds {
                unsafe {
                    libc::close(open_fd);
                }
            }
            return Err(PerfEventError::OpenError {
                cpu,
                source: io::Error::last_os_error(),
            });
        }

        fds.push(fd);
    }

    Ok(fds)
}

/// Updates a map with file descriptors for each CPU.
///
/// # Arguments
///
/// * `map` - A mutable reference to a libbpf-rs map to store the file descriptors
/// * `fds` - Vector of file descriptors to store in the map
///
/// # Returns
///
/// * `Ok(())` on success
/// * `Err(PerfEventError)` on failure
///
pub fn update_map_with_fds(map: &mut MapMut, fds: &[i32]) -> Result<(), PerfEventError> {
    for (cpu, &fd) in fds.iter().enumerate() {
        // Store FD in map
        // Convert cpu to u32 for key and fd to u32 for value
        let key = (cpu as u32).to_le_bytes();
        let value = (fd as u32).to_le_bytes();

        if let Err(err) = map.update(&key, &value, libbpf_rs::MapFlags::ANY) {
            // Don't close FDs here as they are still owned by the caller
            return Err(PerfEventError::MapUpdateError {
                cpu: cpu as i32,
                source: err,
            });
        }
    }

    Ok(())
}

/// Opens perf events for each CPU and updates the provided map with the file descriptors.
///
/// # Arguments
///
/// * `map` - A mutable reference to a libbpf-rs map to store the file descriptors
/// * `attr` - Perf event attributes
///
/// # Returns
///
/// * `Ok(())` on success
/// * `Err(PerfEventError)` on failure
///
/// # Example
///
/// ```no_run
/// use perf_events;
/// use libbpf_rs::MapMut;
/// use perf_event_open_sys as sys;
///
/// fn example(map: &mut MapMut) -> Result<(), perf_events::PerfEventError> {
///     // Configure perf event attributes - similar to MmapStorage::new
///     let mut attr = sys::bindings::perf_event_attr::default();
///     attr.size = std::mem::size_of::<sys::bindings::perf_event_attr>() as u32;
///     attr.type_ = sys::bindings::PERF_TYPE_SOFTWARE;
///     attr.config = sys::bindings::PERF_COUNT_SW_BPF_OUTPUT as u64;
///     attr.sample_type = sys::bindings::PERF_SAMPLE_RAW as u64;
///     
///     // Configure watermark behavior
///     let n_watermark_bytes = 0; // Wake up on every event
///     if n_watermark_bytes > 0 {
///         attr.set_watermark(1);
///         attr.__bindgen_anon_2.wakeup_watermark = n_watermark_bytes;
///     } else {
///         attr.__bindgen_anon_2.wakeup_events = 1;
///     }
///     
///     perf_events::open_events(map, &mut attr)?;
///     perf_events::start_events(map)?;
///     
///     Ok(())
/// }
/// ```
pub fn open_events(
    map: &mut MapMut,
    attr: &mut sys::bindings::perf_event_attr,
) -> Result<(), PerfEventError> {
    // Determine number of CPUs from map max entries
    let n_cpu = map
        .info()
        .map(|info| info.info.max_entries as i32)
        .map_err(PerfEventError::MapInfoError)?;

    // Open perf events for each CPU and get file descriptors
    let fds = open_perf_events(n_cpu, attr)?;

    // Update the map with the file descriptors
    match update_map_with_fds(map, &fds) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Clean up file descriptors on error
            for fd in fds {
                unsafe {
                    libc::close(fd);
                }
            }
            Err(e)
        }
    }
}

/// Types of hardware performance counters
#[derive(Debug, Clone, Copy)]
pub enum HardwareCounter {
    /// CPU cycles
    Cycles,
    /// CPU instructions
    Instructions,
    /// Last Level Cache misses
    LLCMisses,
    /// Cache references
    CacheReferences,
}

/// Opens a hardware performance counter for each CPU and updates the provided map with the file descriptors.
///
/// # Arguments
///
/// * `map` - A mutable reference to a libbpf-rs map to store the file descriptors
/// * `counter_type` - Type of hardware counter to open
///
/// # Returns
///
/// * `Ok(())` on success
/// * `Err(PerfEventError)` on failure
///
/// # Example
///
/// ```no_run
/// use perf_events::{self, HardwareCounter};
/// use libbpf_rs::MapMut;
///
/// fn example(cycles_map: &mut MapMut, instr_map: &mut MapMut) -> Result<(), perf_events::PerfEventError> {
///     // Open cycles counter
///     perf_events::open_perf_counter(cycles_map, HardwareCounter::Cycles)?;
///     
///     // Open instructions counter
///     perf_events::open_perf_counter(instr_map, HardwareCounter::Instructions)?;
///     
///     // Start the events
///     perf_events::start_events(cycles_map)?;
///     perf_events::start_events(instr_map)?;
///     
///     Ok(())
/// }
/// ```
pub fn open_perf_counter(
    map: &mut MapMut,
    counter_type: HardwareCounter,
) -> Result<(), PerfEventError> {
    // Set counter-specific configuration
    let config = match counter_type {
        HardwareCounter::Cycles => sys::bindings::PERF_COUNT_HW_CPU_CYCLES as u64,
        HardwareCounter::Instructions => sys::bindings::PERF_COUNT_HW_INSTRUCTIONS as u64,
        HardwareCounter::LLCMisses => sys::bindings::PERF_COUNT_HW_CACHE_MISSES as u64,
        HardwareCounter::CacheReferences => sys::bindings::PERF_COUNT_HW_CACHE_REFERENCES as u64,
    };

    // Create and configure perf event attributes
    let mut attr = perf_event_open_sys::bindings::perf_event_attr {
        size: std::mem::size_of::<sys::bindings::perf_event_attr>() as u32,
        type_: sys::bindings::PERF_TYPE_HARDWARE,
        read_format: (sys::bindings::PERF_FORMAT_TOTAL_TIME_ENABLED
            | sys::bindings::PERF_FORMAT_TOTAL_TIME_RUNNING) as u64,
        config,
        ..Default::default()
    };

    // Open the events
    open_events(map, &mut attr)
}

/// Enables all perf events stored in the map.
///
/// # Arguments
///
/// * `map` - A reference to a libbpf-rs map containing perf event file descriptors
///
/// # Returns
///
/// * `Ok(())` on success
/// * `Err(PerfEventError)` on failure
pub fn start_events(map: &MapMut) -> Result<(), PerfEventError> {
    // Determine number of CPUs from map max entries
    let n_cpu = map
        .info()
        .map(|info| info.info.max_entries as i32)
        .map_err(PerfEventError::MapInfoError)?;

    // Iterate through each CPU's file descriptor and enable the perf event
    for cpu in 0..n_cpu {
        let key = (cpu as u32).to_le_bytes();

        if let Some(value) = map.lookup(&key, libbpf_rs::MapFlags::ANY).unwrap_or(None) {
            if value.len() >= 4 {
                // Convert bytes to file descriptor
                let fd = u32::from_le_bytes([value[0], value[1], value[2], value[3]]) as i32;

                // Enable the perf event
                let ret = unsafe { libc::ioctl(fd, sys::bindings::ENABLE as libc::c_ulong, 0) };

                if ret < 0 {
                    return Err(PerfEventError::EnableError(io::Error::last_os_error()));
                }
            }
        }
    }

    Ok(())
}
