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
if (!requireNamespace("tidyr", quietly = TRUE)) {
  install.packages("tidyr", repos = "https://cloud.r-project.org/")
}
if (!requireNamespace("purrr", quietly = TRUE)) {
  install.packages("purrr", repos = "https://cloud.r-project.org/")
}

library(nanoparquet)
library(ggplot2)
library(dplyr)
library(tidyr)
library(purrr)

# Parse command line arguments
args <- commandArgs(trailingOnly = TRUE)
input_file <- if(length(args) >= 1) args[1] else "collector-parquet.parquet"
window_duration <- if(length(args) >= 2) as.numeric(args[2]) else 20  # Default to 20 seconds
output_file <- if(length(args) >= 3) args[3] else "instructions_vs_cpi"
top_n_processes <- if(length(args) >= 4) as.numeric(args[4]) else 15  # Default to showing top 15 processes
end_time <- if(length(args) >= 5) as.numeric(args[5]) else NULL  # Optional end time in seconds

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

# Function to prepare plot data
prepare_plot_data <- function(data, n_top_processes = top_n_processes) {
  # Select top processes by total instruction count
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
  
  # Filter data for top processes
  plot_data <- data %>%
    filter(process_name %in% top_processes$process_name)
  
  # Set factor levels for consistent ordering
  plot_data$process_name <- factor(plot_data$process_name, 
                                   levels = top_processes$process_name)
  
  # Generate colors for the processes
  n_colors <- length(unique(plot_data$process_name))
  colors <- rainbow(n_colors, start = 0, end = 0.8)  # Avoid red-pink range for better visibility
  names(colors) <- levels(plot_data$process_name)
  
  return(list(
    plot_data = plot_data,
    colors = colors,
    top_processes = top_processes
  ))
}

# Function to create the instructions vs CPI faceted scatter plot with percentiles
create_instructions_cpi_plot <- function(plot_data_list, window_duration_sec) {
  plot_data <- plot_data_list$plot_data
  
  # Install quantreg if not available for quantile regression
  if (!requireNamespace("quantreg", quietly = TRUE)) {
    install.packages("quantreg", repos = "https://cloud.r-project.org/")
  }
  library(quantreg)
  
  # Calculate summary statistics for each process
  process_stats <- plot_data %>%
    group_by(process_name) %>%
    summarise(
      instruction_range = paste0(format(min(instructions), scientific = TRUE, digits = 2), 
                                 " - ", format(max(instructions), scientific = TRUE, digits = 2)),
      cpi_range = paste0(round(min(cpi), 3), " - ", round(max(cpi), 3)),
      median_cpi = median(cpi, na.rm = TRUE),
      sample_count = n(),
      .groups = 'drop'
    )
  
  message("Process CPI summary:")
  for (i in 1:nrow(process_stats)) {
    process <- process_stats$process_name[i]
    message("  ", process, ": CPI range [", process_stats$cpi_range[i], 
            "], median = ", round(process_stats$median_cpi[i], 3),
            ", samples = ", process_stats$sample_count[i])
  }
  
  
  # Define percentiles to plot
  percentiles <- c(0.05, seq(0.1, 0.9, by = 0.1), 0.95)
  
  # Calculate smooth percentile curves using a simpler approach
  percentile_long <- data.frame()
  
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
            inst_seq = inst_seq,
            cpi_value = smoothed_values,
            percentile = p * 100
          )
          percentile_long <- rbind(percentile_long, proc_percentile)
        }
      }
         }
   }
   
   # Identify and sample outlier points based on smooth percentile curves
   sampled_points <- data.frame()
   
   for (proc in unique(plot_data$process_name)) {
     proc_data <- plot_data[plot_data$process_name == proc, ]
     
     if (nrow(proc_data) > 20) {
       # Get the 5th and 95th percentile curves for this process
       proc_5th <- percentile_long[percentile_long$process_name == proc & percentile_long$percentile == 5, ]
       proc_95th <- percentile_long[percentile_long$process_name == proc & percentile_long$percentile == 95, ]
       
       if (nrow(proc_5th) > 0 && nrow(proc_95th) > 0) {
         # Interpolate percentile values for each data point's instruction count
         log_instructions <- log10(proc_data$instructions)
         
         # Interpolate 5th and 95th percentile values
         interp_5th <- approx(log10(proc_5th$inst_seq), proc_5th$cpi_value, 
                             xout = log_instructions, rule = 2)$y
         interp_95th <- approx(log10(proc_95th$inst_seq), proc_95th$cpi_value, 
                              xout = log_instructions, rule = 2)$y
         
         # Classify points
         proc_data$outlier_type <- "Normal"
         proc_data$outlier_type[proc_data$cpi >= interp_95th] <- "Above 95th percentile"
         proc_data$outlier_type[proc_data$cpi <= interp_5th] <- "Below 5th percentile"
         
         # Sample outliers (10% of outliers)
         outliers <- proc_data[proc_data$outlier_type != "Normal", ]
         if (nrow(outliers) > 0) {
           sampled_outliers <- outliers[sample(nrow(outliers), size = max(1, round(nrow(outliers) * 0.1))), ]
           sampled_points <- rbind(sampled_points, sampled_outliers)
         }
         
         # Sample normal points (1% of normal points)
         normal_points <- proc_data[proc_data$outlier_type == "Normal", ]
         if (nrow(normal_points) > 0) {
           sampled_normal <- normal_points[sample(nrow(normal_points), size = max(1, round(nrow(normal_points) * 0.01))), ]
           sampled_normal$outlier_type <- "Sample (1%)"
           sampled_points <- rbind(sampled_points, sampled_normal)
         }
       }
     }
   }
   
   # Report sampling statistics
   if (nrow(sampled_points) > 0) {
     sampling_summary <- table(sampled_points$outlier_type)
     total_points <- nrow(plot_data)
     total_sampled <- nrow(sampled_points)
     message("Point sampling: plotting ", total_sampled, " points out of ", total_points, 
             " total (", round(total_sampled/total_points*100, 2), "%)")
     for (i in 1:length(sampling_summary)) {
       message("  ", names(sampling_summary)[i], ": ", sampling_summary[i], " points")
     }
   }
   
   # Create the faceted scatter plot with smooth percentile lines
   p <- ggplot(plot_data, aes(x = instructions, y = cpi)) +
     # Add smooth percentile lines
     geom_line(data = percentile_long, 
               aes(x = inst_seq, y = cpi_value, group = percentile),
               alpha = 0.7, size = 0.6, color = "#1E5F8F") +
     # Plot sampled points
     geom_point(data = sampled_points, 
                aes(color = outlier_type), 
                alpha = 0.7, size = 1.2) +
    scale_x_log10(labels = function(x) format(x, scientific = TRUE, digits = 2)) +
         scale_color_manual(values = c(
       "Above 95th percentile" = "#E74C3C", 
       "Below 5th percentile" = "#27AE60",
       "Sample (1%)" = "#95A5A6")) +
    facet_wrap(~ process_name, scales = "free", ncol = 3) +
    labs(
      title = paste0("Instructions vs CPI with Percentiles: Last ", window_duration_sec, " Seconds"),
      subtitle = paste0("Percentile lines (5th, 10th-90th, 95th) with outlier points only (top ", 
                       length(unique(plot_data$process_name)), " processes)"),
      x = "Instructions (log scale)",
      y = "Cycles Per Instruction (CPI)",
      color = "Outliers"
    ) +
    theme_minimal() +
    theme(
      panel.grid.minor = element_blank(),
      plot.title = element_text(face = "bold", size = 16),
      plot.subtitle = element_text(size = 12),
      axis.title = element_text(face = "bold", size = 12),
      axis.text = element_text(size = 9),
      axis.text.x = element_text(angle = 45, hjust = 1),
      strip.text = element_text(face = "bold", size = 10),
      panel.spacing = unit(0.5, "lines"),
      legend.position = "bottom"
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
    
    message("Processing instructions vs CPI analysis...")
    window_data <- load_and_process_parquet(input_file, window_duration, end_time)
    
    # Check if we have enough data
    if (nrow(window_data) < 100) {
      stop("Not enough data points in the selected time window. Found ", nrow(window_data), " points.")
    }
    
    message("Preparing plot data...")
    plot_data_list <- prepare_plot_data(window_data, top_n_processes)
    
    message("Creating instructions vs CPI scatter plot...")
    scatter_plot <- create_instructions_cpi_plot(plot_data_list, window_duration)
    
    # Save the plot
    png_filename <- paste0(output_file, ".png")
    pdf_filename <- paste0(output_file, ".pdf")
    
    # message("Saving scatter plot as PNG: ", png_filename)
    # ggsave(png_filename, scatter_plot, width = 16, height = 12, dpi = 300)
    
    message("Saving scatter plot as PDF: ", pdf_filename)
    ggsave(pdf_filename, scatter_plot, width = 16, height = 12)
    
    message("Analysis complete!")
  }, error = function(e) {
    message("Error: ", e$message)
    quit(status = 1)
  })
}

# Execute main function
main() 