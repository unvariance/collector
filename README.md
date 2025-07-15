[![Collector](https://github.com/unvariance/collector/actions/workflows/test-ebpf-collector.yaml/badge.svg)](https://github.com/unvariance/collector/actions/workflows/test-ebpf-collector.yaml)
[![Helm Chart](https://github.com/unvariance/collector/actions/workflows/test-helm-chart.yaml/badge.svg)](https://github.com/unvariance/collector/actions/workflows/test-helm-chart.yaml)
[![Benchmark](https://github.com/unvariance/collector/actions/workflows/benchmark.yaml/badge.svg)](https://github.com/unvariance/collector/actions/workflows/benchmark.yaml)


# Noisy Neighbor Collector

A Kubernetes-native collector for monitoring noisy neighbor issues, currently focusing on memory subsystem interference between pods. This project is under active development and we welcome contributors to help build this critical observability component.



## Overview

The Noisy Neighbor Collector helps SREs identify and quantify performance degradation caused by memory subsystem interference ("noisy neighbors").

This data helps operators:
- **Quantify the performance impact of noisy neighbors**  
- **Determine which pods are causing interference and which are affected**
- **Mitigate interference by isolating high-noise deployments and working with their owners to optimize them**

## Why This Matters

**Memory noisy neighbors directly impact application latency, especially at the tail (P95/P99).** For example, Google [published](https://dl.acm.org/doi/10.1145/2749469.2749475) 5x-14x increases in P95/P99 latency due to memory subsystem interference.

Collecting noisy neighbor metrics and reducing tail latency delivers two key benefits:

1. **Better response times for users**, improving customer experience and key business metrics. Latency has a [direct impact](https://www.gigaspaces.com/blog/amazon-found-every-100ms-of-latency-cost-them-1-in-sales/) on revenue.

2. **More efficient infrastructure through reduced autoscaling.** Many scaling decisions are based on P95/P99 metrics. By reducing spikes caused by noisy neighbors, you can run at higher utilization without breaching latency SLOs.

Common sources of interference include:
- Garbage collection
- Large transactions (e.g. scanning many database records)
- Analytics workloads



## Security and Data Collection

The collector is designed with security in mind and has a limited scope of data collection:

**Only collects CPU profiling data and process metadata:**
- Performance counters: cycles, instructions, LLC cache misses
- Process metadata: process name, process ID (pid), cgroup/container ID

**Does not access process internals or user data:**
- No application-level data is accessed or collected

You can review our [data schema](https://github.com/unvariance/collector/blob/published-benchmarks/benchmarks/parquet-data/schema.txt) and [sample data files](https://github.com/unvariance/collector/blob/published-benchmarks/benchmarks/parquet-data/sample-100.txt) to see exactly what is collected. These resources demonstrate the limited scope of the collected data.



## Requirements

- **Kernel version:** Minimum 5.15
- **Hardware:** Any server with perf counter support (container images are published for x86_64, arm64)
  - for Intel on AWS, an instance that supports "Hardware" from [this list](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-vtune-amplifier-functionality-on-aws-instances.html)
  - for Intel on GCP: instance that supports "Basic Hardware" from [this list](https://www.intel.com/content/www/us/en/developer/articles/technical/vtune-profiler-functionality-on-google-cloud.html)
- **Resource utilization:**
  - Memory: ~300MB per node
  - CPU: ~1% for eBPF, ~0.75% userspace
  - Storage: ~100MB/hour of collected data (varies with node size; there is a quota configuration option to limit the amount of data collected)

See the [benchmark results](https://unvariance.github.io/collector/benchmark) for more details.



## Installation

### Helm Chart

The easiest way to install the collector is using our Helm chart:

```bash
helm repo add unvariance https://unvariance.github.io/collector/charts
helm repo update
helm install collector unvariance/collector \
  --set storage.type="s3" \
  --set storage.prefix="memory-collector-metrics-" \
  --set storage.s3.bucket="your-bucket-name" \
  --set storage.s3.region="us-west-2" \
  --set storage.s3.auth.method="iam" \
  --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"="arn:aws:iam::123456789012:role/S3Access" \
  --set collector.storageQuota="1000000000"
```

This example shows how to configure the collector with S3 storage using IAM roles for authentication, with a 1GB quota.

For complete configuration options, see the [Helm chart documentation](charts/collector/README.md).

### Manual Installation

See the [GitHub Actions workflow](.github/workflows/test-ebpf-collector.yaml) for detailed build steps.



## Roadmap

We're actively working on:

**Container Metadata (PR [#153](https://github.com/unvariance/collector/issues/153)**  
- Capturing container and pod metadata for processes
- Exploring Node Resource Interface (NRI) for metadata access

**Noisy Neighbor Detection**  
- Identifying noisy neighbors from raw 1ms measurements
- Aggregating metrics over longer intervals to determine:
  - Which pods are noisy vs. sensitive 
  - % time each pod is noisy or impacted by noise
- Quantifying cycles wasted due to noise exposure

## Get Involved 

We welcome contributions! Here's how you can help:

- **Code**: Check our [good first issues](../../issues?q=is:open+is:issue+label:"good+first+issue") and [documentation](https://unvariance.github.io/collector/)
- **Use Cases**: Share interference scenarios, test in your environment  
- **Discussion**: Open GitHub issues or reach out - [@Jonathan Perry](https://cloud-native.slack.com/team/U019KBNGKFT) on [CNCF Slack](https://slack.cncf.io/)
- **Schedule a Chat**: https://yonch.com/collector

## Learn More

- KubeCon Europe 2025: The Missing Metrics [(video)](https://www.youtube.com/watch?v=nXdGXdxmWNQ) [(slides)](https://static.sched.com/hosted_files/kccnceu2025/9a/The%20Missing%20Metrics%20-%20Measuring%20Memory%20Interference%20in%20Cloud%20Native%20Systems.pdf)
- KubeCon NA 2024: Love thy (noisy) neighbor [(video)](https://www.youtube.com/watch?v=VsYp_Z1PvOc) [(slides)](https://static.sched.com/hosted_files/kccncna2024/93/Slides_Kubecon%20NA%2724_%20Love%20thy%20%28Noisy%29%20Neighbor.pdf) [(notes)](https://static.sched.com/hosted_files/kccncna2024/50/Transcript_and_Slides__Love_thy_%28Noisy%29_Neighbor.pdf)



## Benchmark Results

The collector was benchmarked using the OpenTelemetry Demo application.

![1 millisecond resolution LLC misses](https://unvariance.github.io/collector/benchmarks/llc_misses.png)

![Slowdown from LLC misses](https://unvariance.github.io/collector/benchmarks/cpi_slowdown_top5_vs_mid.png)

See the [benchmark results](https://unvariance.github.io/collector/benchmark) for more details.


## Background

This project builds on research and technologies from:
- Google's CPI² system 
- Meta's Resource Control
- Alibaba Cloud's Alita
- MIT's Caladan  

## License

Code is licensed under [Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0). Documentation is licensed under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).