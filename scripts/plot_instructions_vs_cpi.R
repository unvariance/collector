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

library(nanoparquet)
library(ggplot2)
library(dplyr)

# Parse command line arguments
args <- commandArgs(trailingOnly = TRUE)
input_file <- if(length(args) >= 1) args[1] else "collector-parquet.parquet"
window_duration <- if(length(args) >= 2) as.numeric(args[2]) else 20  # Default to 20 seconds
output_file <- if(length(args) >= 3) args[3] else "instructions_vs_cpi"
top_n_processes <- if(length(args) >= 4) as.numeric(args[4]) else 15  # Default to showing top 15 processes

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
  
  # Calculate relative time in nanoseconds
  perf_data$relative_time_ns <- perf_data$start_time - min_time
  
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

# Function to create the instructions vs CPI faceted scatter plot
create_instructions_cpi_plot <- function(plot_data_list, window_duration_sec) {
  plot_data <- plot_data_list$plot_data
  
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
  
  # Create the faceted scatter plot
  p <- ggplot(plot_data, aes(x = instructions, y = cpi)) +
    geom_point(alpha = 0.5, size = 1.2, color = "#2E8B57") +  # Sea green points
    geom_smooth(method = "loess", se = TRUE, alpha = 0.3, color = "#1E5F8F", fill = "#87CEEB") +  # Blue trend line with light blue confidence interval
    scale_x_log10(labels = function(x) format(x, scientific = TRUE, digits = 2)) +
    facet_wrap(~ process_name, scales = "free", ncol = 3) +  # 3 columns for better layout
    labs(
      title = paste0("Instructions vs CPI: Last ", window_duration_sec, " Seconds"),
      subtitle = paste0("Relationship between instruction count and cycles per instruction (top ", 
                       length(unique(plot_data$process_name)), " processes)"),
      x = "Instructions (log scale)",
      y = "Cycles Per Instruction (CPI)"
    ) +
    theme_minimal() +
    theme(
      panel.grid.minor = element_blank(),
      plot.title = element_text(face = "bold", size = 16),
      plot.subtitle = element_text(size = 12),
      axis.title = element_text(face = "bold", size = 12),
      axis.text = element_text(size = 9),
      axis.text.x = element_text(angle = 45, hjust = 1),  # Rotate x-axis labels for better fit
      strip.text = element_text(face = "bold", size = 10),
      panel.spacing = unit(0.5, "lines")
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
    window_data <- load_and_process_parquet(input_file, window_duration)
    
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