# Linux Resource Control (resctrl) Operations Guide

This guide documents our **tested implementation** of Linux Resource Control (resctrl) for Intel RDT (Resource Director Technology) monitoring and allocation. The examples are based on our validated GitHub Actions workflow that demonstrates practical resource control with real workloads.

## Important Note: resctrl vs cgroup

This guide focuses on **Intel RDT/resctrl** (`/sys/fs/resctrl`), which provides hardware-level resource control using Intel RDT features like Cache Allocation Technology (CAT), Memory Bandwidth Allocation (MBA), and Cache Monitoring Technology (CMT). This is different from **cgroup resource control** (`/sys/fs/cgroup`), which provides software-level scheduling controls.

For comprehensive cgroup v2 resource control, see the [Facebook resctl-demo project](https://github.com/facebookexperimental/resctl-demo), which demonstrates cgroup-based resource protection and memory management.

## Table of Contents

1. [Hardware Requirements and Setup](#hardware-requirements-and-setup)
2. [Our Tested Implementation](#our-tested-implementation)
3. [Resource Control Workflow](#resource-control-workflow)
4. [Measurement and Monitoring](#measurement-and-monitoring)
5. [Untested Capabilities](#untested-capabilities)
6. [References](#references)

## Hardware Requirements and Setup

### Verify Hardware Support

```bash
# Check CPU features for RDT support
grep -E "rdt_a|cat_l3|cqm_llc|cqm_occup_llc|cqm_mbm_total|cqm_mbm_local|mba" /proc/cpuinfo

# Check kernel support
cat /proc/filesystems | grep resctrl
```

### Mount resctrl Filesystem

```bash
# Mount resctrl filesystem (basic mount we use)
sudo mkdir -p /sys/fs/resctrl
sudo mount -t resctrl resctrl /sys/fs/resctrl

# Verify mount
mount | grep resctrl
```

### Check Available Capabilities

```bash
# Hardware capabilities we actually use
echo "CLOSIDs: $(cat /sys/fs/resctrl/info/L3/num_closids)"
echo "RMIDs: $(cat /sys/fs/resctrl/info/L3_MON/num_rmids)"
echo "Cache mask: $(cat /sys/fs/resctrl/info/L3/cbm_mask)"
echo "Min bandwidth: $(cat /sys/fs/resctrl/info/MB/min_bandwidth)%"
```

## Our Tested Implementation

### Workload Design

Our implementation demonstrates resource control using two distinct workload types:

1. **Memory Bandwidth Intensive**: Uses `stress-ng --vm X --vm-bytes 75%` to create high memory bandwidth demand
2. **Cache Sensitive**: Uses `stress-ng --cache X --cache-size XM --perf --metrics-brief` to create measurable cache performance patterns

### Resource Group Creation

```bash
# Create two resource control groups (our tested approach)
sudo mkdir -p /sys/fs/resctrl/memory_bandwidth_group
sudo mkdir -p /sys/fs/resctrl/cache_sensitive_group

# CPU assignment strategy: quarters of total CPUs for isolation
# Memory bandwidth group gets first quarter (CPUs 0 to N/4-1)
# Cache sensitive group gets second quarter (CPUs N/4 to N/2-1)
# Remaining CPUs left unassigned for isolation
```

### Stress Testing Configuration

**Memory Bandwidth Workload:**
- Command: `stress-ng --vm [threads] --vm-bytes 75%`
- Thread count: Half of assigned CPU quarter (1/8 of total system CPUs)
- Duration: Long-running (30 minutes) to maintain consistent memory pressure

**Cache Sensitive Workload:**
- Command: `stress-ng --cache [threads] --cache-size [size]M --perf --metrics-brief`
- Thread count: 1/8 of assigned CPU quarter
- Cache size: 1MB per assigned CPU, distributed across threads
- Duration: 17 seconds with 10s warmup + 5s measurement + 2s buffer

## Resource Control Workflow

Our tested workflow demonstrates five phases of resource control:

### Phase 1: Baseline Cache Performance
- **Objective**: Measure cache workload performance without memory bandwidth contention
- **Configuration**: Cache workload only, no memory bandwidth stress
- **Measurements**: Cache references, misses, hit rates, LLC occupancy

### Phase 2: Both Workloads Unthrottled
- **Objective**: Demonstrate resource contention without controls
- **Configuration**: Both workloads running with full resource access
- **Expected Result**: Cache performance degradation due to memory bandwidth interference

### Phase 3: Memory Bandwidth Throttling
- **Objective**: Demonstrate Memory Bandwidth Allocation (MBA)
- **Configuration**: Memory bandwidth limited to 20% via schemata
- **Implementation**: Careful modification preserving existing L3 settings

### Phase 4: Combined Memory and Cache Restrictions
- **Objective**: Demonstrate Cache Allocation Technology (CAT) with MBA
- **Configuration**: Memory at 20% + cache restricted to first 4 ways
- **Implementation**: Careful modification preserving existing MB settings

### Phase 5: Resource Restoration
- **Objective**: Verify performance recovery when restrictions are removed
- **Configuration**: Restore full resources to both groups
- **Expected Result**: Cache performance returns to near-baseline levels

### Careful Schemata Modification

Our implementation takes a conservative approach when modifying resource allocations: we read the existing schemata, modify only the specific resource line we need to change, and write it back. This preserves existing settings and prevents accidentally overwriting other resource allocations during phase transitions, however we did not check if this was strictly necessary.

## Measurement and Monitoring

### Key Metrics We Collect

**Memory Bandwidth Monitoring:**
- `mbm_total_bytes`: Total memory bandwidth (cumulative)
- `mbm_local_bytes`: Local memory bandwidth (cumulative)
- Calculated rates: `(final_value - initial_value) / measurement_duration`

**Cache Performance Monitoring:**
- `llc_occupancy`: Last Level Cache occupancy in bytes
- Cache references (total LLC accesses) and misses from `stress-ng --perf`
- Cache hit rate: `(references - misses) / references * 100`
- Throughput: `stress-ng` bogo operations per second

**Note on Cache Counters**: Per Intel SDM, LLC references and misses may include speculation and L1 hardware prefetcher activity, but may exclude other hardware prefetchers. Value comparison for performance estimation across different systems is not recommended due to implementation-specific characteristics.

**Data Collection Format:**
```csv
phase,workload_type,llc_occupancy,memory_bandwidth_total,memory_bandwidth_local,cache_references_gbps,cache_misses_gbps,cache_hit_rate,bogo_ops
```

### Monitoring File Locations

```bash
# LLC occupancy for each group
/sys/fs/resctrl/[group_name]/mon_data/mon_L3_00/llc_occupancy

# Memory bandwidth counters
/sys/fs/resctrl/[group_name]/mon_data/mon_L3_00/mbm_total_bytes
/sys/fs/resctrl/[group_name]/mon_data/mon_L3_00/mbm_local_bytes

# Resource allocation settings
/sys/fs/resctrl/[group_name]/schemata
```

## Untested Capabilities

resctrl supports additional features we haven't tested:

### Mount Options (Considered but Not Tested)
- `cdp,cdpl2`: Code/Data Prioritization for L3 and L2 caches
- `mba_MBps`: Memory bandwidth allocation in MBps instead of percentage

### Advanced Monitoring (Available but Not Implemented)
- **Monitoring subgroups**: `mon_groups/` for finer-grained RMID allocation
- **Multi-domain systems**: Our implementation assumes single domain (domain 0)
- **L2 cache monitoring**: Focus was on L3 cache allocation and monitoring

### Alternative Stress Testing Approaches (Considered)
Other `stress-ng` options like `--matrix`, `--stream`, and `--cpu` methods could provide different stress patterns but weren't necessary for our demonstration.

### Programming Interface (Available but Not Used)
Tools like [intel-cmt-cat](https://github.com/intel/intel-cmt-cat) provide C/C++ APIs for resctrl operations. Our shell-based approach was sufficient for demonstration purposes.

## References

### Workflow Implementation
- **GitHub Actions Workflow**: `.github/workflows/resctrl-demo.yaml` - Complete tested implementation

### Related Projects and Documentation
- **Linux Kernel Documentation**: [resctrl.rst](https://www.kernel.org/doc/Documentation/x86/resctrl.rst)
- **Facebook resctl-demo**: [GitHub Repository](https://github.com/facebookexperimental/resctl-demo) - Comprehensive cgroup-based resource control (different from resctrl)
- **Intel RDT Tools**: [intel-cmt-cat](https://github.com/intel/intel-cmt-cat) - Intel's official RDT user-space tools
- **stress-ng Documentation**: [Ubuntu stress-ng Reference](https://wiki.ubuntu.com/Kernel/Reference/stress-ng)
