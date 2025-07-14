# Concurrency Analysis

## Overview
The concurrency analysis exports CSV files during the finalization phase, enabling plotting of large files.

## Running the Analysis

```bash
# Run concurrency analysis on a trace file
cargo run --bin trace-analysis -- -f trace_data.parquet --analysis-type concurrency

# With custom output prefix
cargo run --bin trace-analysis -- -f trace_data.parquet --analysis-type concurrency --output-prefix my_analysis
```

## Output Files Generated

1. **Main Parquet Output**: `trace_data_concurrency_analysis.parquet`
   - Contains original data plus two new columns:
     - `avg_total_threads`: Average total concurrent threads
     - `avg_same_process_threads`: Average same-process concurrent threads

2. **CSV Statistics Files**:
   - `trace_data_total_concurrency.csv`: Binned statistics for total concurrency
   - `trace_data_same_process_concurrency.csv`: Binned statistics for same-process concurrency

## CSV File Format

Each CSV file contains:
- `process_name`: Process name (aggregated across all PIDs)
- `concurrency_min`: Lower bound of concurrency bin
- `concurrency_max`: Upper bound of concurrency bin
- `cpi_min`: Lower bound of CPI bin
- `cpi_max`: Upper bound of CPI bin
- `instructions`: Total instructions in this bin

## Visualization

Generate plots from the CSV files:

```bash
# Plot total concurrency analysis
Rscript crates/trace-analysis/plot/concurrency_cpi_analysis.R trace_data_total_concurrency.csv

# Plot same-process concurrency analysis
Rscript crates/trace-analysis/plot/concurrency_cpi_analysis.R trace_data_same_process_concurrency.csv
```

## Memory Efficiency

- **Binned Statistics**: Only stores non-zero bins in HashMaps
- **Configurable Bins**: Total stats with 4.0 concurrency, 0.2 CPI bins; Same process with 0.2 concurrency and CPI bins
- **Max CPI Folding**: CPI values > 4.0 are folded into the max CPI bin
- **Streaming Processing**: Processes data in batches, not all at once

## Key Features

1. **Automatic CSV Export**: No manual intervention needed with progress bars for export
2. **Process-level Aggregation**: Statistics grouped by process name (not PID) for smaller files
3. **Top 20 Processes**: R script automatically filters to top 20 by instruction count
4. **Dual Visualization**: Heat map (CPI distribution) + bar chart (instruction counts)
5. **Normalized Heat Map**: Each concurrency bin sums to 1 for comparison