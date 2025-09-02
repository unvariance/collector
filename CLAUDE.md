# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

The Noisy Neighbor Collector is a Kubernetes-native monitoring system that identifies and quantifies memory subsystem interference between pods. It uses eBPF to collect high-resolution (1ms) performance metrics including CPU cycles, instructions, LLC misses, and cache occupancy via Intel RDT.

## Development Workflow

### Git Workflow and Contribution Process

This project uses a pull request (PR) based workflow with squash-and-merge strategy:

1. **Ensure upstream remote is configured**:
   ```bash
   # Check if upstream exists
   git remote -v
   
   # If not, add upstream
   git remote add upstream https://github.com/unvariance/collector.git
   ```

2. **Create a feature branch** for your changes:
   ```bash
   git checkout -b feature/your-feature-name
   ```

3. **Make commits** on your branch (these will be squashed later)

4. **Push to your fork**:
   ```bash
   git push -u origin feature/your-feature-name
   ```

5. **Submit a Pull Request to upstream** (`unvariance/collector`):
   ```bash
   # Create PR against upstream repository
   gh pr create --repo unvariance/collector --base main --head YOUR_GITHUB_USERNAME:feature/your-feature-name
   ```

6. **Squash and merge**: The upstream repository uses squash-and-merge, combining all commits into a single commit

7. **Commit message format**: The project follows [Conventional Commits](https://www.conventionalcommits.org/) format:
   ```
   <type>(<scope>): <description>
   
   Examples:
   fix(collector): resolve memory leak in parquet writer
   feat(ebpf): add support for kernel 6.11
   docs(readme): update installation instructions
   test(bpf): add coverage for cgroup tracking
   refactor(timeslot): optimize aggregation performance
   ```

   Common types: `feat`, `fix`, `docs`, `test`, `refactor`, `build`, `ci`, `chore`

### Helper Scripts for Git Management
```bash
# Create a new branch (interactive script)
./scripts/git/create-branch.sh

# Sync with upstream
./scripts/git/sync-upstream.sh

# Clean up merged branches
./scripts/git/clean-branches.sh
./scripts/git/clean-squashed-branches.sh
```

## Common Development Commands

### Build Commands
```bash
# Build the Rust collector (requires Rust toolchain, clang, libelf-dev)
cargo build
cargo build --release

# Build specific crates
cargo build -p collector
cargo build -p bpf
cargo build -p trace-analysis

# Build sync timer benchmark (Go implementation)
go build ./cmd/sync_timer_benchmark
```

### Test Commands
```bash
# Run all Rust tests
cargo test

# Test cgroup inode assumptions (critical for container identification)
cargo test --package bpf --test cgroup_inode_test

# Test specific components
cargo test -p collector
cargo test -p perf_events
cargo test -p timeslot

# Run tests that require elevated privileges (CI runs these)
sudo cargo test --package bpf --test cgroup_inode_test
```

### Running the Collector
```bash
# Run the collector (requires root for eBPF)
sudo ./target/debug/collector

# Run with options
sudo ./target/debug/collector -d 30  # Run for 30 seconds
sudo ./target/debug/collector --verbose  # Enable debug output
sudo ./target/debug/collector --trace  # Output raw telemetry events (nanosecond granularity)
sudo ./target/debug/collector --storage-type local --prefix "/tmp/metrics-"
sudo ./target/debug/collector --storage-type s3 --bucket your-bucket --region us-west-2
```

### Sync Timer Benchmark
```bash
# Build and run sync timer benchmark (measures timer jitter across cores)
go build ./cmd/sync_timer_benchmark
sudo ./sync_timer_benchmark -duration 10s -csv results.csv -experiment baseline

# Run with stress generators (as done in CI)
stress-ng --cpu 4 --cpu-load 80 &
sudo ./sync_timer_benchmark -duration 10s -csv results.csv -experiment cpu_stress
```

### Linting and Formatting
```bash
# Rust formatting and linting
cargo fmt
cargo clippy

# Check Go code (for sync timer benchmark)
go fmt ./cmd/sync_timer_benchmark
go vet ./cmd/sync_timer_benchmark
```

### Parquet Analysis Scripts
```bash
# Plot various metrics from collected parquet files
./scripts/plot_parquet.sh <parquet_file>
./scripts/plot_cpi_by_llc_misses.R <parquet_file>
./scripts/plot_cpu_utilization.R <parquet_file>
./scripts/plot_memory_utilization.R <parquet_file>
./scripts/plot_workload_performance.R <parquet_file>
```

## High-Level Architecture

### Core Data Collection Pipeline (Rust)

1. **eBPF Layer** (`crates/bpf/`)
   - Sets up high-resolution timers (1ms intervals) synchronized across all CPUs
   - Captures performance counters (cycles, instructions, LLC misses) per process
   - Tracks task lifecycle (creation, termination) and metadata
   - Outputs events to per-CPU perf ring buffers

2. **Userspace Collector** (`crates/collector/`)
   - Processes events from eBPF via perf ring buffers
   - Aggregates measurements into 1ms time slots
   - Maintains process metadata and state
   - Writes data to Parquet files for analysis
   - Supports multiple storage backends (local, S3, GCS, Azure)

3. **Analysis Tools** (`crates/trace-analysis/`)
   - Analyzes collected parquet files
   - Performs concurrency analysis
   - Identifies hyperthread interference patterns
   - Validates data monotonicity

### Key Data Flow

1. **Timer Synchronization**: eBPF timers fire → read perf counters → send to userspace
2. **Data Aggregation**: Raw events → 1ms time slots → Parquet batches → storage backend
3. **Per-CPU Ring Buffers**: Minimizes lock contention in high-throughput collection

### Important Design Decisions

- **1ms Granularity**: Captures interference at meaningful timescale for cloud workloads
- **Parquet Format**: Efficient columnar storage for time-series analysis
- **eBPF-based Timers**: More efficient than kernel module approach
- **Per-CPU Processing**: Scales linearly with core count

## Container Metadata Integration

The collector integrates with Kubernetes through:
- Container ID extraction from cgroups (validated via `cgroup_inode_test`)
- Process-to-container mapping via `/proc/<pid>/cgroup`
- Future NRI (Node Resource Interface) integration for pod/container metadata

## CI/CD Testing

GitHub Actions workflows validate the collector across multiple scenarios:

1. **test-ebpf-collector.yaml**: 
   - Tests on multiple kernel versions (5.15, 6.1, 6.8, 6.10)
   - Verifies cgroup inode assumptions
   - Runs collector with local storage

2. **benchmark.yaml**:
   - Deploys OpenTelemetry demo application on K3s
   - Runs collector alongside workload
   - Collects performance metrics to S3

3. **test-kernel-module.yaml** (deprecated kernel module):
   - Tests sync timer benchmark with stress-ng workloads
   - Measures timer jitter under various conditions

## Performance Considerations

- CPU overhead: ~1% for eBPF, ~0.75% for userspace processing
- Memory usage: ~300MB per node
- Storage: ~100MB/hour of collected data (configurable quota)
- Kernel requirements: Minimum 5.15, optimal 6.7+ for sync timer support

## Deprecated Components

The following components are deprecated but remain in the repository:
- **Kernel Module** (`module/`): Original RMID allocation and timer implementation
- **Go Collector** (`cmd/collector/`): Early prototype implementation

The main production implementation is the Rust-based collector with eBPF.