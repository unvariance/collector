#!/usr/bin/env Rscript

# Setup - load required libraries
if (!requireNamespace("nanoparquet", quietly = TRUE)) {
  install.packages("nanoparquet", repos = "https://cloud.r-project.org/")
}
if (!requireNamespace("ggplot2", quietly = TRUE)) {
  install.packages("ggplot2", repos = "https://cloud.r-project.org/")
}
if (!requireNamespace("dplyr", quietly = TRUE)) {
  install.packages("dplyr", repos = "https://cloud.r-project.org/")
}
if (!requireNamespace("viridis", quietly = TRUE)) {
  install.packages("viridis", repos = "https://cloud.r-project.org/")
}

library(nanoparquet)
library(ggplot2)
library(dplyr)
library(viridis)

# Parse command line arguments
args <- commandArgs(trailingOnly = TRUE)
input_file <- if(length(args) >= 1) args[1] else "collector-parquet.parquet"
window_duration <- if(length(args) >= 2) as.numeric(args[2]) else 20  # Default to 20 seconds
output_file <- if(length(args) >= 3) args[3] else "contention_analysis"
top_n_processes <- if(length(args) >= 4) as.numeric(args[4]) else 12  # Default to showing top 12 processes
sample_rate <- if(length(args) >= 5) as.numeric(args[5]) else 0.2  # Default to 20% sampling
end_time <- if(length(args) >= 6) as.numeric(args[6]) else NULL  # Optional end time in seconds

# Constants
NS_PER_SEC <- 1e9

# Function to load and process parquet data
load_and_process_parquet <- function(file_path, window_duration_sec, end_time_sec = NULL) {
  # Read the parquet file
  message("Reading parquet file: ", file_path)
  perf_data <- nanoparquet::read_parquet(file_path)
  
  # Convert start_time to relative time in nanoseconds
  min_time <- min(perf_data$start_time, na.rm = TRUE)
  max_time <- max(perf_data$start_time, na.rm = TRUE)
  
  # Calculate window boundaries
  if (!is.null(end_time_sec)) {
    # Use specified end time (in seconds from start of experiment)
    window_end_ns <- min_time + end_time_sec * NS_PER_SEC
    window_start_ns <- window_end_ns - window_duration_sec * NS_PER_SEC
    
    # Ensure we don't go beyond the actual data range
    window_end_ns <- min(window_end_ns, max_time)
    window_start_ns <- max(window_start_ns, min_time)
    
    message("Using specified end time: ", end_time_sec, " seconds from start")
    message("Window: ", (window_start_ns - min_time) / NS_PER_SEC, " to ", (window_end_ns - min_time) / NS_PER_SEC, " seconds (relative)")
  } else {
    # Use last N seconds (original behavior)
    window_start_ns <- max_time - window_duration_sec * NS_PER_SEC
    window_end_ns <- max_time
    
    message("Using data from last ", window_duration_sec, " seconds of experiment")
    message("Window: ", (window_start_ns - min_time) / NS_PER_SEC, " to ", (window_end_ns - min_time) / NS_PER_SEC, " seconds (relative)")
  }
  
  # Filter data within the window
  window_data <- perf_data %>%
    filter(start_time >= window_start_ns & start_time <= window_end_ns) %>%
    mutate(
      # Replace NULL process names with "kernel"
      process_name = ifelse(is.na(process_name), "kernel", process_name),
      # Calculate CPI, filtering out invalid values
      cpi = ifelse(instructions > 0, cycles / instructions, NA)
    ) %>%
    filter(
      !is.na(cpi),
      cpi > 0,
      cpi < 15,  # Filter out extreme CPI values
      instructions > 0,
      instructions < 1e9  # Filter out extreme instruction counts
    )
  
  return(window_data)
}

# Function to prepare contention analysis data
prepare_contention_data <- function(data, n_top_processes = top_n_processes, sample_rate = 0.2, instruction_min, instruction_max) {
  
  # FIRST: Calculate total activity from ALL processes for each time point (before any filtering)
  message("Computing total activity per time slice from all processes...")
  time_totals <- data %>%
    group_by(start_time) %>%
    summarise(
      total_instructions_all = sum(instructions, na.rm = TRUE),
      total_cache_misses_all = sum(llc_misses, na.rm = TRUE),
      processes_active = n(),
      .groups = 'drop'
    )
  
  message("Time slice activity summary:")
  message("  Total time slices: ", nrow(time_totals))
  message("  Avg processes per slice: ", round(mean(time_totals$processes_active), 1))
  message("  Avg total instructions per slice: ", format(mean(time_totals$total_instructions_all), scientific = TRUE, digits = 3))
  message("  Avg total cache misses per slice: ", format(mean(time_totals$total_cache_misses_all), scientific = TRUE, digits = 3))
  
  # SECOND: Select top processes by total instruction count
  top_processes <- data %>%
    group_by(process_name) %>%
    summarise(
      total_instructions = sum(instructions, na.rm = TRUE),
      total_cycles = sum(cycles, na.rm = TRUE),
      sample_count = n()
    ) %>%
    arrange(desc(total_instructions)) %>%
    slice_head(n = n_top_processes)
  
  message("Top ", n_top_processes, " processes by total instruction count:")
  for (i in 1:nrow(top_processes)) {
    process <- top_processes$process_name[i]
    instructions <- top_processes$total_instructions[i]
    cycles <- top_processes$total_cycles[i]
    samples <- top_processes$sample_count[i]
    avg_cpi <- cycles / instructions
    message("  ", i, ". ", process, ": ", 
            format(instructions, scientific = TRUE, digits = 3), " instructions, ",
            "avg CPI = ", round(avg_cpi, 3), " (", samples, " samples)")
  }
  
  # THIRD: Filter data for top processes and add "other" activity from ALL processes
  plot_data <- data %>%
    filter(process_name %in% top_processes$process_name) %>%
    # Join with time totals to get complete "other" activity
    left_join(time_totals, by = "start_time") %>%
    mutate(
      # Other activity = total from ALL processes minus this process
      other_instructions = total_instructions_all - instructions,
      other_cache_misses = total_cache_misses_all - llc_misses
    ) %>%
    filter(other_instructions > 0)  # Only keep time points where other processes were active
  
  # FOURTH: Filter to specific instruction range for clearer analysis
  message("Filtering to instruction range: ", instruction_min, " to ", instruction_max)
  
  filtered_data <- plot_data %>%
    filter(
      instructions >= instruction_min,
      instructions <= instruction_max
    )
  
  message("Instruction range filtering results:")
  message("  Original data points: ", nrow(plot_data))
  message("  After instruction filtering: ", nrow(filtered_data))
  
  if (nrow(filtered_data) < 100) {
    stop("Insufficient data after instruction filtering. Found ", nrow(filtered_data), " points.")
  }
  
  # Report instruction range coverage by process
  range_summary <- filtered_data %>%
    group_by(process_name) %>%
    summarise(
      points_in_range = n(),
      min_instructions = min(instructions),
      max_instructions = max(instructions),
      median_cpi = median(cpi, na.rm = TRUE),
      .groups = 'drop'
    ) %>%
    arrange(desc(points_in_range))
  
  message("Points in instruction range by process:")
  for (i in 1:nrow(range_summary)) {
    process <- range_summary$process_name[i]
    points <- range_summary$points_in_range[i]
    min_inst <- range_summary$min_instructions[i]
    max_inst <- range_summary$max_instructions[i]
    median_cpi <- range_summary$median_cpi[i]
    message("  ", process, ": ", points, " points, instructions ", min_inst, "-", max_inst, 
            ", median CPI = ", round(median_cpi, 3))
  }
  
  # Sample the data for visualization
  sampled_data <- filtered_data %>%
    group_by(process_name) %>%
    sample_frac(sample_rate) %>%
    ungroup()
  
  message("Contention analysis data prepared:")
  message("  Filtered data points: ", nrow(filtered_data))
  message("  Sampled points (", sample_rate*100, "%): ", nrow(sampled_data))
  
  return(sampled_data)
}

# Function to create contention plots
create_contention_plots <- function(contention_data, window_duration_sec, output_file, instruction_min, instruction_max) {
  
  # Generate colors for processes
  unique_processes <- unique(contention_data$process_name)
  n_colors <- length(unique_processes)
  process_colors <- rainbow(n_colors, start = 0, end = 0.8)  # Avoid red-pink range
  names(process_colors) <- unique_processes
  
  # Format instruction range for display
  format_instruction_count <- function(x) {
    if (x >= 1000000) {
      paste0(round(x / 1000000, 1), "M")
    } else if (x >= 1000) {
      paste0(round(x / 1000, 0), "k")
    } else {
      as.character(x)
    }
  }
  
  instruction_range_label <- paste0(format_instruction_count(instruction_min), "-", format_instruction_count(instruction_max))
  
  # Create instructions vs CPI plot
  instructions_plot <- ggplot(contention_data, aes(x = other_instructions, y = cpi, color = process_name)) +
    geom_point(alpha = 0.6, size = 1.2) +
    scale_x_continuous(labels = function(x) format(x, scientific = FALSE, big.mark = ",")) +
    scale_color_manual(values = process_colors) +
    facet_wrap(~ process_name, scales = "free", ncol = 3) +
    labs(
      title = paste0("Instructions vs CPI in Range ", instruction_range_label, ": Last ", window_duration_sec, " Seconds"),
      subtitle = paste0("Filtered to instruction range for clearer analysis (", nrow(contention_data), " sampled points)"),
      x = "Instructions",
      y = "Cycles Per Instruction (CPI)",
      color = "Process"
    ) +
    theme_minimal() +
    theme(
      panel.grid.minor = element_blank(),
      plot.title = element_text(face = "bold", size = 14),
      plot.subtitle = element_text(size = 11),
      axis.title = element_text(face = "bold", size = 11),
      axis.text = element_text(size = 8),
      axis.text.x = element_text(angle = 45, hjust = 1),
      strip.text = element_text(face = "bold", size = 9),
      panel.spacing = unit(0.5, "lines"),
      legend.position = "none"  # Remove legend since we have facets
    )
  
  # Create other cache misses vs CPI plot
  cache_plot <- ggplot(contention_data, aes(x = other_cache_misses, y = cpi, color = process_name)) +
    geom_point(alpha = 0.6, size = 1.2) +
    scale_x_log10(labels = function(x) format(x, scientific = TRUE, digits = 2)) +
    scale_color_manual(values = process_colors) +
    facet_wrap(~ process_name, scales = "free", ncol = 3) +
    labs(
      title = paste0("Other Processes' Cache Misses vs CPI: Last ", window_duration_sec, " Seconds"),
      subtitle = paste0("Process instructions filtered to ", instruction_range_label, " range (", nrow(contention_data), " sampled points)"),
      x = "Other Processes' Total Cache Misses (log scale)",
      y = "Cycles Per Instruction (CPI)",
      color = "Process"
    ) +
    theme_minimal() +
    theme(
      panel.grid.minor = element_blank(),
      plot.title = element_text(face = "bold", size = 14),
      plot.subtitle = element_text(size = 11),
      axis.title = element_text(face = "bold", size = 11),
      axis.text = element_text(size = 8),
      axis.text.x = element_text(angle = 45, hjust = 1),
      strip.text = element_text(face = "bold", size = 9),
      panel.spacing = unit(0.5, "lines"),
      legend.position = "none"  # Remove legend since we have facets
    )
  
  # Create other instructions vs CPI plot
  other_instructions_plot <- ggplot(contention_data, aes(x = other_instructions, y = cpi, color = process_name)) +
    geom_point(alpha = 0.6, size = 1.2) +
    scale_x_log10(labels = function(x) format(x, scientific = TRUE, digits = 2)) +
    scale_color_manual(values = process_colors) +
    facet_wrap(~ process_name, scales = "free", ncol = 3) +
    labs(
      title = paste0("Other Processes' Instructions vs CPI: Last ", window_duration_sec, " Seconds"),
      subtitle = paste0("Process instructions filtered to ", instruction_range_label, " range (", nrow(contention_data), " sampled points)"),
      x = "Other Processes' Total Instructions (log scale)",
      y = "Cycles Per Instruction (CPI)",
      color = "Process"
    ) +
    theme_minimal() +
    theme(
      panel.grid.minor = element_blank(),
      plot.title = element_text(face = "bold", size = 14),
      plot.subtitle = element_text(size = 11),
      axis.title = element_text(face = "bold", size = 11),
      axis.text = element_text(size = 8),
      axis.text.x = element_text(angle = 45, hjust = 1),
      strip.text = element_text(face = "bold", size = 9),
      panel.spacing = unit(0.5, "lines"),
      legend.position = "none"  # Remove legend since we have facets
    )
  
  # Save plots
  instructions_pdf <- paste0(output_file, "_instructions_vs_cpi.pdf")
  cache_pdf <- paste0(output_file, "_cache_misses_vs_cpi.pdf")
  other_instructions_pdf <- paste0(output_file, "_other_instructions_vs_cpi.pdf")
  
  message("Saving instructions vs CPI plot as PDF: ", instructions_pdf)
  ggsave(instructions_pdf, instructions_plot, width = 16, height = 12)
  
  message("Saving cache misses vs CPI plot as PDF: ", cache_pdf)
  ggsave(cache_pdf, cache_plot, width = 16, height = 12)
  
  message("Saving other instructions vs CPI plot as PDF: ", other_instructions_pdf)
  ggsave(other_instructions_pdf, other_instructions_plot, width = 16, height = 12)
  
  return(list(
    instructions_plot = instructions_plot, 
    cache_plot = cache_plot,
    other_instructions_plot = other_instructions_plot
  ))
}

# Main execution
main <- function() {
  tryCatch({
    # Check if input file exists
    if (!file.exists(input_file)) {
      stop("Input file does not exist: ", input_file)
    }
    
    message("Processing contention analysis...")
    window_data <- load_and_process_parquet(input_file, window_duration, end_time)
    
    # Check if we have enough data
    if (nrow(window_data) < 1000) {
      stop("Not enough data points in the selected time window. Found ", nrow(window_data), " points.")
    }
    
    message("Preparing contention analysis data...")
    instruction_min <- 300000
    instruction_max <- 400000
    contention_data <- prepare_contention_data(window_data, top_n_processes, sample_rate, instruction_min, instruction_max)
    
    if (nrow(contention_data) < 50) {
      stop("Insufficient data after filtering and sampling. Found ", nrow(contention_data), " points.")
    }
    
    message("Creating contention analysis plots...")
    plots <- create_contention_plots(contention_data, window_duration, output_file, instruction_min, instruction_max)
    
    message("Contention analysis complete!")
  }, error = function(e) {
    message("Error: ", e$message)
    quit(status = 1)
  })
}

# Execute main function
main() 