#!/usr/bin/env Rscript

# Setup - load required libraries
if (!requireNamespace("nanoparquet", quietly = TRUE)) {
  install.packages("nanoparquet")
}
if (!requireNamespace("ggplot2", quietly = TRUE)) {
  install.packages("ggplot2")
}
if (!requireNamespace("dplyr", quietly = TRUE)) {
  install.packages("dplyr")
}
if (!requireNamespace("tidyr", quietly = TRUE)) {
  install.packages("tidyr")
}

library(nanoparquet)
library(ggplot2)
library(dplyr)
library(tidyr)

# Parse command line arguments
args <- commandArgs(trailingOnly = TRUE)
input_file <- if(length(args) >= 1) args[1] else "collector-parquet.parquet"
start_time_offset <- if(length(args) >= 2) as.numeric(args[2]) else 110  # Default to 110 seconds after start
window_size <- if(length(args) >= 3) as.numeric(args[3]) else 1  # Default to 1 second window
output_file <- if(length(args) >= 4) args[4] else "memory_usage"
top_n_processes <- if(length(args) >= 5) as.numeric(args[5]) else 15  # Default to showing top processes

# Cache line size in bytes
CACHE_LINE_SIZE <- 64
# Bytes in a gigabyte (2^30)
BYTES_PER_GB <- 1073741824
# Nanoseconds in a second
NS_PER_SEC <- 1e9
# Nanoseconds in a millisecond
NS_PER_MS <- 1e6

# Function to load and process parquet data
load_and_process_parquet <- function(file_path, time_offset_pct, window_size_pct) {
  # Read the parquet file
  message("Reading parquet file: ", file_path)
  perf_data <- nanoparquet::read_parquet(file_path)
  
  # Convert start_time to nanoseconds since beginning of experiment
  # Normalize by subtracting the minimum start_time to get relative time
  # Keep integer precision by working with nanoseconds
  min_time <- min(perf_data$start_time, na.rm = TRUE)
  
  # Calculate window boundaries in nanoseconds
  # Convert parameters to numeric explicitly to prevent NA errors
  window_start_ns <- as.numeric(time_offset_pct) * NS_PER_SEC
  window_end_ns <- as.numeric(time_offset_pct + window_size_pct) * NS_PER_SEC
  
  # Calculate relative time in nanoseconds (preserving precision)
  perf_data$relative_time_ns <- perf_data$start_time - min_time
  
  message("Filtering data for time window: ", 
          window_start_ns / NS_PER_SEC, " to ", 
          window_end_ns / NS_PER_SEC, " seconds (absolute)")
  
  # Filter data within the window
  window_data <- perf_data %>%
    filter(relative_time_ns >= window_start_ns & 
           relative_time_ns <= window_end_ns)
  
  # Calculate millisecond timestamps within the window
  # Convert to 0-based milliseconds within the window
  window_data$ms_bucket <- as.integer((window_data$relative_time_ns - window_start_ns) / NS_PER_MS)
  
  # Replace NULL process names with "kernel" for better visualization
  window_data$process_name[is.na(window_data$process_name)] <- "kernel"
  
  # Ensure process_name is a factor with ordered levels based on total memory usage (LLC misses + cache references)
  process_totals <- window_data %>%
    group_by(process_name) %>%
    summarise(
      total_llc_misses = sum(llc_misses, na.rm = TRUE),
      total_cache_references = sum(cache_references, na.rm = TRUE),
      total_memory_usage = total_llc_misses + total_cache_references
    ) %>%
    arrange(desc(total_memory_usage))
  
  window_data$process_name <- factor(window_data$process_name, 
                                     levels = process_totals$process_name)
  
  # Store the time window information for plot titles (in seconds)
  attr(window_data, "window_start_s") <- window_start_ns / NS_PER_SEC
  attr(window_data, "window_end_s") <- window_end_ns / NS_PER_SEC
  
  return(window_data)
}

# Function to prepare plot data with common logic for both plot types
prepare_plot_data <- function(data, n_top_processes = top_n_processes) {
  # Calculate original totals for validation
  totals_original <- list(
    llc_misses = sum(data$llc_misses, na.rm = TRUE),
    cache_references = if("cache_references" %in% colnames(data)) sum(data$cache_references, na.rm = TRUE) else 0
  )
  
  # Select top processes by total memory usage (LLC misses + cache references)
  top_processes <- data %>%
    group_by(process_name) %>%
    summarise(
      total_llc_misses = sum(llc_misses, na.rm = TRUE),
      total_cache_references = if("cache_references" %in% colnames(data)) sum(cache_references, na.rm = TRUE) else 0,
      total_memory_usage = total_llc_misses + total_cache_references
    ) %>%
    arrange(desc(total_memory_usage)) %>%
    slice_head(n = n_top_processes) %>%
    pull(process_name)
  
  # Calculate coverage percentages
  coverage <- list(
    llc_misses = sum(data$llc_misses[data$process_name %in% top_processes], na.rm = TRUE) / totals_original$llc_misses * 100
  )
  
  if("cache_references" %in% colnames(data)) {
    coverage$cache_references <- sum(data$cache_references[data$process_name %in% top_processes], na.rm = TRUE) / 
                                totals_original$cache_references * 100
  }
  
  message("Top ", n_top_processes, " processes account for ",
          round(coverage$llc_misses, 2), "% of total LLC misses",
          if("cache_references" %in% colnames(data)) paste0(" and ", round(coverage$cache_references, 2), "% of total cache references") else "")
  
  # Filter data for top processes and group the rest as "other"
  plot_data <- data %>%
    mutate(process_group = ifelse(process_name %in% top_processes, 
                                 as.character(process_name), 
                                 "other"))
  
  # Aggregate by millisecond bucket and process group
  if("cache_references" %in% colnames(data)) {
    ms_data <- plot_data %>%
      group_by(ms_bucket, process_group) %>%
      summarise(
        llc_misses = sum(llc_misses, na.rm = TRUE),
        cache_references = sum(cache_references, na.rm = TRUE),
        .groups = 'drop'
      ) %>%
      ungroup()
  } else {
    ms_data <- plot_data %>%
      group_by(ms_bucket, process_group) %>%
      summarise(
        llc_misses = sum(llc_misses, na.rm = TRUE),
        .groups = 'drop'
      ) %>%
      ungroup()
  }

  # Validate that no data was lost during aggregation
  totals_after <- list(
    llc_misses = sum(ms_data$llc_misses, na.rm = TRUE)
  )
  
  if("cache_references" %in% colnames(ms_data)) {
    totals_after$cache_references = sum(ms_data$cache_references, na.rm = TRUE)
  }
  
  llc_diff <- abs(totals_original$llc_misses - totals_after$llc_misses)
  cache_diff <- if("cache_references" %in% colnames(ms_data)) {
    abs(totals_original$cache_references - totals_after$cache_references)
  } else 0
  
  if (llc_diff > 0.01 || cache_diff > 0.01) {
    warning("Possible data loss in aggregation. LLC misses: Original=", 
            totals_original$llc_misses, ", After=", totals_after$llc_misses,
            if("cache_references" %in% colnames(ms_data)) {
              paste0(". Cache references: Original=", totals_original$cache_references, 
                     ", After=", totals_after$cache_references)
            } else "")
  } else {
    message("Aggregation validation passed. All data accounted for.")
  }
  
  # Generate colors for the plot
  all_colors <- colorRampPalette(
    c("#E41A1C", "#377EB8", "#4DAF4A", "#984EA3", "#FF7F00", 
      "#FFFF33", "#A65628", "#F781BF", "#999999")
  )(length(unique(ms_data$process_group)))
  
  names(all_colors) <- unique(ms_data$process_group)
  
  # If "other" is present, make it gray and place it at the bottom of the stack
  if ("other" %in% names(all_colors)) {
    all_colors["other"] <- "#CCCCCC"  # Gray for "other"
    
    # Reorder factor levels to ensure "other" is at the bottom of the stack
    process_groups <- setdiff(names(all_colors), "other")
    ms_data$process_group <- factor(ms_data$process_group, 
                                  levels = c(process_groups, "other"))
  }
  
  # Return structured data for plotting
  return(list(
    ms_data = ms_data,
    colors = all_colors,
    totals_original = totals_original,
    coverage = coverage,
    has_cache_references = "cache_references" %in% colnames(ms_data)
  ))
}

# Function to create the combined memory usage plot with both LLC misses and cache references
create_memory_usage_plot <- function(data, plot_data, n_top_processes = top_n_processes) {
  # Get time window info from attributes
  subtitle <- paste0("1-second window at ", start_time_offset, 
                    " seconds after experiment start (showing top ", n_top_processes, " processes)")
  
  # Convert to long format for faceted plotting
  ms_data_long <- plot_data$ms_data %>%
    pivot_longer(
      cols = c(llc_misses, cache_references),
      names_to = "metric_type",
      values_to = "count"
    ) %>%
    mutate(
      bytes = count * CACHE_LINE_SIZE,
      gb_per_second = (bytes / BYTES_PER_GB) * 1000,  # Convert to GB/s (1000 millisecond frames per second)
      metric_label = case_when(
        metric_type == "llc_misses" ~ "LLC Misses",
        metric_type == "cache_references" ~ "Cache References",
        TRUE ~ metric_type
      )
    )
  
  # Calculate the total GB/s for sanity checking
  total_gb_per_second <- ms_data_long %>%
    group_by(ms_bucket, metric_type) %>%
    summarise(total_gb_per_second = sum(gb_per_second, na.rm = TRUE), .groups = 'drop')
  
  # Calculate summary statistics
  summary_stats <- total_gb_per_second %>%
    group_by(metric_type) %>%
    summarise(
      median_gb = median(total_gb_per_second, na.rm = TRUE),
      max_gb = max(total_gb_per_second, na.rm = TRUE),
      .groups = 'drop'
    )
  
  for (i in 1:nrow(summary_stats)) {
    metric <- summary_stats$metric_type[i]
    message(metric, " - Median: ", round(summary_stats$median_gb[i], 2), 
            " GB/s, Maximum: ", round(summary_stats$max_gb[i], 2), " GB/s")
  }
  
  # Use the same process group factor levels from plot_data
  ms_data_long$process_group <- factor(ms_data_long$process_group, 
                                      levels = levels(plot_data$ms_data$process_group))
  
  # Order the metric facets with LLC Misses on top, Cache References on bottom
  ms_data_long$metric_label <- factor(ms_data_long$metric_label, 
                                      levels = c("LLC Misses", "Cache References"))
  
  # Create the faceted stacked area plot
  p <- ggplot(ms_data_long, aes(x = ms_bucket, y = gb_per_second, fill = process_group)) +
    geom_col(position = "stack", width = 1.0, alpha = 0.8) +
    facet_grid(metric_label ~ ., scales = "free_y") +
    scale_fill_manual(values = plot_data$colors) +
    scale_y_continuous(labels = function(x) sprintf("%.2f", x)) +
    labs(
      title = "Memory Bandwidth by Process: LLC Misses and Cache References",
      subtitle = subtitle,
      x = "Time (milliseconds)",
      y = "Gigabytes Per Second",
      fill = "Process"
    ) +
    theme_minimal() +
    theme(
      legend.position = "right",
      panel.grid.minor = element_blank(),
      plot.title = element_text(face = "bold", size = 16),
      plot.subtitle = element_text(size = 12),
      axis.title = element_text(face = "bold", size = 14),
      axis.text = element_text(size = 12),
      legend.title = element_text(face = "bold", size = 12),
      legend.text = element_text(size = 10),
      strip.text = element_text(face = "bold", size = 12),
      panel.spacing = unit(0.8, "lines")
    )
  
  return(p)
}

# Function to create the LLC misses plot (for backward compatibility)
create_llc_misses_plot <- function(data, plot_data, n_top_processes = top_n_processes) {
  # Get time window info from attributes
  subtitle <- paste0("1-second window at ", start_time_offset, 
                    " seconds after experiment start (showing top ", n_top_processes, " processes)")
  
  # Convert LLC misses to GB/s
  ms_data <- plot_data$ms_data %>%
    mutate(
      bytes = llc_misses * CACHE_LINE_SIZE,
      gb_per_second = (bytes / BYTES_PER_GB) * 1000  # Convert to GB/s (1000 millisecond frames per second)
    )
  
  # Calculate the total GB/s for sanity checking
  total_gb_per_second <- ms_data %>%
    group_by(ms_bucket) %>%
    summarise(total_gb_per_second = sum(gb_per_second, na.rm = TRUE)) %>%
    ungroup()
  
  median_total_gb <- median(total_gb_per_second$total_gb_per_second, na.rm = TRUE)
  max_total_gb <- max(total_gb_per_second$total_gb_per_second, na.rm = TRUE)
  
  message("Median total memory bandwidth: ", round(median_total_gb, 2), " GB/s")
  message("Maximum total memory bandwidth: ", round(max_total_gb, 2), " GB/s")
  
  # Create the stacked area plot
  p <- ggplot(ms_data, aes(x = ms_bucket, y = gb_per_second, fill = process_group)) +
    geom_col(position = "stack", width = 1.0, alpha = 0.8) +
    scale_fill_manual(values = plot_data$colors) +
    scale_y_continuous(labels = function(x) sprintf("%.2f GB/s", x)) +
    labs(
      title = "Memory Bandwidth from LLC Misses by Process",
      subtitle = subtitle,
      x = "Time (milliseconds)",
      y = "Gigabytes Per Second",
      fill = "Process"
    ) +
    theme_minimal() +
    theme(
      legend.position = "right",
      panel.grid.minor = element_blank(),
      plot.title = element_text(face = "bold", size = 16),
      plot.subtitle = element_text(size = 12),
      axis.title = element_text(face = "bold", size = 14),
      axis.text = element_text(size = 12),
      legend.title = element_text(face = "bold", size = 12),
      legend.text = element_text(size = 10)
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
    
    message("Processing memory usage data...")
    window_data <- load_and_process_parquet(input_file, start_time_offset, window_size)
    
    # Check if we have enough data
    if (nrow(window_data) < 10) {
      stop("Not enough data points in the selected time window. Try a different time offset or window size.")
    }
    
    message("Preparing plot data...")
    plot_data <- prepare_plot_data(window_data, top_n_processes)
    
    # Check if cache_references column exists
    if (plot_data$has_cache_references) {
      message("Creating combined memory usage plot...")
      memory_plot <- create_memory_usage_plot(window_data, plot_data, top_n_processes)
      
      # Save combined plot
      combined_png_filename <- paste0(output_file, "_combined.png")
      combined_pdf_filename <- paste0(output_file, "_combined.pdf")
      
      message("Saving combined plot as PNG: ", combined_png_filename)
      # Use 16:9 aspect ratio for slides, with appropriate dimensions
      ggsave(combined_png_filename, memory_plot, width = 16, height = 9, dpi = 300)
      
      message("Saving combined plot as PDF: ", combined_pdf_filename)
      ggsave(combined_pdf_filename, memory_plot, width = 16, height = 9)
    } else {
      message("Warning: cache_references column not found in data. Skipping combined plot.")
    }
    
    message("Creating LLC misses plot...")
    llc_plot <- create_llc_misses_plot(window_data, plot_data, top_n_processes)
    
    # Save LLC misses plot (for backward compatibility)
    llc_png_filename <- paste0(output_file, ".png")
    llc_pdf_filename <- paste0(output_file, ".pdf")
    
    message("Saving LLC misses plot as PNG: ", llc_png_filename)
    # Use 16:9 aspect ratio for slides
    ggsave(llc_png_filename, llc_plot, width = 16, height = 9, dpi = 300)
    
    message("Saving LLC misses plot as PDF: ", llc_pdf_filename)
    ggsave(llc_pdf_filename, llc_plot, width = 16, height = 9)
    
    message("Done!")
  }, error = function(e) {
    message("Error: ", e$message)
    quit(status = 1)
  })
}

# Execute main function
main() 