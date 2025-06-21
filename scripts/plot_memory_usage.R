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
  
  # Add sanity checking and cache hits calculation if cache_references exists
  if("cache_references" %in% colnames(data)) {
    # Check for anomalous cases where LLC misses exceed cache references
    anomalous_cases <- data %>%
      filter(llc_misses > cache_references) %>%
      mutate(negative_diff = llc_misses - cache_references)
    
    if(nrow(anomalous_cases) > 0) {
      # Report anomalous cases by process
      anomaly_summary <- anomalous_cases %>%
        group_by(process_name) %>%
        summarise(
          anomalous_count = n(),
          total_negative_diff = sum(negative_diff, na.rm = TRUE),
          .groups = 'drop'
        ) %>%
        arrange(desc(total_negative_diff))
      
      message("\n=== SANITY CHECK RESULTS ===")
      message("Found ", nrow(anomalous_cases), " time slots where LLC misses > cache references")
      message("This represents ", round(nrow(anomalous_cases) / nrow(data) * 100, 2), "% of all measurements")
      
      total_negative_reads <- sum(anomaly_summary$total_negative_diff)
      message("Total negative difference across all anomalous cases: ", total_negative_reads, " cache line reads")
      message("This is ", round(total_negative_reads / totals_original$llc_misses * 100, 2), "% of total LLC misses")
      
      message("\nBreakdown by process:")
      for(i in 1:nrow(anomaly_summary)) {
        process <- anomaly_summary$process_name[i]
        count <- anomaly_summary$anomalous_count[i]
        diff <- anomaly_summary$total_negative_diff[i]
        message("  ", process, ": ", count, " anomalous time slots, ", diff, " negative cache line reads")
      }
      message("=== END SANITY CHECK ===\n")
    } else {
      message("\n=== SANITY CHECK RESULTS ===")
      message("No anomalous cases found - all cache references >= LLC misses")
      message("=== END SANITY CHECK ===\n")
    }
    
    # Calculate cache hits = cache references - LLC misses, ensuring non-negative values
    data$cache_hits <- pmax(0, data$cache_references - data$llc_misses)
    
    # Update totals to include cache hits
    totals_original$cache_hits <- sum(data$cache_hits, na.rm = TRUE)
    
    message("Cache hits calculation: ", totals_original$cache_hits, " total cache hits")
    message("Hit rate: ", round(totals_original$cache_hits / totals_original$cache_references * 100, 2), "%")
  }
  
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
    coverage$cache_hits <- sum(data$cache_hits[data$process_name %in% top_processes], na.rm = TRUE) / 
                          totals_original$cache_hits * 100
  }
  
  message("Top ", n_top_processes, " processes account for ",
          round(coverage$llc_misses, 2), "% of total LLC misses",
          if("cache_references" %in% colnames(data)) paste0(", ", round(coverage$cache_references, 2), "% of total cache references, and ", round(coverage$cache_hits, 2), "% of total cache hits") else "")
  
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
        cache_hits = sum(cache_hits, na.rm = TRUE),
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
    totals_after$cache_hits = sum(ms_data$cache_hits, na.rm = TRUE)
  }
  
  llc_diff <- abs(totals_original$llc_misses - totals_after$llc_misses)
  cache_diff <- if("cache_references" %in% colnames(ms_data)) {
    abs(totals_original$cache_references - totals_after$cache_references)
  } else 0
  hits_diff <- if("cache_hits" %in% colnames(ms_data)) {
    abs(totals_original$cache_hits - totals_after$cache_hits)
  } else 0
  
  if (llc_diff > 0.01 || cache_diff > 0.01 || hits_diff > 0.01) {
    warning("Possible data loss in aggregation. LLC misses: Original=", 
            totals_original$llc_misses, ", After=", totals_after$llc_misses,
            if("cache_references" %in% colnames(ms_data)) {
              paste0(". Cache references: Original=", totals_original$cache_references, 
                     ", After=", totals_after$cache_references,
                     ". Cache hits: Original=", totals_original$cache_hits,
                     ", After=", totals_after$cache_hits)
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

# Function to create the combined memory usage plot with both LLC misses and cache hits
create_memory_usage_plot <- function(data, plot_data, n_top_processes = top_n_processes) {
  # Get time window info from attributes
  subtitle <- paste0("1-second window at ", start_time_offset, 
                    " seconds after experiment start (showing top ", n_top_processes, " processes)")
  
  # Convert to long format for faceted plotting
  ms_data_long <- plot_data$ms_data %>%
    pivot_longer(
      cols = c(llc_misses, cache_hits),
      names_to = "metric_type",
      values_to = "count"
    ) %>%
    mutate(
      bytes = count * CACHE_LINE_SIZE,
      gb_per_second = (bytes / BYTES_PER_GB) * 1000,  # Convert to GB/s (1000 millisecond frames per second)
      metric_label = case_when(
        metric_type == "llc_misses" ~ "LLC Misses",
        metric_type == "cache_hits" ~ "Cache Hits",
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
  
  # Order the metric facets with LLC Misses on top, Cache Hits on bottom
  ms_data_long$metric_label <- factor(ms_data_long$metric_label, 
                                      levels = c("LLC Misses", "Cache Hits"))
  
  # Create the faceted stacked area plot
  p <- ggplot(ms_data_long, aes(x = ms_bucket, y = gb_per_second, fill = process_group)) +
    geom_col(position = "stack", width = 1.0, alpha = 0.8) +
    facet_grid(metric_label ~ ., scales = "free_y") +
    scale_fill_manual(values = plot_data$colors) +
    scale_y_continuous(labels = function(x) sprintf("%.2f", x)) +
    labs(
      title = "Memory Bandwidth by Process: LLC Misses and Cache Hits",
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

# Function to create dual-panel plot: LLC misses on top, CPI scatter on bottom
create_dual_panel_plot <- function(data, plot_data, n_top_processes = top_n_processes) {
  # Get time window info from attributes
  subtitle <- paste0("1-second window at ", start_time_offset, 
                    " seconds after experiment start (showing top ", n_top_processes, " processes)")
  
  # Prepare LLC misses stacked plot data (top panel)
  llc_data <- plot_data$ms_data %>%
    mutate(
      bytes = llc_misses * CACHE_LINE_SIZE,
      gb_per_second = (bytes / BYTES_PER_GB) * 1000  # Convert to GB/s
    )
  
  # Create top panel: LLC misses stacked area plot
  top_panel <- ggplot(llc_data, aes(x = ms_bucket, y = gb_per_second, fill = process_group)) +
    geom_col(position = "stack", width = 1.0, alpha = 0.8) +
    scale_fill_manual(values = plot_data$colors) +
    scale_y_continuous(labels = function(x) sprintf("%.2f", x)) +
    labs(
      title = "Memory Bandwidth Analysis: LLC Misses and CPI",
      subtitle = subtitle,
      x = NULL,  # No x-axis label for top panel
      y = "LLC Misses (GB/s)",
      fill = "Process"
    ) +
    theme_minimal() +
    theme(
      legend.position = "right",
      panel.grid.minor = element_blank(),
      plot.title = element_text(face = "bold", size = 16),
      plot.subtitle = element_text(size = 12),
      axis.title = element_text(face = "bold", size = 12),
      axis.text = element_text(size = 10),
      axis.text.x = element_blank(),  # Remove x-axis text for top panel
      legend.title = element_text(face = "bold", size = 10),
      legend.text = element_text(size = 8),
      plot.margin = margin(t = 20, r = 20, b = 0, l = 20, unit = "pt")
    )
  
  # Prepare CPI deviation stacked plot data (bottom panel)
  # Calculate CPI and median baselines for each process
  cpi_data <- data %>%
    filter(process_name %in% levels(plot_data$ms_data$process_group)[levels(plot_data$ms_data$process_group) != "other"]) %>%
    mutate(
      ms_bucket = as.integer((relative_time_ns - attr(data, "window_start_s") * NS_PER_SEC) / NS_PER_MS),
      cpi = ifelse(instructions > 0, cycles / instructions, NA),  # Avoid division by zero
      process_name = factor(process_name, levels = levels(plot_data$ms_data$process_group))
    ) %>%
    filter(!is.na(cpi), cpi > 0, cpi < 10)  # Filter out invalid CPI values
  
  # Calculate median CPI for each process across an expanded window (5 seconds before + 5 seconds after)
  # First, create expanded window boundaries
  expanded_window_start_ns <- attr(data, "window_start_s") * NS_PER_SEC - 5 * NS_PER_SEC  # 5 seconds before
  expanded_window_end_ns <- (attr(data, "window_start_s") + window_size) * NS_PER_SEC + 5 * NS_PER_SEC  # 5 seconds after
  
  message("Computing baseline CPI using expanded window: ", 
          expanded_window_start_ns / NS_PER_SEC, " to ", 
          expanded_window_end_ns / NS_PER_SEC, " seconds")
  
  # Get expanded dataset for baseline calculation
  expanded_baseline_data <- data %>%
    filter(
      relative_time_ns >= pmax(0, expanded_window_start_ns),  # Don't go before start of file
      relative_time_ns <= expanded_window_end_ns,             # Don't go past end of file
      process_name %in% levels(plot_data$ms_data$process_group)[levels(plot_data$ms_data$process_group) != "other"],
      instructions > 100000  # Only use high-quality samples with >100k instructions
    ) %>%
    mutate(
      cpi = ifelse(instructions > 0, cycles / instructions, NA),
      process_name = factor(process_name, levels = levels(plot_data$ms_data$process_group))
    ) %>%
    filter(!is.na(cpi), cpi > 0, cpi < 10)  # Filter out invalid CPI values
  
  # Calculate median CPI for each process using the expanded, filtered dataset
  process_medians <- expanded_baseline_data %>%
    group_by(process_name) %>%
    summarise(
      median_cpi = median(cpi, na.rm = TRUE),
      baseline_samples = n(),
      .groups = 'drop'
    )
  
  message("Baseline CPI calculation summary:")
  for (i in 1:nrow(process_medians)) {
    process <- process_medians$process_name[i]
    median_val <- process_medians$median_cpi[i]
    samples <- process_medians$baseline_samples[i]
    message("  ", process, ": median CPI = ", round(median_val, 3), 
            " (", samples, " high-quality samples)")
  }
  
  # Join medians back and calculate deviations
  cpi_deviations <- cpi_data %>%
    left_join(process_medians, by = "process_name") %>%
    mutate(
      cpi_deviation = cpi - median_cpi,
      deviation_type = ifelse(cpi_deviation >= 0, "positive", "negative"),
      abs_deviation = abs(cpi_deviation)
    ) %>%
    filter(!is.na(cpi_deviation))
  
  # Aggregate deviations by millisecond bucket and process
  deviation_data <- cpi_deviations %>%
    group_by(ms_bucket, process_name, deviation_type) %>%
    summarise(total_deviation = sum(cpi_deviation, na.rm = TRUE), .groups = 'drop') %>%
    ungroup()
  
  message("CPI deviation summary:")
  summary_stats <- deviation_data %>%
    group_by(deviation_type) %>%
    summarise(
      mean_deviation = mean(abs(total_deviation), na.rm = TRUE),
      max_deviation = max(abs(total_deviation), na.rm = TRUE),
      .groups = 'drop'
    )
  for (i in 1:nrow(summary_stats)) {
    message("  ", summary_stats$deviation_type[i], " deviations - Mean: ", 
            round(summary_stats$mean_deviation[i], 3), ", Max: ", 
            round(summary_stats$max_deviation[i], 3))
  }
  
     # Create middle panel: CPI deviation stacked plot centered at zero
   middle_panel <- ggplot(deviation_data, aes(x = ms_bucket, y = total_deviation, fill = process_name)) +
     geom_col(position = "stack", width = 1.0, alpha = 0.8) +
     geom_hline(yintercept = 0, color = "black", size = 0.5) +  # Zero reference line
     scale_fill_manual(values = plot_data$colors[names(plot_data$colors) %in% unique(deviation_data$process_name)]) +
     scale_y_continuous(labels = function(x) sprintf("%.3f", x)) +
     labs(
       x = NULL,  # No x-axis label for middle panel
       y = "CPI Deviation from Median",
       fill = "Process"
     ) +
     theme_minimal() +
     theme(
       legend.position = "none",  # Legend already shown in top panel
       panel.grid.minor = element_blank(),
       panel.grid.major.x = element_blank(),
       axis.title = element_text(face = "bold", size = 12),
       axis.text = element_text(size = 10),
       axis.text.x = element_blank(),  # Remove x-axis text for middle panel
       plot.margin = margin(t = 0, r = 20, b = 0, l = 20, unit = "pt")
     )
   
   # Prepare cycle deviation data (bottom panel)
   # Calculate expected cycles using median CPI and compare to actual cycles
   cycle_deviations <- cpi_data %>%
     left_join(process_medians, by = "process_name") %>%
     mutate(
       expected_cycles = instructions * median_cpi,
       cycle_deviation = cycles - expected_cycles
     ) %>%
     filter(!is.na(cycle_deviation))
   
   # Aggregate cycle deviations by millisecond bucket and process
   cycle_deviation_data <- cycle_deviations %>%
     group_by(ms_bucket, process_name) %>%
     summarise(total_cycle_deviation = sum(cycle_deviation, na.rm = TRUE), .groups = 'drop') %>%
     ungroup()
   
   message("Cycle deviation summary:")
   cycle_summary_stats <- cycle_deviation_data %>%
     mutate(deviation_type = ifelse(total_cycle_deviation >= 0, "positive", "negative")) %>%
     group_by(deviation_type) %>%
     summarise(
       mean_deviation = mean(abs(total_cycle_deviation), na.rm = TRUE),
       max_deviation = max(abs(total_cycle_deviation), na.rm = TRUE),
       .groups = 'drop'
     )
   for (i in 1:nrow(cycle_summary_stats)) {
     message("  ", cycle_summary_stats$deviation_type[i], " cycle deviations - Mean: ", 
             format(cycle_summary_stats$mean_deviation[i], scientific = TRUE, digits = 3), 
             ", Max: ", format(cycle_summary_stats$max_deviation[i], scientific = TRUE, digits = 3))
   }
   
   # Create bottom panel: Cycle deviation stacked plot centered at zero
   bottom_panel <- ggplot(cycle_deviation_data, aes(x = ms_bucket, y = total_cycle_deviation, fill = process_name)) +
     geom_col(position = "stack", width = 1.0, alpha = 0.8) +
     geom_hline(yintercept = 0, color = "black", size = 0.5) +  # Zero reference line
     scale_fill_manual(values = plot_data$colors[names(plot_data$colors) %in% unique(cycle_deviation_data$process_name)]) +
     scale_y_continuous(labels = function(x) format(x, scientific = TRUE, digits = 2)) +
     labs(
       x = "Time (milliseconds)",
       y = "Cycle Deviation from Expected",
       fill = "Process"
     ) +
     theme_minimal() +
     theme(
       legend.position = "none",  # Legend already shown in top panel
       panel.grid.minor = element_blank(),
       panel.grid.major.x = element_blank(),
       axis.title = element_text(face = "bold", size = 12),
       axis.text = element_text(size = 10),
       plot.margin = margin(t = 0, r = 20, b = 20, l = 20, unit = "pt")
     )
  
  # Combine panels using patchwork or grid
  if (!requireNamespace("patchwork", quietly = TRUE)) {
    install.packages("patchwork", repos = "https://cloud.r-project.org/")
  }
  library(patchwork)
  
     combined_plot <- top_panel / middle_panel / bottom_panel + 
     plot_layout(heights = c(1, 1, 1))  # Equal height for all three panels
  
  return(combined_plot)
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
      
      message("Creating dual-panel plot (LLC misses + CPI)...")
      dual_panel_plot <- create_dual_panel_plot(window_data, plot_data, top_n_processes)
      
      # Save dual-panel plot
      dual_png_filename <- paste0(output_file, "_dual_panel.png")
      dual_pdf_filename <- paste0(output_file, "_dual_panel.pdf")
      
      message("Saving dual-panel plot as PNG: ", dual_png_filename)
      # Use taller aspect ratio to accommodate three panels
      ggsave(dual_png_filename, dual_panel_plot, width = 16, height = 18, dpi = 300)
      
      message("Saving dual-panel plot as PDF: ", dual_pdf_filename)
      ggsave(dual_pdf_filename, dual_panel_plot, width = 16, height = 18)
    } else {
      message("Warning: cache_references column not found in data. Skipping combined and dual-panel plots.")
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