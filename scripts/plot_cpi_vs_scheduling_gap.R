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
if (!requireNamespace("gridExtra", quietly = TRUE)) {
  install.packages("gridExtra", repos = "https://cloud.r-project.org/")
}
if (!requireNamespace("cowplot", quietly = TRUE)) {
  install.packages("cowplot", repos = "https://cloud.r-project.org/")
}

library(nanoparquet)
library(ggplot2)
library(dplyr)
library(tidyr)
library(gridExtra)
library(cowplot)

# Parse command line arguments
args <- commandArgs(trailingOnly = TRUE)
input_file <- if(length(args) >= 1) args[1] else "collector-parquet.parquet"
window_duration <- if(length(args) >= 2) as.numeric(args[2]) else 20  # Default to 20 seconds
output_file <- if(length(args) >= 3) args[3] else "cpi_vs_scheduling_gap"
top_n_processes <- if(length(args) >= 4) as.numeric(args[4]) else 15  # Default to showing top 15 processes
end_time <- if(length(args) >= 5) as.numeric(args[5]) else NULL  # Optional end time in seconds

# Constants
NS_PER_SEC <- 1e9
MS_PER_SEC <- 1000

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
      cpi = ifelse(instructions > 0, cycles / instructions, NA),
      # Convert to milliseconds for easier scheduling gap calculation
      start_time_ms = start_time / 1e6
    ) %>%
    filter(
      !is.na(cpi),
      cpi > 0,
      instructions > 0
    )
  
  return(window_data)
}

# Function to calculate scheduling gaps
calculate_scheduling_gaps <- function(data) {
  message("Calculating scheduling gaps...")
  
  # Sort by process and time
  data_sorted <- data %>%
    arrange(process_name, pid, start_time_ms)
  
  # Calculate gaps for each process
  gap_data <- data_sorted %>%
    group_by(process_name, pid) %>%
    mutate(
      # Calculate time since previous sample for this process
      prev_time = lag(start_time_ms),
      scheduling_gap_ms = ifelse(is.na(prev_time), 0, start_time_ms - prev_time),
      # Cap scheduling gaps at 10ms to avoid extreme outliers
      scheduling_gap_ms = pmin(scheduling_gap_ms, 20)
    ) %>%
    ungroup() %>%
    filter(!is.na(scheduling_gap_ms))
  
  message("Gap calculation complete. Found ", nrow(gap_data), " samples with gap information.")
  
  # Report gap distribution
  gap_summary <- gap_data %>%
    summarise(
      zero_gap = sum(scheduling_gap_ms == 0),
      short_gap = sum(scheduling_gap_ms > 0 & scheduling_gap_ms <= 2),
      medium_gap = sum(scheduling_gap_ms > 2 & scheduling_gap_ms <= 5),
      long_gap = sum(scheduling_gap_ms > 5)
    )
  
  message("Gap distribution:")
  message("  Zero gap (0ms): ", gap_summary$zero_gap, " samples")
  message("  Short gap (0-2ms): ", gap_summary$short_gap, " samples")
  message("  Medium gap (2-5ms): ", gap_summary$medium_gap, " samples")
  message("  Long gap (>5ms): ", gap_summary$long_gap, " samples")
  
  return(gap_data)
}

# Function to prepare plot data
prepare_plot_data <- function(data, n_top_processes = top_n_processes) {
  # Select top processes by total instruction count
  top_processes <- data %>%
    group_by(process_name) %>%
    summarise(
      total_instructions = sum(instructions, na.rm = TRUE),
      sample_count = n()
    ) %>%
    arrange(desc(total_instructions)) %>%
    slice_head(n = n_top_processes)
  
  message("Top ", n_top_processes, " processes by total instruction count:")
  for (i in 1:nrow(top_processes)) {
    process <- top_processes$process_name[i]
    instructions <- top_processes$total_instructions[i]
    samples <- top_processes$sample_count[i]
    message("  ", i, ". ", process, ": ", 
            format(instructions, scientific = TRUE, digits = 3), " instructions (", samples, " samples)")
  }
  
  # Filter data for top processes
  plot_data <- data %>%
    filter(process_name %in% top_processes$process_name)
  
  # Set factor levels for consistent ordering
  plot_data$process_name <- factor(plot_data$process_name, 
                                   levels = top_processes$process_name)
  
  return(plot_data)
}


# Function to create combined scatter plots with bar charts
create_combined_plots <- function(plot_data, window_duration_sec) {
  message("Creating combined scatter plots with bar charts...")
  
  # Install quantreg if not available for quantile regression
  if (!requireNamespace("quantreg", quietly = TRUE)) {
    install.packages("quantreg", repos = "https://cloud.r-project.org/")
  }
  library(quantreg)
  
  # Define percentiles to plot
  percentiles <- c(0.05, seq(0.1, 0.9, by = 0.1), 0.95)
  
  # Calculate smooth percentile curves
  percentile_long <- data.frame()
  
  for (proc in unique(plot_data$process_name)) {
    proc_data <- plot_data[plot_data$process_name == proc, ]
    
    if (nrow(proc_data) > 20) {  # Need minimum data for percentiles
      # Create gap sequence for smooth curves
      gap_range <- range(proc_data$scheduling_gap_ms)
      gap_seq <- seq(gap_range[1], gap_range[2], length.out = 50)
      
      # Calculate percentiles for each gap level
      for (p in percentiles) {
        percentile_values <- rep(NA, length(gap_seq))
        
        for (i in 1:length(gap_seq)) {
          # Find nearby points (within a window)
          window_size <- diff(gap_range) / 20  # Adaptive window size
          nearby_idx <- abs(proc_data$scheduling_gap_ms - gap_seq[i]) <= window_size
          
          if (sum(nearby_idx) >= 5) {  # Need at least 5 points for percentile
            percentile_values[i] <- quantile(proc_data$cpi[nearby_idx], p, na.rm = TRUE)
          }
        }
        
        # Remove NAs and smooth the percentile curve
        valid_idx <- !is.na(percentile_values)
        if (sum(valid_idx) >= 3) {
          # Use loess smoothing for the percentile curve
          smooth_fit <- loess(percentile_values[valid_idx] ~ gap_seq[valid_idx], span = 0.5)
          smoothed_values <- predict(smooth_fit, gap_seq)
          
          # Add to combined data
          proc_percentile <- data.frame(
            process_name = proc,
            gap_seq = gap_seq,
            cpi_value = smoothed_values,
            percentile = p * 100
          )
          percentile_long <- rbind(percentile_long, proc_percentile)
        }
      }
    }
  }
  
  # Sample data for plotting points
  sampled_points <- data.frame()
  
  for (proc in unique(plot_data$process_name)) {
    proc_data <- plot_data[plot_data$process_name == proc, ]
    
    if (nrow(proc_data) > 20) {
      # Get the 5th and 95th percentile curves for this process
      proc_5th <- percentile_long[percentile_long$process_name == proc & percentile_long$percentile == 5, ]
      proc_95th <- percentile_long[percentile_long$process_name == proc & percentile_long$percentile == 95, ]
      
      if (nrow(proc_5th) > 0 && nrow(proc_95th) > 0) {
        # Interpolate percentile values for each data point's gap
        gaps <- proc_data$scheduling_gap_ms
        
        # Interpolate 5th and 95th percentile values
        interp_5th <- approx(proc_5th$gap_seq, proc_5th$cpi_value, 
                            xout = gaps, rule = 2)$y
        interp_95th <- approx(proc_95th$gap_seq, proc_95th$cpi_value, 
                             xout = gaps, rule = 2)$y
        
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
  
  # Create histogram data for bar charts
  histogram_data <- plot_data %>%
    mutate(gap_bin = round(scheduling_gap_ms * 2) / 2) %>%  # 0.5ms bins
    group_by(process_name, gap_bin) %>%
    summarise(count = n(), .groups = 'drop')
  
  # Create individual paired plots for each process
  processes <- unique(plot_data$process_name)
  process_plots <- list()
  
  for (proc in processes) {
    message("Processing: ", proc)
    
    # Filter data for this process
    proc_data <- plot_data[plot_data$process_name == proc, ]
    proc_percentiles <- percentile_long[percentile_long$process_name == proc, ]
    proc_sampled <- sampled_points[sampled_points$process_name == proc, ]
    proc_histogram <- histogram_data[histogram_data$process_name == proc, ]
    
    message("  Data rows: ", nrow(proc_data), ", Percentiles: ", nrow(proc_percentiles), 
            ", Sampled: ", nrow(proc_sampled), ", Histogram: ", nrow(proc_histogram))
    
    # Skip if no data for this process
    if (nrow(proc_data) == 0) {
      message("  Skipping - no data")
      next
    }
    
    # Debug data ranges
    message("  CPI range: ", min(proc_data$cpi, na.rm = TRUE), " to ", max(proc_data$cpi, na.rm = TRUE))
    message("  Gap range: ", min(proc_data$scheduling_gap_ms, na.rm = TRUE), " to ", max(proc_data$scheduling_gap_ms, na.rm = TRUE))
    
    tryCatch({
      # Create basic scatter plot first
      message("  Creating basic scatter plot...")
      scatter_plot <- ggplot(proc_data, aes(x = scheduling_gap_ms, y = cpi)) +
        scale_x_continuous(breaks = seq(0, 20, by = 5)) +
        labs(
          title = proc,
          x = NULL,
          y = "CPI"
        ) +
        theme_minimal() +
        theme(
          panel.grid.minor = element_blank(),
          plot.title = element_text(face = "bold", size = 12),
          axis.title = element_text(face = "bold", size = 10),
          axis.text = element_text(size = 9),
          axis.text.x = element_blank(),
          legend.position = "none"
        )
      
      # Add percentile lines if available
      if (nrow(proc_percentiles) > 0) {
        message("  Adding percentile lines...")
        scatter_plot <- scatter_plot +
          geom_line(data = proc_percentiles, 
                    aes(x = gap_seq, y = cpi_value, group = percentile),
                    alpha = 0.7, linewidth = 0.6, color = "#1E5F8F")
      }
      
      # Add sampled points if available
      if (nrow(proc_sampled) > 0) {
        message("  Adding sampled points...")
        # Check if outlier_type column exists and has valid values
        if ("outlier_type" %in% names(proc_sampled)) {
          unique_types <- unique(proc_sampled$outlier_type)
          message("    Outlier types: ", paste(unique_types, collapse = ", "))
          
          scatter_plot <- scatter_plot +
            geom_point(data = proc_sampled, 
                       aes(color = outlier_type), 
                       alpha = 0.7, size = 1.2)
          
          # Only add color scale if we have the expected types
          if (any(c("Above 95th percentile", "Below 5th percentile", "Sample (1%)") %in% unique_types)) {
            scatter_plot <- scatter_plot +
              scale_color_manual(values = c(
                "Above 95th percentile" = "#E74C3C", 
                "Below 5th percentile" = "#27AE60",
                "Sample (1%)" = "#95A5A6"))
          }
        } else {
          # Fallback: add points without color mapping
          scatter_plot <- scatter_plot +
            geom_point(data = proc_sampled, alpha = 0.7, size = 1.2, color = "#95A5A6")
        }
      } else {
        message("  No sampled points available")
      }
      
      message("  Scatter plot created successfully")
      
    }, error = function(e) {
      message("  ERROR in scatter plot creation: ", e$message)
      # Create minimal scatter plot as fallback
      scatter_plot <- ggplot(proc_data, aes(x = scheduling_gap_ms, y = cpi)) +
        geom_point(alpha = 0.5, size = 1) +
        labs(title = proc, x = NULL, y = "CPI") +
        theme_minimal()
    })
    
    tryCatch({
      # Create bar chart for this process
      if (nrow(proc_histogram) > 0) {
        message("  Creating bar chart...")
        bar_chart <- ggplot(proc_histogram, aes(x = gap_bin, y = count)) +
          geom_col(fill = "#2E86AB", alpha = 0.7, width = 0.4) +
          scale_x_continuous(breaks = seq(0, 20, by = 5)) +
          scale_y_continuous(labels = function(x) ifelse(x >= 1000, paste0(round(x/1000, 1), "K"), x)) +
          labs(
            x = "Gap (ms)",
            y = "Count"
          ) +
          theme_minimal() +
          theme(
            panel.grid.minor = element_blank(),
            axis.title = element_text(face = "bold", size = 10),
            axis.text = element_text(size = 8)
          )
      } else {
        message("  Creating empty bar chart...")
        bar_chart <- ggplot() +
          labs(x = "Gap (ms)", y = "Count") +
          theme_minimal() +
          theme(
            panel.grid.minor = element_blank(),
            axis.title = element_text(face = "bold", size = 10),
            axis.text = element_text(size = 8)
          )
      }
      
      message("  Bar chart created successfully")
      
    }, error = function(e) {
      message("  ERROR in bar chart creation: ", e$message)
      # Create minimal bar chart as fallback
      bar_chart <- ggplot() +
        labs(x = "Gap (ms)", y = "Count") +
        theme_minimal()
    })
    
    tryCatch({
      # Combine scatter and bar for this process using cowplot
      message("  Combining plots...")
      combined_proc <- plot_grid(scatter_plot, bar_chart, ncol = 1, 
                                rel_heights = c(0.7, 0.3), align = "v")
      process_plots[[proc]] <- combined_proc
      message("  Combined plot created successfully")
      
    }, error = function(e) {
      message("  ERROR in plot combination: ", e$message)
      # Just use scatter plot as fallback
      process_plots[[proc]] <- scatter_plot
    })
  }
  
  message("Creating final plot arrangement...")
  
  tryCatch({
    # Create overall title
    message("  Creating title...")
    title <- ggdraw() + 
      draw_label(
        paste0("CPI vs Scheduling Gap with Percentiles: Last ", window_duration_sec, " Seconds\n",
               "Percentile lines (5th, 10th-90th, 95th) with sample counts below (top ", 
               length(processes), " processes)"),
        fontface = 'bold',
        size = 14
      )
    message("  Title created successfully")
    
  }, error = function(e) {
    message("  ERROR creating title: ", e$message)
    title <- ggdraw() + draw_label("CPI vs Scheduling Gap Analysis", fontface = 'bold', size = 14)
  })
  
  tryCatch({
    # Create legend
    message("  Creating legend...")
    if (nrow(sampled_points) > 0 && "outlier_type" %in% names(sampled_points)) {
      legend_plot <- ggplot(sampled_points, aes(color = outlier_type)) +
        geom_point(alpha = 0) +  # Invisible points just for legend
        scale_color_manual(values = c(
          "Above 95th percentile" = "#E74C3C", 
          "Below 5th percentile" = "#27AE60",
          "Sample (1%)" = "#95A5A6")) +
        labs(color = "Outliers") +
        theme_void() +
        theme(
          legend.position = "bottom",
          legend.title = element_text(face = "bold", size = 10),
          legend.text = element_text(size = 9)
        )
      
      legend <- get_legend(legend_plot)
      message("  Legend created successfully")
    } else {
      message("  No legend data available, creating empty legend")
      legend <- ggdraw()
    }
    
  }, error = function(e) {
    message("  ERROR creating legend: ", e$message)
    legend <- ggdraw()
  })
  
  tryCatch({
    # Determine grid layout
    message("  Arranging process plots...")
    n_processes <- length(process_plots)
    message("  Number of process plots: ", n_processes)
    
    if (n_processes == 0) {
      message("  No process plots to arrange!")
      return(ggdraw() + draw_label("No data to plot", size = 16))
    }
    
    ncol_grid <- min(3, n_processes)  # Max 3 columns
    nrow_grid <- ceiling(n_processes / ncol_grid)
    
    # Arrange process plots in grid
    if (n_processes == 1) {
      process_grid <- process_plots[[1]]
    } else {
      process_grid <- plot_grid(plotlist = process_plots, ncol = ncol_grid)
    }
    message("  Process grid created successfully")
    
  }, error = function(e) {
    message("  ERROR arranging process plots: ", e$message)
    # Fallback: just use the first plot
    if (length(process_plots) > 0) {
      process_grid <- process_plots[[1]]
    } else {
      process_grid <- ggdraw() + draw_label("No plots available", size = 16)
    }
  })
  
  tryCatch({
    # Combine title, process grid, and legend
    message("  Combining final plot...")
    final_plot <- plot_grid(
      title,
      process_grid,
      legend,
      ncol = 1,
      rel_heights = c(0.1, 0.85, 0.05)
    )
    message("  Final plot created successfully")
    
  }, error = function(e) {
    message("  ERROR combining final plot: ", e$message)
    # Fallback: just return the process grid
    final_plot <- process_grid
  })
  
  return(final_plot)
}

# Function to create summary statistics
create_summary_stats <- function(plot_data) {
  message("Calculating summary statistics...")
  
  # Overall correlation
  overall_corr <- cor(plot_data$scheduling_gap_ms, plot_data$cpi, use = "complete.obs")
  message("Overall correlation between scheduling gap and CPI: ", round(overall_corr, 4))
  
  # Gap category analysis
  gap_analysis <- plot_data %>%
    mutate(
      gap_category = case_when(
        scheduling_gap_ms == 0 ~ "Zero gap",
        scheduling_gap_ms <= 2 ~ "Short gap (0-2ms)",
        scheduling_gap_ms <= 5 ~ "Medium gap (2-5ms)",
        TRUE ~ "Long gap (>5ms)"
      )
    ) %>%
    group_by(gap_category) %>%
    summarise(
      count = n(),
      mean_cpi = mean(cpi, na.rm = TRUE),
      median_cpi = median(cpi, na.rm = TRUE),
      sd_cpi = sd(cpi, na.rm = TRUE),
      q25_cpi = quantile(cpi, 0.25, na.rm = TRUE),
      q75_cpi = quantile(cpi, 0.75, na.rm = TRUE)
    )
  
  message("CPI statistics by scheduling gap category:")
  for (i in 1:nrow(gap_analysis)) {
    cat <- gap_analysis$gap_category[i]
    count <- gap_analysis$count[i]
    mean_cpi <- round(gap_analysis$mean_cpi[i], 3)
    median_cpi <- round(gap_analysis$median_cpi[i], 3)
    sd_cpi <- round(gap_analysis$sd_cpi[i], 3)
    message("  ", cat, ": n=", count, ", mean CPI=", mean_cpi, 
            ", median=", median_cpi, ", sd=", sd_cpi)
  }
  
  return(gap_analysis)
}

# Main execution
main <- function() {
  tryCatch({
    # Check if input file exists
    if (!file.exists(input_file)) {
      stop("Input file does not exist: ", input_file)
    }
    
    message("Processing CPI vs scheduling gap analysis...")
    window_data <- load_and_process_parquet(input_file, window_duration, end_time)
    
    # Check if we have enough data
    if (nrow(window_data) < 100) {
      stop("Not enough data points in the selected time window. Found ", nrow(window_data), " points.")
    }
    
    message("Calculating scheduling gaps...")
    gap_data <- calculate_scheduling_gaps(window_data)
    
    message("Preparing plot data...")
    plot_data <- prepare_plot_data(gap_data, top_n_processes)
    
    message("Creating summary statistics...")
    summary_stats <- create_summary_stats(plot_data)
    
    message("Creating combined plots...")
    combined_plot <- create_combined_plots(plot_data, window_duration)
    
    # Save the plot
    output_pdf <- paste0(output_file, ".pdf")
    
    message("Saving combined plot as PDF: ", output_pdf)
    ggsave(output_pdf, combined_plot, width = 16, height = 18)
    
    message("Analysis complete!")
  }, error = function(e) {
    message("Error: ", e$message)
    quit(status = 1)
  })
}

# Execute main function
main()