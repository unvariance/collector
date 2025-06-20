#!/usr/bin/env Rscript

# Setup - load required libraries
if (!requireNamespace("nanoparquet", quietly = FALSE)) {
  install.packages("nanoparquet", repos = "https://cloud.r-project.org/")
}
if (!requireNamespace("ggplot2", quietly = TRUE)) {
  install.packages("ggplot2", repos = "https://cloud.r-project.org/")
}
if (!requireNamespace("dplyr", quietly = TRUE)) {
  install.packages("dplyr", repos = "https://cloud.r-project.org/")
}
if (!requireNamespace("tidyr", quietly = TRUE)) {
  install.packages("tidyr", repos = "https://cloud.r-project.org/")
}
if (!requireNamespace("forcats", quietly = TRUE)) {
  install.packages("forcats", repos = "https://cloud.r-project.org/")
}

library(nanoparquet)
library(ggplot2)
library(dplyr)
library(tidyr)
library(forcats)

# Script Description:
# This script analyzes performance data from a parquet file and creates CPI distribution
# plots comparing high vs. low LLC miss periods for different processes.
#
# Command line arguments:
# 1. input_file: Path to the input parquet file (default: collector-parquet.parquet)
# 2. instruction_threshold: Minimum number of instructions for a sample to be considered (default: 100000)
# 3. output_file: Base name for output files without extension (default: cpi_by_llc_misses)
# 4. top_n_processes: Number of top processes to show individually (default: 23)
# 5. start_time_seconds: Start time in seconds for the analysis window (default: 205)
# 6. end_time_seconds: End time in seconds for the analysis window (default: 255)
#
# Example usage:
# Rscript plot_cpi_by_llc_misses.R my-data.parquet 50000 my-output 15 200 300
# This would use my-data.parquet, 50k instruction threshold, output to my-output.{png,pdf},
# show top 15 processes, and analyze data from 200-300 seconds.

# Parse command line arguments
args <- commandArgs(trailingOnly = TRUE)
input_file <- if(length(args) >= 1) args[1] else "collector-parquet.parquet"
instruction_threshold <- if(length(args) >= 2) as.numeric(args[2]) else 100000  # Default to 100k instructions
output_file <- if(length(args) >= 3) args[3] else "cpi_by_llc_misses"
top_n_processes <- if(length(args) >= 4) as.numeric(args[4]) else 23  # Default to showing top 23 processes + "other"
start_time_seconds <- if(length(args) >= 5) as.numeric(args[5]) else 205  # Default steady state starts at 205 seconds
end_time_seconds <- if(length(args) >= 6) as.numeric(args[6]) else 255  # Default steady state ends at 255 seconds

# Function to load and process parquet data
load_and_process_parquet <- function(file_path) {
  # Read the parquet file
  message("Reading parquet file: ", file_path)
  perf_data <- nanoparquet::read_parquet(file_path)
  
  # Replace NULL process names with "kernel" for better visualization
  perf_data$process_name[is.na(perf_data$process_name)] <- "kernel"
  
  # Extract millisecond time slots from start_time (nanoseconds)
  # We're assuming the timestamps are already aligned to millisecond boundaries
  perf_data$ms_slot <- floor(perf_data$start_time / 1e6)
  
  # Calculate cycles per instruction (CPI) for each sample
  perf_data$cpi <- perf_data$cycles / pmax(perf_data$instructions, 1)  # Avoid division by zero
  
  # Convert start_time from nanoseconds to seconds for time window filtering
  perf_data$time_seconds <- perf_data$start_time / 1e9
  
  return(perf_data)
}

# Function to analyze LLC misses and create the faceted histogram
create_cpi_llc_analysis <- function(data, instruction_threshold, top_n_processes,
                                   low_start_percentile, low_end_percentile,
                                   high_start_percentile, high_end_percentile,
                                   start_time_seconds, end_time_seconds) {
  message(paste0("Analyzing LLC misses and CPI comparing ",
                high_start_percentile, "-", high_end_percentile, "% vs ",
                low_start_percentile, "-", low_end_percentile, "%..."))
  
  # Calculate aggregate LLC misses for each millisecond time slot
  ms_aggregates <- data %>%
    group_by(ms_slot) %>%
    summarise(
      total_llc_misses = sum(llc_misses, na.rm = TRUE),
      .groups = "drop"
    )
  
  # Determine the LLC misses thresholds for the given percentiles
  low_start_threshold <- quantile(ms_aggregates$total_llc_misses, low_start_percentile/100, na.rm = TRUE)
  low_end_threshold <- quantile(ms_aggregates$total_llc_misses, low_end_percentile/100, na.rm = TRUE)
  high_start_threshold <- quantile(ms_aggregates$total_llc_misses, high_start_percentile/100, na.rm = TRUE)
  high_end_threshold <- if(high_end_percentile == 100) Inf else quantile(ms_aggregates$total_llc_misses, high_end_percentile/100, na.rm = TRUE)
  
  message("Low LLC miss threshold (", low_start_percentile, "-", low_end_percentile, "%): ", 
          low_start_threshold, " to ", low_end_threshold)
  message("High LLC miss threshold (", high_start_percentile, "-", high_end_percentile, "%): ", 
          high_start_threshold, " to ", high_end_threshold)

  # Create category labels for the legend
  high_label <- paste0("High LLC Misses (", high_start_percentile, "-", high_end_percentile, "%)")
  low_label <- paste0("Low LLC Misses (", low_start_percentile, "-", low_end_percentile, "%)")

  # Classify time slots as high or low LLC miss periods
  ms_aggregates <- ms_aggregates %>%
    mutate(llc_category = case_when(
      total_llc_misses >= low_start_threshold & total_llc_misses <= low_end_threshold ~ low_label,
      total_llc_misses > high_start_threshold & total_llc_misses <= high_end_threshold ~ high_label,
      TRUE ~ "Other"
    ))
  
  # Join the LLC category back to the original data
  data_with_llc <- data %>%
    left_join(ms_aggregates, by = "ms_slot")
  
  # Identify top processes by total instructions
  top_processes <- data %>%
    group_by(process_name) %>%
    summarise(total_instructions = sum(instructions, na.rm = TRUE)) %>%
    arrange(desc(total_instructions)) %>%
    slice_head(n = top_n_processes) %>%
    pull(process_name)
  
  message("Top ", length(top_processes), " processes by instructions:")
  print(head(top_processes, 10))
  
  # Filter for significant samples (>instruction_threshold) and prepare for plotting
  plot_data <- data_with_llc %>%
    # Group all non-top processes as "other"
    mutate(process_group = ifelse(process_name %in% top_processes, 
                                 as.character(process_name), 
                                 "other")) %>%
    # Filter for samples with significant instruction counts
    filter(instructions > instruction_threshold) %>%
    # Filter out the "Other" category
    filter(llc_category != "Other") %>%
    # Cap extreme CPI values for better visualization
    mutate(cpi_capped = pmin(cpi, 10))  # Cap at 10 cycles per instruction
  
  # Count samples per process for reporting
  process_counts <- plot_data %>%
    group_by(process_group, llc_category) %>%
    summarise(
      sample_count = n(),
      total_cycles = sum(cycles, na.rm = TRUE),
      total_instructions = sum(instructions, na.rm = TRUE),
      .groups = "drop"
    ) %>%
    mutate(avg_cpi = total_cycles / pmax(total_instructions, 1))
  
  message("Sample counts and average CPI by process and LLC category:")
  print(process_counts)
  
  # Ensure we have enough data to plot
  if(nrow(plot_data) < 10) {
    stop("Not enough data points after filtering. Try lowering the instruction threshold.")
  }
  
  # Reorder factor levels based on total instructions for better visualization
  process_order <- c(top_processes, "other")
  plot_data$process_group <- factor(plot_data$process_group, levels = process_order)
  
  
  # Create the faceted histogram plot
  p <- ggplot(plot_data, aes(x = cpi_capped, fill = llc_category)) +
    geom_density(alpha = 0.7, adjust = 1.5) +
    facet_wrap(~ process_group, scales = "free_y", ncol = 3) +
    scale_fill_manual(values = c("#E41A1C", "#377EB8")) +
    labs(
      title = "Cycles Per Instruction (CPI) Distribution by Process",
      subtitle = paste0("Comparing LLC miss percentiles: ", 
                       high_start_percentile, "-", high_end_percentile, "% vs ", 
                       low_start_percentile, "-", low_end_percentile, "% ",
                       "(", start_time_seconds, "-", end_time_seconds, "s window, ",
                       instruction_threshold, " instructions)"),
      x = "Cycles Per Instruction (CPI)",
      y = "Density",
      fill = "LLC Miss Category"
    ) +
    theme_minimal() +
    theme(
      legend.position = "top",
      strip.background = element_rect(fill = "lightgray"),
      strip.text = element_text(face = "bold"),
      plot.title = element_text(face = "bold"),
      axis.title = element_text(face = "bold")
    )
  
  return(p)
}

# Function to create CPI slowdown bar plots comparing high LLC percentiles to low ones
create_cpi_slowdown_plot <- function(data, instruction_threshold, top_n_processes,
                                    low_start_percentile, low_end_percentile,
                                    high_start_percentile, high_end_percentile,
                                    start_time_seconds, end_time_seconds) {
  message(paste0("Creating CPI slowdown plot comparing LLC misses ",
                 high_start_percentile, "-", high_end_percentile, "% vs ",
                 low_start_percentile, "-", low_end_percentile, "%..."))
  
  # Calculate aggregate LLC misses for each millisecond time slot
  ms_aggregates <- data %>%
    group_by(ms_slot) %>%
    summarise(
      total_llc_misses = sum(llc_misses, na.rm = TRUE),
      .groups = "drop"
    )
  
  # Determine the LLC misses thresholds for the given percentiles
  low_start_threshold <- quantile(ms_aggregates$total_llc_misses, low_start_percentile/100, na.rm = TRUE)
  low_end_threshold <- quantile(ms_aggregates$total_llc_misses, low_end_percentile/100, na.rm = TRUE)
  high_start_threshold <- quantile(ms_aggregates$total_llc_misses, high_start_percentile/100, na.rm = TRUE)
  high_end_threshold <- if(high_end_percentile == 100) Inf else quantile(ms_aggregates$total_llc_misses, high_end_percentile/100, na.rm = TRUE)
  
  message("Low LLC miss threshold (", low_start_percentile, "-", low_end_percentile, "%): ", 
          low_start_threshold, " to ", low_end_threshold)
  message("High LLC miss threshold (", high_start_percentile, "-", high_end_percentile, "%): ", 
          high_start_threshold, " to ", high_end_threshold)
  
  # Classify time slots into low/high LLC miss categories
  ms_aggregates <- ms_aggregates %>%
    mutate(llc_category = case_when(
      total_llc_misses >= low_start_threshold & total_llc_misses <= low_end_threshold ~ 
        paste0(low_start_percentile, "-", low_end_percentile, "%"),
      total_llc_misses > high_start_threshold & total_llc_misses <= high_end_threshold ~ 
        paste0(high_start_percentile, "-", high_end_percentile, "%"),
      TRUE ~ "Other"
    ))
  
  # Join the LLC category back to the original data
  data_with_llc <- data %>%
    left_join(ms_aggregates, by = "ms_slot")
  
  # Identify top processes by total instructions
  top_processes <- data %>%
    group_by(process_name) %>%
    summarise(total_instructions = sum(instructions, na.rm = TRUE)) %>%
    arrange(desc(total_instructions)) %>%
    slice_head(n = top_n_processes) %>%
    pull(process_name)
  
  # Filter for significant samples and prepare for analysis
  plot_data <- data_with_llc %>%
    # Group all non-top processes as "other"
    mutate(process_group = ifelse(process_name %in% top_processes, 
                                 as.character(process_name), 
                                 "other")) %>%
    # Filter for samples with significant instruction counts
    filter(instructions > instruction_threshold) %>%
    # Keep only low and high LLC categories
    filter(llc_category != "Other")
  
  # Calculate aggregate cycles and instructions for each process and LLC category,
  # then compute CPI from these aggregates
  cpi_by_category <- plot_data %>%
    group_by(process_group, llc_category) %>%
    summarise(
      total_cycles = sum(cycles, na.rm = TRUE),
      total_instructions = sum(instructions, na.rm = TRUE),
      sample_count = n(),
      .groups = "drop"
    ) %>%
    mutate(aggregate_cpi = total_cycles / pmax(total_instructions, 1))  # Avoid division by zero
  
  # Ensure we have data for both categories for each process
  process_categories <- cpi_by_category %>%
    group_by(process_group) %>%
    summarise(category_count = n_distinct(llc_category), .groups = "drop") %>%
    filter(category_count == 2) %>%
    pull(process_group)
  
  if(length(process_categories) == 0) {
    stop("No processes have data for both LLC miss categories. Consider adjusting thresholds or using more data.")
  }
  
  # Filter to include only processes with both categories
  cpi_by_category <- cpi_by_category %>%
    filter(process_group %in% process_categories)
  
  # Reshape the data to calculate slowdown ratios
  cpi_wide <- cpi_by_category %>%
    select(process_group, llc_category, aggregate_cpi) %>%
    pivot_wider(
      id_cols = process_group,
      names_from = llc_category,
      values_from = aggregate_cpi
    )
  
  low_col <- paste0(low_start_percentile, "-", low_end_percentile, "%")
  high_col <- paste0(high_start_percentile, "-", high_end_percentile, "%")
  
  # Calculate slowdown ratio
  cpi_wide$slowdown_ratio <- cpi_wide[[high_col]] / cpi_wide[[low_col]]
  
  # Prepare data for plotting
  plot_data_ratio <- cpi_wide %>%
    select(process_group, slowdown_ratio) %>%
    # Sort by slowdown ratio
    arrange(desc(slowdown_ratio))
  
  # Reorder factor levels based on slowdown ratio for better visualization
  process_order <- plot_data_ratio$process_group
  plot_data_ratio$process_group <- factor(plot_data_ratio$process_group, 
                                         levels = process_order)
  
  # Create the bar plot
  p <- ggplot(plot_data_ratio, aes(x = process_group, y = slowdown_ratio, fill = slowdown_ratio)) +
    geom_bar(stat = "identity") +
    geom_text(aes(label = sprintf("%.2fx", slowdown_ratio)), 
              vjust = -0.5, size = 6) +
    scale_fill_gradient(low = "#377EB8", high = "#E41A1C") +
    labs(
      title = paste0("CPI Slowdown: ", high_start_percentile, "-", high_end_percentile, 
                   "% vs ", low_start_percentile, "-", low_end_percentile, "% LLC Misses"),
      subtitle = paste0("Analysis of ", start_time_seconds, "-", end_time_seconds, 
                       "s window, ", instruction_threshold, " instruction threshold"),
      x = "Process",
      y = "CPI Slowdown Factor",
      fill = "Slowdown"
    ) +
    theme_minimal() +
    theme(
      legend.position = "none",
      plot.title = element_text(face = "bold", size = 24),
      plot.subtitle = element_text(size = 18),
      axis.title = element_text(face = "bold", size = 18),
      axis.text.x = element_text(angle = 60, hjust = 1, size = 16),
      axis.text.y = element_text(size = 14)
    )
  
  # Print summary statistics
  message("CPI slowdown statistics:")
  summary_stats <- summary(plot_data_ratio$slowdown_ratio)
  print(summary_stats)
  
  # Print top slowdowns
  message("Top 5 processes by CPI slowdown:")
  print(head(plot_data_ratio, 5))
  
  return(p)
}

# Function to create scatter plot of CPI vs memory bandwidth derived from LLC misses
create_cpi_vs_bandwidth_plot <- function(data, instruction_threshold, top_n_processes,
                                        start_time_seconds, end_time_seconds) {
  message("Creating CPI vs memory bandwidth scatter plot...")
  
  # Calculate aggregate LLC misses for each millisecond time slot
  ms_aggregates <- data %>%
    group_by(ms_slot) %>%
    summarise(
      total_llc_misses = sum(llc_misses, na.rm = TRUE),
      .groups = "drop"
    ) %>%
    # Calculate the aggregate memory bandwidth in GB/s for each ms_slot
    # Each LLC miss is 64 bytes, and time slots are 1ms
    # So bandwidth in GB/s = LLC misses * 64 bytes / 1ms * (1000 ms/s) / (10^9 bytes/GB)
    # Simplified: LLC misses * 64 * 1000 / 10^9 = LLC misses * 64 / 10^6 = LLC misses * 0.000064 GB/s
    mutate(global_bandwidth_gbps = total_llc_misses * 64 / 1000000)
  
  # Calculate the 99.5 percentile to filter out extreme outliers
  bandwidth_threshold <- quantile(ms_aggregates$global_bandwidth_gbps, 0.995, na.rm = TRUE)
  message("Filtering out memory bandwidth above ", format(bandwidth_threshold, digits = 4), " GB/s (99.5 percentile)")
  
  # Filter out extreme bandwidth outliers
  ms_aggregates_filtered <- ms_aggregates %>%
    filter(global_bandwidth_gbps <= bandwidth_threshold)
  
  # Join the aggregate bandwidth back to the original data
  data_with_bandwidth <- data %>%
    left_join(ms_aggregates_filtered, by = "ms_slot") %>%
    # Filter for samples with significant instruction counts and remove NAs (those were outliers)
    filter(instructions > instruction_threshold & !is.na(global_bandwidth_gbps))
  
  # Identify top processes by total instructions
  top_processes <- data %>%
    group_by(process_name) %>%
    summarise(total_instructions = sum(instructions, na.rm = TRUE)) %>%
    arrange(desc(total_instructions)) %>%
    slice_head(n = top_n_processes) %>%
    pull(process_name)
  
  message("Top ", length(top_processes), " processes by instructions for bandwidth plot:")
  print(head(top_processes, 10))
  
  # Prepare data for plotting
  plot_data <- data_with_bandwidth %>%
    # Group all non-top processes as "other"
    mutate(process_group = ifelse(process_name %in% top_processes, 
                                 as.character(process_name), 
                                 "other")) %>%
    # Cap extreme CPI values for better visualization
    mutate(cpi_capped = pmin(cpi, 10))  # Cap at 10 cycles per instruction
  
  # Reorder factor levels based on total instructions for better visualization
  process_order <- c(top_processes, "other")
  plot_data$process_group <- factor(plot_data$process_group, levels = process_order)
  
  # Create the faceted scatter plot
  p <- ggplot(plot_data, aes(x = global_bandwidth_gbps, y = cpi_capped)) +
    geom_point(alpha = 0.03, size = 0.8, color = "#377EB8") +  # Reduced alpha for better density visualization
    facet_wrap(~ process_group, scales = "free_y", ncol = 3) +  # Only free y-axis to maintain same x-axis scale
    labs(
      title = "CPI vs Global Memory Bandwidth by Process",
      subtitle = paste0("Analysis of ", start_time_seconds, "-", end_time_seconds, 
                       "s window, ", instruction_threshold, " instruction threshold, ",
                       "excluding top 0.5% bandwidth"),
      x = "Global Memory Bandwidth (GB/s)",
      y = "Cycles Per Instruction (CPI)"
    ) +
    theme_minimal() +
    theme(
      strip.background = element_rect(fill = "lightgray"),
      strip.text = element_text(face = "bold"),
      plot.title = element_text(face = "bold"),
      axis.title = element_text(face = "bold")
    )
  
  return(p)
}

# Main execution
main <- function() {
  tryCatch({
    # Check if input file exists
    if (!file.exists(input_file)) {
      stop("Input file does not exist: ", input_file)
    }
    
    message("Processing performance data...")
    perf_data <- load_and_process_parquet(input_file)
    
    # Check if we have enough data
    if (nrow(perf_data) < 10) {
      stop("Not enough data points in the input file.")
    }
    
    # Apply time window filtering
    message(sprintf("Filtering data for time window: %.1f - %.1f seconds", start_time_seconds, end_time_seconds))
    perf_data_filtered <- perf_data %>%
      filter(time_seconds >= start_time_seconds & time_seconds <= end_time_seconds)
    
    # Check if we still have enough data after time filtering
    if (nrow(perf_data_filtered) < 10) {
      stop("Not enough data points after time window filtering. Consider adjusting the time window.")
    }
    
    message(sprintf("Retained %d of %d samples (%.1f%%) after time window filtering", 
                   nrow(perf_data_filtered), nrow(perf_data), 
                   100 * nrow(perf_data_filtered) / nrow(perf_data)))
    
    # Create the CPI distribution plots comparing different LLC miss ranges
    message("Creating CPI distribution plots...")
    
    # 1. Top 5% vs Bottom 95%
    cpi_plot_5_vs_95 <- create_cpi_llc_analysis(perf_data_filtered, instruction_threshold, 
                                              top_n_processes, 0, 95, 95, 100, 
                                              start_time_seconds, end_time_seconds)
    
    # 2. Top 5% vs Middle 45-55%
    cpi_plot_5_vs_mid <- create_cpi_llc_analysis(perf_data_filtered, instruction_threshold, 
                                               top_n_processes, 45, 55, 95, 100, 
                                               start_time_seconds, end_time_seconds)
    
    # Create the CPI slowdown bar plots
    # 1. Top 5% vs Bottom 95%
    slowdown_plot_5_vs_95 <- create_cpi_slowdown_plot(perf_data_filtered, instruction_threshold, 
                                                     top_n_processes, 0, 95, 95, 100, 
                                                     start_time_seconds, end_time_seconds)
    
    # 2. Top 5% vs Middle 45-55%
    slowdown_plot_5_vs_mid <- create_cpi_slowdown_plot(perf_data_filtered, instruction_threshold, 
                                                     top_n_processes, 45, 55, 95, 100, 
                                                     start_time_seconds, end_time_seconds)
    
    # Create the CPI vs bandwidth scatter plot
    bandwidth_plot <- create_cpi_vs_bandwidth_plot(perf_data_filtered, instruction_threshold, 
                                                 top_n_processes, start_time_seconds, end_time_seconds)
    
    # Add white background for PNG version to improve visibility in dark themes
    bandwidth_plot_png_version <- bandwidth_plot + 
      theme(plot.background = element_rect(fill = "white", color = NA))
    
    # Save outputs for CPI distribution plots
    cpi_plot_5_vs_95_png <- paste0(output_file, "_dist_top5_vs_bottom95.png")
    cpi_plot_5_vs_95_pdf <- paste0(output_file, "_dist_top5_vs_bottom95.pdf")
    
    message("Saving top 5% vs bottom 95% distribution plot as PNG: ", cpi_plot_5_vs_95_png)
    ggsave(cpi_plot_5_vs_95_png, cpi_plot_5_vs_95, width = 15, height = 20, dpi = 300)
    
    message("Saving top 5% vs bottom 95% distribution plot as PDF: ", cpi_plot_5_vs_95_pdf)
    ggsave(cpi_plot_5_vs_95_pdf, cpi_plot_5_vs_95, width = 15, height = 20)
    
    cpi_plot_5_vs_mid_png <- paste0(output_file, "_dist_top5_vs_mid45-55.png")
    cpi_plot_5_vs_mid_pdf <- paste0(output_file, "_dist_top5_vs_mid45-55.pdf")
    
    message("Saving top 5% vs middle 45-55% distribution plot as PNG: ", cpi_plot_5_vs_mid_png)
    ggsave(cpi_plot_5_vs_mid_png, cpi_plot_5_vs_mid, width = 15, height = 20, dpi = 300)
    
    message("Saving top 5% vs middle 45-55% distribution plot as PDF: ", cpi_plot_5_vs_mid_pdf)
    ggsave(cpi_plot_5_vs_mid_pdf, cpi_plot_5_vs_mid, width = 15, height = 20)
    
    # Save outputs for slowdown plots
    slowdown_5_vs_95_png <- paste0(output_file, "_slowdown_top5_vs_bottom95.png")
    slowdown_5_vs_95_pdf <- paste0(output_file, "_slowdown_top5_vs_bottom95.pdf")
    
    message("Saving top 5% vs bottom 95% slowdown plot as PNG: ", slowdown_5_vs_95_png)
    ggsave(slowdown_5_vs_95_png, slowdown_plot_5_vs_95, width = 15, height = 10, dpi = 300)
    
    message("Saving top 5% vs bottom 95% slowdown plot as PDF: ", slowdown_5_vs_95_pdf)
    ggsave(slowdown_5_vs_95_pdf, slowdown_plot_5_vs_95, width = 15, height = 10)
    
    slowdown_5_vs_mid_png <- paste0(output_file, "_slowdown_top5_vs_mid45-55.png")
    slowdown_5_vs_mid_pdf <- paste0(output_file, "_slowdown_top5_vs_mid45-55.pdf")
    
    message("Saving top 5% vs middle 45-55% slowdown plot as PNG: ", slowdown_5_vs_mid_png)
    ggsave(slowdown_5_vs_mid_png, slowdown_plot_5_vs_mid, width = 15, height = 10, dpi = 300)
    
    message("Saving top 5% vs middle 45-55% slowdown plot as PDF: ", slowdown_5_vs_mid_pdf)
    ggsave(slowdown_5_vs_mid_pdf, slowdown_plot_5_vs_mid, width = 15, height = 10)
    
    # Save outputs for bandwidth plot
    bandwidth_plot_png <- paste0(output_file, "_cpi_vs_bandwidth.png")
    bandwidth_plot_pdf <- paste0(output_file, "_cpi_vs_bandwidth.pdf")
    
    message("Saving CPI vs bandwidth plot as PNG (with white background): ", bandwidth_plot_png)
    ggsave(bandwidth_plot_png, bandwidth_plot_png_version, width = 15, height = 20, dpi = 300, bg = "white")
    
    message("Saving CPI vs bandwidth plot as PDF: ", bandwidth_plot_pdf)
    ggsave(bandwidth_plot_pdf, bandwidth_plot, width = 15, height = 20)
    
    message("Done!")
  }, error = function(e) {
    message("Error: ", e$message)
    quit(status = 1)
  })
}

# Execute main function
main()