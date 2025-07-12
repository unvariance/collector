# CPI Histogram Analysis

This directory contains plotting scripts for analyzing the output of the trace-analysis tool.

## hyperthread_cpi_histogram.R

Creates probability density plots showing the distribution of cycles per instruction (CPI) for the top 20 processes, categorized by peer hyperthread activity.

### What it does

1. **Reads augmented trace data** - Takes the output parquet file from trace-analysis
2. **Identifies top processes** - Finds the 20 processes with the most total instructions
3. **Calculates CPI** - cycles / instructions for each measurement
4. **Computes CPU seconds** - Calculates total CPU seconds for each hyperthread category per process
5. **Limits X-axis display** - All plots limited to CPI ≤ 10 for readability
6. **Creates weighted histograms** - Uses instruction-proportional weighting (nanoseconds / CPI)
7. **Generates density plots** - Three lines per process showing:
   - **Same Process** (green) - When peer hyperthread runs the same process
   - **Different Process** (red) - When peer hyperthread runs a different process  
   - **Kernel** (blue) - When peer hyperthread runs kernel code

### Requirements

Install required R packages:
```r
install.packages(c("nanoparquet", "dplyr", "ggplot2", "tidyr", "stringr"))
```

### Usage

```bash
Rscript hyperthread_cpi_histogram.R <input_hyperthread_analysis.parquet>
```

Example:
```bash
# After running trace-analysis
cargo run --bin trace-analysis -- -f trace_data.parquet --output-prefix analysis

# Generate plots
Rscript hyperthread_cpi_histogram.R analysis_hyperthread_analysis.parquet
```

### Output

- **PNG file** - `<input_file>_cpi_histogram.png` with the density plots
- **Console output** - CPU seconds summary and CPI statistics for statistical significance assessment

### Interpretation

- **X-axis**: Cycles per instruction (CPI) - higher values indicate more cycles needed per instruction
- **Y-axis**: Density weighted by instructions - shows probability distribution normalized by instruction count
- **Colors**: Different peer hyperthread states
- **Facets**: One subplot per process (top 20 by instruction count)
- **Facet titles**: Show CPU seconds for each category to assess statistical significance

#### Statistical Significance Assessment
The CPU seconds shown in each process title help determine if the data is statistically significant:
- **> 1.0 seconds**: Generally sufficient for statistical analysis
- **0.1 - 1.0 seconds**: May have limited statistical power
- **< 0.1 seconds**: Likely insufficient for reliable conclusions

#### X-axis Scaling
All plots are limited to display CPI values from 0 to 10 for readability. Data points with CPI > 10 are not shown but contribute to the density calculation within the visible range.

#### Performance Impact Analysis
The plots reveal how hyperthread contention affects CPI distributions:
- **Right shift in "Different Process"**: Suggests hyperthread contention increases CPI
- **Kernel patterns**: Can reveal system call overhead and interrupt effects
- **Same process patterns**: Shows benefits/costs of process affinity