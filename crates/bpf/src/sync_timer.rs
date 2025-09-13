use anyhow::Result;
use log::{debug, error, info, warn};
use nix::sched::{sched_getaffinity, sched_getcpu, sched_setaffinity, CpuSet};
use nix::unistd::Pid;
use std::fs;
use std::io;
use thiserror::Error;

// Import the auto-generated enums from BPF skeleton
use crate::bpf::types::{sync_timer_init_error, sync_timer_mode};

impl sync_timer_mode {
    fn description(&self) -> &'static str {
        match self {
            sync_timer_mode::SYNC_TIMER_MODE_MODERN => {
                "modern CPU pinning + absolute time (kernel 6.7+)"
            }
            sync_timer_mode::SYNC_TIMER_MODE_INTERMEDIATE => {
                "intermediate absolute time only (kernel 6.4-6.6)"
            }
            sync_timer_mode::SYNC_TIMER_MODE_LEGACY => {
                "legacy relative time only (kernel 5.15-6.3)"
            }
        }
    }
}

/// Errors that can occur during sync timer initialization
#[derive(Error, Debug)]
pub enum SyncTimerError {
    #[error("Failed to get current CPU affinity")]
    AffinityGetFailed(#[source] nix::Error),

    #[error("Failed to get CPU count")]
    CpuCountFailed(#[source] libbpf_rs::Error),

    #[error("Failed to set CPU {} in CpuSet", cpu)]
    CpuSetFailed { cpu: usize },

    #[error("Failed to set CPU affinity to core {}", cpu)]
    AffinitySetFailed {
        cpu: usize,
        #[source]
        source: nix::Error,
    },

    #[error("Failed to get current CPU")]
    CurrentCpuFailed(#[source] nix::Error),

    #[error("Failed to run BPF init program on core {}", cpu)]
    BpfProgramFailed {
        cpu: usize,
        #[source]
        source: libbpf_rs::Error,
    },

    #[error("BPF map update failed on core {}", cpu)]
    MapUpdateFailed { cpu: usize },

    #[error("BPF map lookup failed after insertion on core {}", cpu)]
    MapLookupFailed { cpu: usize },

    #[error("BPF timer initialization failed on core {}", cpu)]
    TimerInitFailed { cpu: usize },

    #[error("BPF timer callback setup failed on core {}", cpu)]
    TimerSetCallbackFailed { cpu: usize },

    #[error("BPF timer start failed on core {}", cpu)]
    TimerStartFailed { cpu: usize },

    #[error("Unknown BPF error code {} on core {}", code, cpu)]
    UnknownBpfError { cpu: usize, code: u32 },

    #[error("Failed to pin to CPU {}. Currently on CPU {}", target, current)]
    CpuPinFailed { target: usize, current: usize },

    #[error("Failed to restore original CPU affinity")]
    AffinityRestoreFailed(#[source] nix::Error),

    #[error("Failed to initialize timer on {} out of {} cores: {:?}. This may indicate hardware/kernel limitations with BPF timers, insufficient permissions, or platform incompatibility.", failed_count, total_count, failed_cores)]
    MultipleFailures {
        failed_cores: Vec<usize>,
        failed_count: usize,
        total_count: usize,
    },

    #[error("Failed to read kernel.timer_migration sysctl")]
    SysctlReadFailed(#[source] io::Error),

    #[error("Failed to write kernel.timer_migration sysctl")]
    SysctlWriteFailed(#[source] io::Error),

    #[error("Failed to parse kernel.timer_migration value: {}", value)]
    SysctlParseFailed { value: String },

    #[error("All timer initialization methods failed (modern, intermediate, and legacy)")]
    AllMethodsFailed,
}

const TIMER_MIGRATION_SYSCTL_PATH: &str = "/proc/sys/kernel/timer_migration";

/// Read the current value of kernel.timer_migration sysctl
fn read_timer_migration_sysctl() -> Result<u8, SyncTimerError> {
    let content = fs::read_to_string(TIMER_MIGRATION_SYSCTL_PATH)
        .map_err(SyncTimerError::SysctlReadFailed)?;

    let value = content.trim();
    value
        .parse::<u8>()
        .map_err(|_| SyncTimerError::SysctlParseFailed {
            value: value.to_string(),
        })
}

/// Write a value to kernel.timer_migration sysctl
fn write_timer_migration_sysctl(value: u8) -> Result<(), SyncTimerError> {
    fs::write(TIMER_MIGRATION_SYSCTL_PATH, value.to_string())
        .map_err(SyncTimerError::SysctlWriteFailed)
}

/// Initializes and starts a synchronized timer on all available CPU cores with three-way fallback support
///
/// This function attempts to initialize BPF timers using three different methods in order of preference:
///
/// # Fallback Strategy
///
/// 1. **Modern Pinning (Kernel 6.7+)**: Uses `BPF_F_TIMER_CPU_PIN` + `BPF_F_TIMER_ABS` flags
/// 2. **Intermediate Pinning (Kernel 6.4-6.6)**: Uses `BPF_F_TIMER_ABS` flag only with timer migration control
/// 3. **Legacy Pinning (Kernel 5.15-6.3)**: Uses relative time with no flags and timer migration control
///
/// # Errors
///
/// Returns `SyncTimerError` with specific details about what failed:
/// - CPU affinity operations
/// - BPF program execution
/// - BPF timer setup (init, callback, start)
/// - BPF map operations
/// - Sysctl operations for intermediate/legacy fallback
///
/// # Example
///
/// ```rust,no_run
/// use bpf::BpfLoader;
/// use log::{error, info};
///
/// // Create loader with number of perf ring pages
/// let mut loader = BpfLoader::new(256).expect("Failed to create BpfLoader");
///
/// match loader.start_sync_timer() {
///     Ok(()) => info!("Sync timer initialized successfully"),
///     Err(e) => {
///         error!("Sync timer initialization failed: {}", e);
///         std::process::exit(1);
///     }
/// }
/// ```
pub fn initialize_sync_timer(
    timer_init_prog: &libbpf_rs::ProgramMut,
) -> Result<(), SyncTimerError> {
    info!("Initializing synchronized timer on all cores...");

    // Try modern pinning first (kernel 6.7+)
    debug!("Attempting modern timer initialization with CPU pinning + absolute time...");
    match initialize_timers_with_mode(timer_init_prog, sync_timer_mode::SYNC_TIMER_MODE_MODERN) {
        Ok(()) => {
            info!(
                "Successfully initialized timers using {}",
                sync_timer_mode::SYNC_TIMER_MODE_MODERN.description()
            );
            return Ok(());
        }
        Err(e) => {
            warn!("Modern timer initialization failed: {}", e);
            debug!("Falling back to intermediate timer initialization...");
        }
    }

    // Fall back to intermediate method (kernel 6.4-6.6)
    info!("Attempting intermediate timer initialization with absolute time only...");
    match initialize_timers_with_mode(
        timer_init_prog,
        sync_timer_mode::SYNC_TIMER_MODE_INTERMEDIATE,
    ) {
        Ok(()) => {
            info!(
                "Successfully initialized timers using {}",
                sync_timer_mode::SYNC_TIMER_MODE_INTERMEDIATE.description()
            );
            return Ok(());
        }
        Err(e) => {
            warn!("Intermediate timer initialization failed: {}", e);
            debug!("Falling back to legacy timer initialization...");
        }
    }

    // Fall back to legacy method (kernel 5.15-6.3)
    info!("Attempting legacy timer initialization with relative time only...");
    match initialize_timers_with_mode(timer_init_prog, sync_timer_mode::SYNC_TIMER_MODE_LEGACY) {
        Ok(()) => {
            info!(
                "Successfully initialized timers using {}",
                sync_timer_mode::SYNC_TIMER_MODE_LEGACY.description()
            );
            Ok(())
        }
        Err(e) => {
            error!("Legacy timer initialization also failed: {}", e);
            Err(SyncTimerError::AllMethodsFailed)
        }
    }
}

/// Initialize timers with specified mode
fn initialize_timers_with_mode(
    timer_init_prog: &libbpf_rs::ProgramMut,
    mode: sync_timer_mode,
) -> Result<(), SyncTimerError> {
    let mut original_migration = None;

    // For intermediate and legacy modes, temporarily disable timer migration
    if matches!(
        mode,
        sync_timer_mode::SYNC_TIMER_MODE_INTERMEDIATE | sync_timer_mode::SYNC_TIMER_MODE_LEGACY
    ) {
        let current_migration = read_timer_migration_sysctl()?;
        debug!(
            "Current kernel.timer_migration value: {}",
            current_migration
        );

        if current_migration != 0 {
            debug!(
                "Temporarily disabling timer migration for {} mode...",
                mode.description()
            );
            write_timer_migration_sysctl(0)?;
            original_migration = Some(current_migration);
        }
    }

    // Initialize timers on all cores
    let result = initialize_timers_on_all_cores(timer_init_prog, mode);

    // Restore original timer migration setting if we changed it
    if let Some(original_value) = original_migration {
        debug!(
            "Restoring original timer migration setting: {}",
            original_value
        );
        if let Err(e) = write_timer_migration_sysctl(original_value) {
            error!("Failed to restore timer migration setting: {}", e);
            // Don't fail the entire operation for this
        }
    }

    result
}

/// Core timer initialization logic shared by all modes
fn initialize_timers_on_all_cores(
    timer_init_prog: &libbpf_rs::ProgramMut,
    mode: sync_timer_mode,
) -> Result<(), SyncTimerError> {
    // Get current thread's CPU affinity to restore it later
    let current_pid = Pid::from_raw(0); // 0 means the current thread
    let original_cpu_set =
        sched_getaffinity(current_pid).map_err(SyncTimerError::AffinityGetFailed)?;

    // Determine the number of available CPUs
    let num_possible_cpus =
        libbpf_rs::num_possible_cpus().map_err(SyncTimerError::CpuCountFailed)?;

    debug!("Found {} CPU cores", num_possible_cpus);

    // Track any failed initializations
    let mut failed_cores = Vec::new();

    // Initialize timer on each core sequentially
    for cpu_id in 0..num_possible_cpus {
        if let Err(e) = initialize_timer_on_core(timer_init_prog, cpu_id, current_pid, mode) {
            debug!(
                "Timer initialization failed on core {} with strategy {} (this is one of multiple fallback attempts): {}",
                cpu_id, mode.description(), e
            );
            failed_cores.push(cpu_id);
        } else {
            debug!("Timer initialization succeeded on core {}", cpu_id);
        }
    }

    // Restore original CPU affinity
    sched_setaffinity(current_pid, &original_cpu_set)
        .map_err(SyncTimerError::AffinityRestoreFailed)?;

    // Check if any cores failed initialization
    if !failed_cores.is_empty() {
        return Err(SyncTimerError::MultipleFailures {
            failed_cores: failed_cores.clone(),
            failed_count: failed_cores.len(),
            total_count: num_possible_cpus,
        });
    }

    debug!(
        "Synchronized timer initialized on {} cores using {}",
        num_possible_cpus,
        mode.description()
    );
    Ok(())
}

/// Initialize timer on a specific CPU core
fn initialize_timer_on_core(
    timer_init_prog: &libbpf_rs::ProgramMut,
    cpu_id: usize,
    current_pid: Pid,
    mode: sync_timer_mode,
) -> Result<(), SyncTimerError> {
    // Create a CPU set with just this core
    let mut cpu_set = CpuSet::new();
    cpu_set
        .set(cpu_id)
        .map_err(|_| SyncTimerError::CpuSetFailed { cpu: cpu_id })?;

    // Set CPU affinity for the current thread
    sched_setaffinity(current_pid, &cpu_set).map_err(|e| SyncTimerError::AffinitySetFailed {
        cpu: cpu_id,
        source: e,
    })?;

    // Verify we're running on the correct CPU
    let current_cpu = sched_getcpu().map_err(SyncTimerError::CurrentCpuFailed)?;

    if current_cpu as usize != cpu_id {
        warn!(
            "Failed to pin to CPU {}. Currently on CPU {}",
            cpu_id, current_cpu
        );
        return Err(SyncTimerError::CpuPinFailed {
            target: cpu_id,
            current: current_cpu as usize,
        });
    }

    debug!(
        "Initializing timer on CPU {} using {}",
        cpu_id,
        mode.description()
    );

    // Create input context with mode parameter
    // We'll use the bpf_sock_addr structure to pass the mode
    let mut context_in = [0u8; 16];

    // Set up the context to pass the mode parameter
    // We'll use the first 4 bytes to simulate user_family = AF_INET (2)
    // and the next 4 bytes for user_ip4 containing our mode
    context_in[0] = 2; // AF_INET
    context_in[1] = 0;
    context_in[2] = 0;
    context_in[3] = 0;
    context_in[4] = mode as u8; // mode parameter in user_ip4
    context_in[5] = 0;
    context_in[6] = 0;
    context_in[7] = 0;

    let input = libbpf_rs::ProgramInput {
        context_in: Some(&mut context_in),
        ..Default::default()
    };

    // Run the initialization program on this core
    let output = timer_init_prog
        .test_run(input)
        .map_err(|e| SyncTimerError::BpfProgramFailed {
            cpu: cpu_id,
            source: e,
        })?;

    // Check return value using the auto-generated enum
    if output.return_value != sync_timer_init_error::SYNC_TIMER_SUCCESS as u32 {
        return match output.return_value {
            v if v == sync_timer_init_error::SYNC_TIMER_MAP_UPDATE_FAILED as u32 => {
                Err(SyncTimerError::MapUpdateFailed { cpu: cpu_id })
            }
            v if v == sync_timer_init_error::SYNC_TIMER_MAP_LOOKUP_FAILED as u32 => {
                Err(SyncTimerError::MapLookupFailed { cpu: cpu_id })
            }
            v if v == sync_timer_init_error::SYNC_TIMER_TIMER_INIT_FAILED as u32 => {
                Err(SyncTimerError::TimerInitFailed { cpu: cpu_id })
            }
            v if v == sync_timer_init_error::SYNC_TIMER_TIMER_SET_CALLBACK_FAILED as u32 => {
                Err(SyncTimerError::TimerSetCallbackFailed { cpu: cpu_id })
            }
            v if v == sync_timer_init_error::SYNC_TIMER_TIMER_START_FAILED as u32 => {
                Err(SyncTimerError::TimerStartFailed { cpu: cpu_id })
            }
            unknown => Err(SyncTimerError::UnknownBpfError {
                cpu: cpu_id,
                code: unknown,
            }),
        };
    }

    Ok(())
}
