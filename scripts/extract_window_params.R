#!/usr/bin/env Rscript

# Setup - load required libraries
if (!requireNamespace("nanoparquet", quietly = TRUE)) {
  install.packages("nanoparquet", repos = "https://cloud.r-project.org/")
}

library(nanoparquet)

# Parse command line arguments
args <- commandArgs(trailingOnly = TRUE)
if (length(args) < 1) {
  cat("Usage: extract_window_params.R <parquet_file>\n", file = stderr())
  quit(status = 1)
}

input_file <- args[1]

# Check if input file exists
if (!file.exists(input_file)) {
  cat("Error: File does not exist:", input_file, "\n", file = stderr())
  quit(status = 1)
}

tryCatch({
  # Read only start_time column for efficiency
  message("Reading parquet file timestamps: ", input_file, file = stderr())
  
  # Read the parquet file - only the start_time column for speed
  perf_data <- nanoparquet::read_parquet(input_file, col_select = c("start_time"))
  
  # Get min and max timestamps
  min_timestamp_ns <- min(perf_data$start_time, na.rm = TRUE)
  max_timestamp_ns <- max(perf_data$start_time, na.rm = TRUE)
  
  # Convert from nanoseconds to seconds
  min_timestamp_s <- min_timestamp_ns / 1e9
  max_timestamp_s <- max_timestamp_ns / 1e9
  
  # Calculate experiment length
  experiment_length_s <- max_timestamp_s - min_timestamp_s
  
  # Output in format that shell script can easily parse
  # Format: FIRST_TIMESTAMP|LAST_TIMESTAMP|EXPERIMENT_LENGTH
  cat(sprintf("%.6f|%.6f|%.6f\n", min_timestamp_s, max_timestamp_s, experiment_length_s))
  
}, error = function(e) {
  cat("Error reading parquet file:", e$message, "\n", file = stderr())
  quit(status = 1)
}) 