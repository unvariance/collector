# Memory Analysis Scripts

This directory contains scripts for analyzing and visualizing memory and CPU metrics collected during experiments.

## Memory Utilization Plotting

The `plot_memory_utilization.R` script generates time-series graphs showing memory utilization of specific processes over the experiment duration.

### Prerequisites

The script requires the following R packages:
- ggplot2
- dplyr
- readr
- tidyr

You can install them with:

```R
install.packages(c("ggplot2", "dplyr", "readr", "tidyr"))
```

### Usage

```bash
Rscript plot_memory_utilization.R <memory_metrics_file> [process_name] [output_file]
```

- `<memory_metrics_file>`: Path to the memory metrics CSV file (pidstat output)
- `[process_name]`: Name of the process to analyze (default: "collector")
- `[output_file]`: Base name for output files (default: "memory_utilization")

### Examples

#### Example 1: Plotting systemd memory usage

```bash
Rscript plot_memory_utilization.R scripts/memory_metrics_sample.csv systemd systemd_memory
```

This command will:
1. Parse the memory metrics from `scripts/memory_metrics_sample.csv`
2. Filter data for the "systemd" process
3. Generate a time-series plot showing memory utilization
4. Save the plot as `systemd_memory.png` and `systemd_memory.pdf`

#### Example 2: Plotting awk memory usage

```bash
Rscript plot_memory_utilization.R scripts/memory_metrics_sample.csv awk awk_memory
```

#### Example 3: Plotting collector process (for real experiment data)

```bash
Rscript plot_memory_utilization.R experiment_data.csv collector collector_memory
```

### Output

The script generates:
- A PNG image of the plot
- A PDF version of the plot
- Summary statistics printed to the console

The plot shows:
- Memory utilization (in MB) on the Y-axis
- Time (in seconds) on the X-axis

For processes with only a single data point, the script will create a point plot with a special subtitle noting the limited data. 

## Converting CPU Metrics Files

Before plotting CPU metrics, you need to convert the raw pidstat output (semicolon-separated) to CSV format. The `convert_cpu_metrics.sh` script handles this conversion.

### Usage

```bash
./convert_cpu_metrics.sh <input_file> <output_file>
```

- `<input_file>`: Path to the raw CPU metrics file from pidstat (semicolon-separated)
- `<output_file>`: Path where the converted CSV will be written

### Example

To convert raw pidstat output to the CSV format required by the plotting script:

```bash
# First collect data with pidstat (example)
pidstat -u -r -l -p ALL -T TASK 1 > raw_cpu_metrics.txt

# Then convert to CSV format
./convert_cpu_metrics.sh raw_cpu_metrics.txt cpu_metrics.csv
```

The converted file can then be used with the plotting script.

## CPU Utilization Plotting

The `plot_cpu_utilization.R` script generates time-series graphs showing CPU utilization of specific processes over the experiment duration.

### Prerequisites

The script requires the following R packages:
- ggplot2
- dplyr
- readr
- tidyr

You can install them with:

```R
install.packages(c("ggplot2", "dplyr", "readr", "tidyr"))
```

### Usage

```bash
Rscript plot_cpu_utilization.R <cpu_metrics_file> [process_name] [output_file]
```

- `<cpu_metrics_file>`: Path to the CPU metrics CSV file (pidstat output)
- `[process_name]`: Name of the process to analyze (default: "collector")
- `[output_file]`: Base name for output files (default: "cpu_utilization")

### Examples

#### Example 1: Plotting collector CPU usage

```bash
Rscript plot_cpu_utilization.R scripts/cpu_metrics_sample.csv collector collector_cpu
```

This command will:
1. Parse the CPU metrics from `scripts/cpu_metrics_sample.csv`
2. Filter data for the "collector" process
3. Generate time-series plots showing CPU utilization
4. Save the plots as `collector_cpu_process.png`, `collector_cpu_other_processes.png`, and `collector_cpu_comparison.png` (and PDF versions)

#### Example 2: Plotting java process CPU usage

```bash
Rscript plot_cpu_utilization.R scripts/cpu_metrics_sample.csv java java_cpu
```

### Output

The script generates three types of visualizations:

1. **Target Process CPU Usage**: 
   - Line plot showing total CPU utilization of the target process
   - CPU utilization in millicores (1/10th of a CPU core)
   - Output: `<output_file>_process.png` and `<output_file>_process.pdf`

2. **Workload CPU Usage**:
   - Line plot showing aggregated CPU utilization of all other processes
   - CPU utilization in millicores
   - Output: `<output_file>_other_processes.png` and `<output_file>_other_processes.pdf`

3. **Comparison Plot with Facets**:
   - Two facets showing the target process and workload CPU utilization
   - Allows for easy comparison of collector overhead against workload CPU usage
   - Each facet uses its own y-axis scale for better visibility of dynamics
   - Output: `<output_file>_comparison.png` and `<output_file>_comparison.pdf`

Additionally, the script prints summary statistics including mean and peak CPU utilization for both the target process and other processes. 

## Memory Usage Plotting

The `plot_memory_usage.R` script generates a stacked area graph showing both Last Level Cache (LLC) misses and cache references per process at millisecond granularity.

### Prerequisites

The script requires the following R packages:
- nanoparquet
- ggplot2
- dplyr
- tidyr

You can install them with:

```R
install.packages(c("nanoparquet", "ggplot2", "dplyr", "tidyr"))
```

### Usage

```bash
Rscript plot_memory_usage.R [parquet_file] [start_time_offset] [window_size] [output_file] [top_n_processes]
```

- `[parquet_file]`: Path to the parquet file containing collector data (default: "collector-parquet.parquet")
- `[start_time_offset]`: Seconds after experiment start to begin analysis (default: 110)
- `[window_size]`: Duration in seconds to analyze (default: 1)
- `[output_file]`: Base name for output files (default: "memory_usage")
- `[top_n_processes]`: Number of top processes to show (default: 15)

### Examples

#### Example 1: Using default settings

```bash
Rscript plot_memory_usage.R collector-parquet.parquet
```

This command will:
1. Parse the memory usage data from `collector-parquet.parquet`
2. Filter for data at 110 seconds after experiment start, with a 1-second window
3. Create both a combined plot and LLC misses plot
4. Save the plots as `memory_usage_combined.png`, `memory_usage.png`, and PDF versions

#### Example 2: Specifying time window and output name

```bash
Rscript plot_memory_usage.R collector-parquet.parquet 120 2 high_load_memory 20
```

This will analyze a 2-second window starting at 120 seconds into the experiment, show the top 20 processes, and save the output as `high_load_memory_combined.png` and `high_load_memory.png` (plus PDF versions).

### Output

The script generates multiple visualizations:

1. **Combined Memory Usage Plot**:
   - Faceted plot with LLC misses on top and cache references on bottom
   - Both metrics normalized to gigabytes per second
   - Same legend and process selection across both facets
   - 16:9 aspect ratio optimized for slide presentations
   - Output: `<output_file>_combined.png` and `<output_file>_combined.pdf`

2. **LLC Misses Plot** (for backward compatibility):
   - Stacked area graph showing LLC misses by process
   - Normalized to gigabytes per second
   - Output: `<output_file>.png` and `<output_file>.pdf`

**Key Features**:
- Process filtering based on total memory usage (LLC misses + cache references)
- Top N processes shown individually, others grouped as "other"
- 16:9 aspect ratio with large fonts suitable for presentations
- Time in milliseconds on the X-axis (within the selected window)
- Both plots use consistent process selection and coloring

### Process Selection Logic

The script uses intelligent process selection based on total memory usage:
- Calculates total memory usage as the sum of LLC misses and cache references for each process
- Selects the top N processes by this combined metric
- Groups remaining processes as "other" for cleaner visualization
- This approach ensures that processes with high cache hit rates (high cache references but low LLC misses) are still prominently displayed

## Workload Performance Visualization

The `plot_workload_performance.R` script generates visualizations from Locust load generator metrics, focusing on workload performance characteristics such as RPS and latency percentiles.

### Prerequisites

The script requires the following R packages:
- ggplot2
- dplyr
- readr
- tidyr
- scales

You can install them with:

```R
install.packages(c("ggplot2", "dplyr", "readr", "tidyr", "scales"))
```

### Usage

```bash
Rscript plot_workload_performance.R <stats_history_file> [output_file]
```

- `<stats_history_file>`: Path to the Locust stats history CSV file
- `[output_file]`: Base name for output files (default: "workload_performance")

### Examples

#### Example 1: Visualizing with default output names

```bash
Rscript plot_workload_performance.R scripts/stats_stats_history.csv
```

This command will:
1. Parse the Locust metrics from `scripts/stats_stats_history.csv`
2. Filter for "Aggregated" data rows
3. Generate three visualizations (see Output section)
4. Save the plots with default base name "workload_performance"

#### Example 2: Specifying a custom output file name

```bash
Rscript plot_workload_performance.R scripts/stats_stats_history.csv experiment1_performance
```

This will save the output files with the base name "experiment1_performance".

### Output

The script generates three visualizations:

1. **Combined RPS and Latency Plot**:
   - Multi-axis graph showing RPS and latency percentiles on the same timeline
   - X-axis: Time elapsed during experiment (seconds)
   - Left Y-axis: Requests per second
   - Right Y-axis: Latency in milliseconds (P50, P95, P99)
   - Output: `<output_file>.png` and `<output_file>.pdf`

2. **Workload Scaling Characteristics**:
   - Scatter plot showing RPS vs concurrent user count
   - Includes smoothed trend line to show scaling properties
   - Output: `<output_file>_scaling.png` and `<output_file>_scaling.pdf`

3. **Response Time Percentiles**:
   - Line graph showing P50, P95, and P99 latencies over time
   - Helps identify latency degradation patterns
   - Output: `<output_file>_latency.png` and `<output_file>_latency.pdf`

Additionally, the script prints summary statistics to the console, including maximum and average values for RPS and latencies. 