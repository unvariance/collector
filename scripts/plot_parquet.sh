#!/bin/bash

set -x

# Check if a parquet file was provided
if [ $# -eq 0 ]; then
    echo "Usage: $0 <parquet_file>"
    exit 1
fi

PARQUET_FILE="$1"
if [ ! -f "$PARQUET_FILE" ]; then
    echo "Error: File '$PARQUET_FILE' does not exist"
    exit 1
fi

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Get the base name of the parquet file (without extension)
BASE_NAME=$(basename "$PARQUET_FILE" .parquet)
OUTPUT_DIR="visualization_results_${BASE_NAME}"

# Create output directory
mkdir -p "$OUTPUT_DIR"

# Check if required tools are installed
if ! command -v pqrs &> /dev/null; then
    echo "Error: pqrs is not installed"
    exit 1
fi

if ! command -v R &> /dev/null; then
    echo "Error: R is not installed"
    exit 1
fi

if ! command -v bc &> /dev/null; then
    echo "Error: bc (calculator) is not installed"
    exit 1
fi

# Install required R packages if not already installed
#R -e "if (!require('tidyverse')) install.packages('tidyverse', repos='https://cloud.r-project.org/')" &> /dev/null
R -e "if (!require('scales')) install.packages('scales', repos='https://cloud.r-project.org/')" &> /dev/null
R -e "if (!require('nanoparquet')) install.packages('nanoparquet', repos='https://cloud.r-project.org/')" &> /dev/null
R -e "if (!require('gridExtra')) install.packages('gridExtra', repos='https://cloud.r-project.org/')" &> /dev/null
R -e "if (!require('stringr')) install.packages('stringr', repos='https://cloud.r-project.org/')" &> /dev/null

echo "Generating plots for $PARQUET_FILE"
echo "Output will be saved to $OUTPUT_DIR"

# Determine experiment length using pqrs
echo "Determining experiment length using pqrs..."
FIRST_RECORD=$(pqrs head "$PARQUET_FILE" --records 1 --json 2>/dev/null | head -1)
LAST_RECORD=$(pqrs cat "$PARQUET_FILE" --json 2>/dev/null | tail -1)

if [ -z "$FIRST_RECORD" ] || [ -z "$LAST_RECORD" ]; then
    echo "Error: Could not read parquet file with pqrs"
    exit 1
fi

# Extract timestamps - try different common field names
FIRST_TIMESTAMP=""
LAST_TIMESTAMP=""

# Try common timestamp field names, starting with the known schema field
for field in "start_time" "timestamp" "time" "time_ns" "elapsed_time" "elapsed_ns"; do
    FIRST_TIMESTAMP=$(echo "$FIRST_RECORD" | grep -o "\"$field\":[0-9]*" | cut -d: -f2 | tr -d ',')
    LAST_TIMESTAMP=$(echo "$LAST_RECORD" | grep -o "\"$field\":[0-9]*" | cut -d: -f2 | tr -d ',')
    
    if [ -n "$FIRST_TIMESTAMP" ] && [ -n "$LAST_TIMESTAMP" ]; then
        echo "Using field '$field' for timestamps"
        break
    fi
done

if [ -z "$FIRST_TIMESTAMP" ] || [ -z "$LAST_TIMESTAMP" ]; then
    echo "Warning: Could not extract timestamps from known fields, using fallback approach..."
    # Try to extract any numeric field that looks like a timestamp
    FIRST_TIMESTAMP=$(echo "$FIRST_RECORD" | grep -o '[0-9]\+\.[0-9]\+' | head -1)
    LAST_TIMESTAMP=$(echo "$LAST_RECORD" | grep -o '[0-9]\+\.[0-9]\+' | tail -1)
    
    # If still no decimal numbers, try integers
    if [ -z "$FIRST_TIMESTAMP" ] || [ -z "$LAST_TIMESTAMP" ]; then
        FIRST_TIMESTAMP=$(echo "$FIRST_RECORD" | grep -o '[0-9]\+' | head -1)
        LAST_TIMESTAMP=$(echo "$LAST_RECORD" | grep -o '[0-9]\+' | tail -1)
    fi
fi

if [ -z "$FIRST_TIMESTAMP" ] || [ -z "$LAST_TIMESTAMP" ]; then
    echo "Error: Could not extract timestamps from parquet file"
    exit 1
fi

# Convert from nanoseconds to seconds if the values are very large (likely nanoseconds)
if (( $(echo "$FIRST_TIMESTAMP > 1000000000" | bc -l) )); then
    echo "Detected nanosecond timestamps, converting to seconds..."
    FIRST_TIMESTAMP=$(echo "scale=6; $FIRST_TIMESTAMP / 1000000000" | bc -l)
    LAST_TIMESTAMP=$(echo "scale=6; $LAST_TIMESTAMP / 1000000000" | bc -l)
fi

EXPERIMENT_LENGTH=$(echo "scale=6; $LAST_TIMESTAMP - $FIRST_TIMESTAMP" | bc -l)
echo "Experiment length: $EXPERIMENT_LENGTH seconds (from $FIRST_TIMESTAMP to $LAST_TIMESTAMP)"

# Calculate time windows for beginning, middle, and end
WINDOW_SIZE=5.0  # 10 intervals of 0.5 seconds each
MIDDLE_START=$(echo "$FIRST_TIMESTAMP + ($EXPERIMENT_LENGTH / 2) - ($WINDOW_SIZE / 2)" | bc -l)
END_START=$(echo "$LAST_TIMESTAMP - $WINDOW_SIZE" | bc -l)

# Only proceed if experiment is long enough
MIN_LENGTH=15.0  # Need at least 15 seconds for all three windows
if (( $(echo "$EXPERIMENT_LENGTH < $MIN_LENGTH" | bc -l) )); then
    echo "Warning: Experiment too short ($EXPERIMENT_LENGTH s) for all three windows. Using entire duration."
    # Generate plots for entire duration
    echo "Generating memory usage plots for entire duration..."
    Rscript "$SCRIPT_DIR/plot_memory_usage.R" "$PARQUET_FILE" 0 "$EXPERIMENT_LENGTH" "$OUTPUT_DIR/memory_usage_entire" || true
else
    # Generate 10 memory usage plots for beginning, middle, and end
    echo "Generating 10 memory usage plots for beginning, middle, and end..."
    
    # Beginning: 10 plots with 0.5s windows starting at 0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5
    echo "Generating beginning plots..."
    for i in {0..9}; do
        start_time=$(echo "scale=1; $i * 0.5" | bc -l)
        window_size=0.5
        output_name="memory_usage_beginning_${i}_${start_time}s"
        echo "  Plot $((i+1))/10: ${start_time}s - $(echo "$start_time + $window_size" | bc -l)s"
        Rscript "$SCRIPT_DIR/plot_memory_usage.R" "$PARQUET_FILE" "$start_time" "$window_size" "$OUTPUT_DIR/$output_name" || true
    done
    
    # Middle: 10 plots with 0.5s windows centered around middle
    echo "Generating middle plots..."
    middle_center=$(echo "scale=6; $FIRST_TIMESTAMP + ($EXPERIMENT_LENGTH / 2)" | bc -l)
    middle_start_base=$(echo "scale=6; $middle_center - 2.5" | bc -l)  # Start 2.5s before center
    for i in {0..9}; do
        start_time=$(echo "scale=6; $middle_start_base + ($i * 0.5)" | bc -l)
        window_size=0.5
        relative_start=$(echo "scale=6; $start_time - $FIRST_TIMESTAMP" | bc -l)
        output_name="memory_usage_middle_${i}_${relative_start}s"
        echo "  Plot $((i+1))/10: ${relative_start}s - $(echo "$relative_start + $window_size" | bc -l)s (relative to start)"
        Rscript "$SCRIPT_DIR/plot_memory_usage.R" "$PARQUET_FILE" "$relative_start" "$window_size" "$OUTPUT_DIR/$output_name" || true
    done
    
    # End: 10 plots with 0.5s windows leading up to the end
    echo "Generating end plots..."
    end_start_base=$(echo "scale=6; $EXPERIMENT_LENGTH - 5.0" | bc -l)  # Start 5s before end
    for i in {0..9}; do
        start_time=$(echo "scale=6; $end_start_base + ($i * 0.5)" | bc -l)
        window_size=0.5
        output_name="memory_usage_end_${i}_${start_time}s"
        echo "  Plot $((i+1))/10: ${start_time}s - $(echo "$start_time + $window_size" | bc -l)s (relative to start)"
        Rscript "$SCRIPT_DIR/plot_memory_usage.R" "$PARQUET_FILE" "$start_time" "$window_size" "$OUTPUT_DIR/$output_name" || true
    done
fi

# Generate CPI by LLC misses plots for entire run
echo "Generating CPI by LLC misses plots for entire run..."
Rscript "$SCRIPT_DIR/plot_cpi_by_llc_misses.R" "$PARQUET_FILE" 100000 "$OUTPUT_DIR/cpi_by_llc_misses_full" 23 "$FIRST_TIMESTAMP" "$LAST_TIMESTAMP" || true

echo "Plot generation complete. Results saved to $OUTPUT_DIR" 