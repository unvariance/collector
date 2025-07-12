# Trace Analysis

A Rust-based analysis system that processes raw trace Parquet files and outputs derived metrics to understand CPI variability, with a focus on hyperthread contention analysis.

## Overview

This crate analyzes how peer hyperthread activity correlates with performance measurements by tracking three types of peer hyperthread states:
- **Same Process** - Peer hyperthread runs the same process
- **Different Process** - Peer hyperthread runs a different process  
- **Kernel** - Peer hyperthread runs kernel code

## Usage

### Basic Analysis

```bash
# Analyze a trace file
cargo run --bin trace-analysis -- -f trace_data.parquet

# Specify custom output prefix
cargo run --bin trace-analysis -- -f trace_data.parquet --output-prefix my_analysis
```

### Analysis + Visualization

```bash
# Run analysis first
cargo run --bin trace-analysis -- -f trace_data.parquet --output-prefix my_analysis

# Then generate plots
Rscript plot/hyperthread_cpi_histogram.R my_analysis_hyperthread_analysis.parquet
```

## Input Requirements

The input Parquet file must contain:
- **Metadata**: `num_cpus` key-value pair indicating CPU count
- **Columns**:
  - `timestamp` (Int64) - Event timestamp in nanoseconds
  - `cpu_id` (Int32) - CPU ID where event occurred
  - `is_context_switch` (Boolean) - Whether event is a context switch
  - `next_tgid` (Int32, nullable) - Process ID being switched to (required for context switches)
  - `cycles` (Int64) - CPU cycles measured
  - `instructions` (Int64) - Instructions executed

## Output

The analysis produces an augmented Parquet file with three additional columns:
- `ns_peer_same_process` - Nanoseconds peer hyperthread spent in same process
- `ns_peer_different_process` - Nanoseconds peer hyperthread spent in different process  
- `ns_peer_kernel` - Nanoseconds peer hyperthread spent in kernel

## Hyperthread Pairing Logic

CPUs are paired as hyperthreads using the topology:
- CPU `i` pairs with CPU `i + num_cpus/2`
- Example with 8 CPUs: (0,4), (1,5), (2,6), (3,7)

## Algorithm

For each event:
1. **Update counters** - Synchronize hyperthread counters for both CPUs up to event timestamp
2. **Record values** - Output current counter values with the event
3. **Update state** - Update CPU state if it's a context switch
4. **Reset counters** - Zero counters after recording

Key behaviors:
- **Initial state**: CPUs start with unknown state (`None`), producing zero counters until first context switch
- **Unknown state handling**: No time attribution when either CPU state is unknown
- **Error on null `next_tgid`**: Context switches must specify the incoming process

## Plotting

The `plot/` directory contains R scripts for visualization:

### CPI Histogram Analysis

Generates probability density plots showing CPI (cycles per instruction) distributions for the top 20 processes, categorized by peer hyperthread activity.

**Features:**
- Instruction-weighted distributions (not event counts)
- Normalized probability densities
- Separate lines for each hyperthread category
- Faceted by process (top 20 by instruction count)

**Requirements:**
```r
install.packages(c("nanoparquet", "dplyr", "ggplot2", "tidyr", "stringr"))
```

See `plot/README.md` for detailed plotting documentation.

## Testing

Run the test suite:
```bash
cargo test --bin trace-analysis
```

Tests cover:
- Initial state producing zero counters
- Hyperthread counter logic and timing
- Same/different process detection  
- Error handling for malformed data
- Null value handling

## Architecture

- **`main.rs`** - CLI interface and file processing coordination
- **`hyperthread_analysis.rs`** - Core analysis logic and Parquet I/O
- **`plot/`** - Visualization scripts and utilities