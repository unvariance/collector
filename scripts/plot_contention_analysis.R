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
sample_rate <- if(length(args) >= 5) as.numeric(args[5]) else 0.05  # Default to 5% sampling

# Constants
NS_PER_SEC <- 1e9

# Function to load and process parquet data
load_and_process_parquet <- function(file_path, window_duration_sec) {
  # Read the parquet file
  message("Reading parquet file: ", file_path)
  perf_data <- nanoparquet::read_parquet(file_path)
  
  # Convert start_time to relative time in nanoseconds
  min_time <- min(perf_data$start_time, na.rm = TRUE)
  max_time <- max(perf_data$start_time, na.rm = TRUE)
  
  # Calculate window start time (last N seconds)
  window_start_ns <- max_time - window_duration_sec * NS_PER_SEC
  
  message("Using data from last ", window_duration_sec, " seconds of experiment")
  message("Window: ", window_start_ns / NS_PER_SEC, " to ", max_time / NS_PER_SEC, " seconds (absolute)")
  
  # Filter data within the window
  window_data <- perf_data %>%
    filter(start_time >= window_start_ns) %>%
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
prepare_contention_data <- function(data, n_top_processes = top_n_processes, sample_rate = 0.05) {
  
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
  
  # FOURTH: Compute smooth percentile curves for CPI coloring (similar to plot_instructions_vs_cpi.R)
  message("Computing smooth CPI percentile curves...")
  
  # Define percentiles to compute (every 10th + 5th and 95th)
  percentiles <- c(0.05, seq(0.1, 0.9, by = 0.1), 0.95)
  
  # Calculate smooth percentile curves for each process
  percentile_data <- data.frame()
  
  for (proc in unique(plot_data$process_name)) {
    proc_data <- plot_data[plot_data$process_name == proc, ]
    
    if (nrow(proc_data) > 20) {  # Need minimum data for percentiles
      # Create instruction sequence for smooth curves
      log_inst_range <- range(log10(proc_data$instructions))
      log_inst_seq <- seq(log_inst_range[1], log_inst_range[2], length.out = 50)
      inst_seq <- 10^log_inst_seq
      
      # Calculate percentiles for each instruction level
      for (p in percentiles) {
        percentile_values <- rep(NA, length(inst_seq))
        
        for (i in 1:length(inst_seq)) {
          # Find nearby points (within a window on log scale)
          window_size <- diff(log_inst_range) / 20  # Adaptive window size
          nearby_idx <- abs(log10(proc_data$instructions) - log_inst_seq[i]) <= window_size
          
          if (sum(nearby_idx) >= 5) {  # Need at least 5 points for percentile
            percentile_values[i] <- quantile(proc_data$cpi[nearby_idx], p, na.rm = TRUE)
          }
        }
        
        # Remove NAs and smooth the percentile curve
        valid_idx <- !is.na(percentile_values)
        if (sum(valid_idx) >= 3) {
          # Use loess smoothing for the percentile curve
          smooth_fit <- loess(percentile_values[valid_idx] ~ log_inst_seq[valid_idx], span = 0.5)
          smoothed_values <- predict(smooth_fit, log_inst_seq)
          
          # Add to combined data
          proc_percentile <- data.frame(
            process_name = proc,
            log_inst_seq = log_inst_seq,
            inst_seq = inst_seq,
            cpi_value = smoothed_values,
            percentile = p * 100
          )
          percentile_data <- rbind(percentile_data, proc_percentile)
        }
      }
    }
  }
  
  # FIFTH: Interpolate CPI percentiles for each data point using fast vectorized approach
  message("Interpolating CPI percentiles for color mapping...")
  
  contention_data <- plot_data
  contention_data$cpi_percentile <- NA
  
  # Use a much more efficient approach with pre-computed interpolation functions
  for (proc in unique(plot_data$process_name)) {
    proc_data_idx <- which(plot_data$process_name == proc)
    proc_percentiles <- percentile_data[percentile_data$process_name == proc, ]
    
    if (nrow(proc_percentiles) > 0 && length(proc_data_idx) > 0) {
      message("  Processing ", length(proc_data_idx), " points for ", proc, "...")
      
      # Get data for this process
      proc_log_inst <- log10(plot_data$instructions[proc_data_idx])
      proc_cpi <- plot_data$cpi[proc_data_idx]
      
      # Create interpolation functions for each percentile curve
      unique_percentiles <- sort(unique(proc_percentiles$percentile))
      percentile_functions <- list()
      
      for (p in unique_percentiles) {
        p_data <- proc_percentiles[proc_percentiles$percentile == p, ]
        if (nrow(p_data) > 1) {
          # Create interpolation function for this percentile curve
          percentile_functions[[as.character(p)]] <- approxfun(p_data$log_inst_seq, p_data$cpi_value, rule = 2)
        }
      }
      
      if (length(percentile_functions) >= 2) {
        # Vectorized approach: evaluate all percentile functions at all instruction points
        percentile_matrix <- matrix(NA, nrow = length(proc_log_inst), ncol = length(percentile_functions))
        colnames(percentile_matrix) <- names(percentile_functions)
        
        # Evaluate all percentile functions at once
        for (i in seq_along(percentile_functions)) {
          percentile_matrix[, i] <- percentile_functions[[i]](proc_log_inst)
        }
        
        # Fully vectorized percentile assignment using apply
        percentile_names <- as.numeric(names(percentile_functions))
        
        percentile_values <- sapply(seq_along(proc_cpi), function(i) {
          cpi_val <- proc_cpi[i]
          percentile_cpis <- percentile_matrix[i, ]
          valid_percentiles <- !is.na(percentile_cpis)
          
          if (sum(valid_percentiles) >= 2) {
            # Use approx to find percentile, but with pre-computed values
            percentile_val <- approx(percentile_cpis[valid_percentiles], 
                                   percentile_names[valid_percentiles], 
                                   xout = cpi_val, rule = 2)$y
            return(pmax(0, pmin(100, percentile_val)))
          } else {
            return(NA)
          }
        })
        
        # Assign percentiles back to main data
        contention_data$cpi_percentile[proc_data_idx] <- percentile_values
      }
    }
  }
  
  # Remove points where percentile calculation failed
  contention_data <- contention_data %>%
    filter(!is.na(cpi_percentile))
  
  # Sample the data for visualization
  sampled_data <- contention_data %>%
    group_by(process_name) %>%
    sample_frac(sample_rate) %>%
    ungroup()
  
  message("Contention analysis data prepared:")
  message("  Total data points: ", nrow(contention_data))
  message("  Sampled points (", sample_rate*100, "%): ", nrow(sampled_data))
  
  # Report CPI percentile distribution
  percentile_summary <- sampled_data %>%
    group_by(process_name) %>%
    summarise(
      min_percentile = min(cpi_percentile, na.rm = TRUE),
      max_percentile = max(cpi_percentile, na.rm = TRUE),
      median_percentile = median(cpi_percentile, na.rm = TRUE),
      .groups = 'drop'
    )
  
  message("CPI percentile ranges by process:")
  for (i in 1:nrow(percentile_summary)) {
    process <- percentile_summary$process_name[i]
    message("  ", process, ": ", 
            round(percentile_summary$min_percentile[i], 1), "-", 
            round(percentile_summary$max_percentile[i], 1), 
            "% (median: ", round(percentile_summary$median_percentile[i], 1), "%)")
  }
  
  return(sampled_data)
}

# Function to create contention plots
create_contention_plots <- function(contention_data, window_duration_sec, output_file) {
  
  # Create instructions contention plot
  instructions_plot <- ggplot(contention_data, aes(x = instructions, y = other_instructions, color = cpi_percentile)) +
    geom_point(alpha = 0.2, size = 1.0) +
    scale_x_log10(labels = function(x) format(x, scientific = TRUE, digits = 2)) +
    scale_y_log10(labels = function(x) format(x, scientific = TRUE, digits = 2)) +
    scale_color_viridis_c(name = "CPI\nPercentile", 
                         option = "plasma",
                         trans = "identity",
                         breaks = c(1, 25, 50, 75, 99),
                         labels = c("1st", "25th", "50th", "75th", "99th")) +
    facet_wrap(~ process_name, scales = "free", ncol = 3) +
    labs(
      title = paste0("Process Instructions vs Other Processes' Instructions: Last ", window_duration_sec, " Seconds"),
      subtitle = paste0("Color shows CPI percentile within instruction bins (", nrow(contention_data), " sampled points)"),
      x = "Process Instructions (log scale)",
      y = "Other Processes' Total Instructions (log scale)"
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
      legend.position = "right"
    )
  
  # Create cache misses contention plot
  cache_plot <- ggplot(contention_data, aes(x = instructions, y = other_cache_misses, color = cpi_percentile)) +
    geom_point(alpha = 0.2, size = 1.0) +
    scale_x_log10(labels = function(x) format(x, scientific = TRUE, digits = 2)) +
    scale_y_log10(labels = function(x) format(x, scientific = TRUE, digits = 2)) +
    scale_color_viridis_c(name = "CPI\nPercentile", 
                         option = "plasma",
                         trans = "identity",
                         breaks = c(1, 25, 50, 75, 99),
                         labels = c("1st", "25th", "50th", "75th", "99th")) +
    facet_wrap(~ process_name, scales = "free", ncol = 3) +
    labs(
      title = paste0("Process Instructions vs Other Processes' Cache Misses: Last ", window_duration_sec, " Seconds"),
      subtitle = paste0("Color shows CPI percentile within instruction bins (", nrow(contention_data), " sampled points)"),
      x = "Process Instructions (log scale)",
      y = "Other Processes' Total Cache Misses (log scale)"
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
      legend.position = "right"
    )
  
  # Save plots
  instructions_pdf <- paste0(output_file, "_instructions.pdf")
  cache_pdf <- paste0(output_file, "_cache_misses.pdf")
  
  message("Saving instructions contention plot as PDF: ", instructions_pdf)
  ggsave(instructions_pdf, instructions_plot, width = 16, height = 12)
  
  message("Saving cache misses contention plot as PDF: ", cache_pdf)
  ggsave(cache_pdf, cache_plot, width = 16, height = 12)
  
  return(list(instructions_plot = instructions_plot, cache_plot = cache_plot))
}

# Main execution
main <- function() {
  tryCatch({
    # Check if input file exists
    if (!file.exists(input_file)) {
      stop("Input file does not exist: ", input_file)
    }
    
    message("Processing contention analysis...")
    window_data <- load_and_process_parquet(input_file, window_duration)
    
    # Check if we have enough data
    if (nrow(window_data) < 1000) {
      stop("Not enough data points in the selected time window. Found ", nrow(window_data), " points.")
    }
    
    message("Preparing contention analysis data...")
    contention_data <- prepare_contention_data(window_data, top_n_processes, sample_rate)
    
    if (nrow(contention_data) < 50) {
      stop("Insufficient data after filtering and sampling. Found ", nrow(contention_data), " points.")
    }
    
    message("Creating contention analysis plots...")
    plots <- create_contention_plots(contention_data, window_duration, output_file)
    
    message("Contention analysis complete!")
  }, error = function(e) {
    message("Error: ", e$message)
    quit(status = 1)
  })
}

# Execute main function
main() 