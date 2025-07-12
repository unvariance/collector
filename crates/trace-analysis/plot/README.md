# CPI Histogram Analysis

This directory contains plotting scripts for analyzing the output of the trace-analysis tool.

## cpi_histogram.R

Creates probability density plots showing the distribution of cycles per instruction (CPI) for the top 20 processes, categorized by peer hyperthread activity.

### What it does

1. **Reads augmented trace data** - Takes the output parquet file from trace-analysis
2. **Identifies top processes** - Finds the 20 processes with the most total instructions
3. **Calculates CPI** - cycles / instructions for each measurement
4. **Creates weighted histograms** - Uses instruction-proportional weighting (nanoseconds / CPI)
5. **Generates density plots** - Three lines per process showing:
   - **Same Process** (green) - When peer hyperthread runs the same process
   - **Different Process** (red) - When peer hyperthread runs a different process  
   - **Kernel** (blue) - When peer hyperthread runs kernel code

### Requirements

Install required R packages:
```r
install.packages(c("arrow", "dplyr", "ggplot2", "tidyr", "stringr"))
```

### Usage

```bash
Rscript cpi_histogram.R <input_hyperthread_analysis.parquet>
```

Example:
```bash
# After running trace-analysis
cargo run --bin trace-analysis -- -f trace_data.parquet --output-prefix analysis

# Generate plots
Rscript plot/cpi_histogram.R analysis_hyperthread_analysis.parquet
```

### Output

- **PNG file** - `<input_file>_cpi_histogram.png` with the density plots
- **Console output** - Summary statistics and process information

### Interpretation

- **X-axis**: Cycles per instruction (CPI) - higher values indicate more cycles needed per instruction
- **Y-axis**: Density weighted by instructions - shows probability distribution normalized by instruction count
- **Colors**: Different peer hyperthread states
- **Facets**: One subplot per process (top 20 by instruction count)

The plots reveal how hyperthread contention affects CPI distributions. For example:
- If "Different Process" line is shifted right, it suggests hyperthread contention increases CPI
- If "Same Process" and "Different Process" lines are similar, hyperthread sharing may not significantly impact performance
- Kernel activity patterns can show system call overhead effects