#!/usr/bin/env Rscript

# Concurrency CPI Analysis - Faceted plots for top 20 processes
# This script creates combined plots showing:
# 1. Heat map of CPI distribution vs concurrency (top 2/3)
# 2. Bar chart of instruction counts vs concurrency (bottom 1/3)
# Works with CSV files from trace-analysis concurrency analysis

library(dplyr)
library(ggplot2)
library(tidyr)
library(cowplot)
library(stringr)

# Command line argument parsing
args <- commandArgs(trailingOnly = TRUE)
if (length(args) != 1) {
  cat("Usage: Rscript concurrency_cpi_analysis.R <concurrency_csv_file>\n")
  cat("Input should be the CSV file from trace-analysis concurrency analysis\n")
  quit(status = 1)
}

input_file <- args[1]
output_file <- gsub("\\.csv$", "_plot.pdf", input_file)

cat("Reading CSV file:", input_file, "\n")

# Read the CSV data with concurrency analysis
df <- read.csv(input_file, stringsAsFactors = FALSE)

cat("Loaded", nrow(df), "rows\n")

# Calculate midpoint values for concurrency and CPI bins
df <- df %>%
  mutate(
    concurrency_midpoint = (concurrency_min + concurrency_max) / 2,
    cpi_midpoint = (cpi_min + cpi_max) / 2,
    concurrency_width = concurrency_max - concurrency_min,
    cpi_width = cpi_max - cpi_min
  ) %>%
  filter(instructions > 0, concurrency_midpoint >= 1.0)  # Remove concurrency < 1 and zero instructions

cat("After processing:", nrow(df), "rows\n")

# Find top 20 processes by total instructions
top_processes <- df %>%
  group_by(process_name) %>%
  summarise(total_instructions = sum(instructions), .groups = 'drop') %>%
  arrange(desc(total_instructions)) %>%
  head(20) %>%
  pull(process_name)

cat("Top 20 processes by instruction count:\n")
print(top_processes)

# Filter to top 20 processes
df_top <- df %>%
  filter(process_name %in% top_processes)

cat("Filtered to top 20 processes:", nrow(df_top), "rows\n")

# Function to create plots from CSV data
create_concurrency_plots <- function(data, plot_title) {
  cat("Creating plots for", plot_title, "\n")
  
  # Data is already binned, so we use the bin midpoints directly
  # Prepare data for instruction count bar chart
  instruction_data <- data %>%
    group_by(process_name, concurrency_midpoint) %>%
    summarise(
      total_instructions = sum(instructions),
      concurrency_width = first(concurrency_width),
      .groups = 'drop'
    )
  
  # Prepare data for CPI heat map
  # The data is already in the format we need (concurrency_midpoint, cpi_midpoint, instructions)
  heatmap_data <- data
  
  # Normalize by concurrency bin to get proportions
  heatmap_data <- heatmap_data %>%
    group_by(process_name, concurrency_midpoint) %>%
    mutate(
      total_instructions_bin = sum(instructions),
      proportion = instructions / total_instructions_bin
    ) %>%
    ungroup()
  
  # Create plots for each process
  process_plots <- list()
  
  for (proc in top_processes) {
    proc_instruction_data <- instruction_data %>%
      filter(process_name == proc)
    
    proc_heatmap_data <- heatmap_data %>%
      filter(process_name == proc)
    
    # Skip if no data
    if (nrow(proc_instruction_data) == 0 || nrow(proc_heatmap_data) == 0) {
      next
    }
    
    # Check if we should filter out extreme concurrency values for this process
    # Calculate instruction-weighted 99th percentile for concurrency values
    total_instructions_for_process <- sum(proc_instruction_data$total_instructions)
    cumulative_instructions <- 0
    
    # Sort by concurrency and calculate cumulative instruction percentage
    proc_sorted <- proc_instruction_data %>%
      arrange(concurrency_midpoint) %>%
      mutate(
        cumulative_instructions = cumsum(total_instructions),
        cumulative_pct = cumulative_instructions / total_instructions_for_process
      )
    
    # Find the 99th percentile by instruction count
    concurrency_99th_idx <- which(proc_sorted$cumulative_pct >= 0.99)[1]
    if (is.na(concurrency_99th_idx)) {
      concurrency_99th <- max(proc_sorted$concurrency_midpoint)
    } else {
      concurrency_99th <- proc_sorted$concurrency_midpoint[concurrency_99th_idx]
    }
    
    concurrency_max <- max(proc_instruction_data$concurrency_midpoint, na.rm = TRUE)
    concurrency_min <- min(proc_instruction_data$concurrency_midpoint, na.rm = TRUE)
    
    # Calculate axis ranges
    range_with_outliers <- concurrency_max - concurrency_min
    range_without_outliers <- concurrency_99th - concurrency_min
    
    # Check if removing top 1% (by instruction count) makes axis more than 1.5x smaller
    should_filter <- (range_with_outliers > 1.5 * range_without_outliers) && (range_without_outliers > 0)
    
    if (should_filter) {
      cat("  Filtering extreme concurrency values for", proc, 
          "(range reduced from", round(range_with_outliers, 2), 
          "to", round(range_without_outliers, 2), ")\n")
      
      # Filter both datasets to remove top 1%
      proc_instruction_data <- proc_instruction_data %>%
        filter(concurrency_midpoint <= concurrency_99th)
      
      proc_heatmap_data <- proc_heatmap_data %>%
        filter(concurrency_midpoint <= concurrency_99th)
      
      # Skip if no data left after filtering
      if (nrow(proc_instruction_data) == 0 || nrow(proc_heatmap_data) == 0) {
        next
      }
    }
    
    # Create instruction count bar chart
    bar_plot <- ggplot(proc_instruction_data, aes(x = concurrency_midpoint, y = total_instructions)) +
      geom_col(fill = "#2E86AB", alpha = 0.7, width = proc_instruction_data$concurrency_width[1]) +
      scale_y_continuous(labels = function(x) {
        ifelse(x >= 1e9, paste0(round(x/1e9, 1), "B"),
               ifelse(x >= 1e6, paste0(round(x/1e6, 1), "M"),
                      ifelse(x >= 1e3, paste0(round(x/1e3, 1), "K"), x)))
      }) +
      labs(
        x = "Average Concurrency",
        y = "Instructions"
      ) +
      theme_minimal() +
      theme(
        panel.grid.minor = element_blank(),
        axis.title = element_text(size = 8),
        axis.text = element_text(size = 7),
        plot.margin = margin(0, 5, 5, 5)
      )
    
    # Create CPI heat map
    heatmap_plot <- ggplot(proc_heatmap_data, aes(x = concurrency_midpoint, y = cpi_midpoint, fill = proportion)) +
      geom_tile(width = proc_heatmap_data$concurrency_width[1], height = proc_heatmap_data$cpi_width[1]) +
      scale_fill_viridis_c(name = "Proportion\nof Instructions", trans = "sqrt", na.value = "#2c2c54") +
      labs(
        title = proc,
        x = NULL,
        y = "CPI"
      ) +
      theme_minimal() +
      theme(
        panel.grid = element_blank(),
        panel.background = element_rect(fill = "#2c2c54", color = NA),  # Dark background for missing data
        plot.background = element_rect(fill = "white", color = NA),
        axis.title = element_text(size = 8),
        axis.text = element_text(size = 7),
        axis.text.x = element_blank(),
        plot.title = element_text(size = 10, face = "bold"),
        legend.position = "none",
        plot.margin = margin(5, 5, 0, 5)
      )
    
    # Combine heat map (top 2/3) and bar chart (bottom 1/3)
    combined_plot <- plot_grid(
      heatmap_plot, 
      bar_plot, 
      ncol = 1, 
      rel_heights = c(2, 1), 
      align = "v"
    )
    
    process_plots[[proc]] <- combined_plot
  }
  
  # Create overall plot with facets
  if (length(process_plots) == 0) {
    cat("No plots created for", plot_title, "\n")
    return(NULL)
  }
  
  # Arrange in grid (4 columns to fit 20 processes in 5 rows)
  final_plot <- plot_grid(plotlist = process_plots, ncol = 4, align = "hv")
  
  # Add overall title
  title <- ggdraw() + 
    draw_label(
      paste0(plot_title, " - Top 20 Processes by Instruction Count\n",
             "Heat map shows CPI distribution (normalized by concurrency), Bar chart shows instruction counts"),
      fontface = 'bold',
      size = 14
    )
  
  # Create legend from sample data
  legend_data <- heatmap_data %>%
    filter(process_name == top_processes[1]) %>%
    head(10)
  
  if (nrow(legend_data) > 0) {
    legend_plot <- ggplot(legend_data, aes(x = concurrency_midpoint, y = cpi_midpoint, fill = proportion)) +
      geom_tile() +
      scale_fill_viridis_c(name = "Proportion\nof Instructions", trans = "sqrt") +
      theme_void() +
      theme(
        legend.position = "bottom",
        legend.title = element_text(size = 10),
        legend.text = element_text(size = 8),
        legend.key.width = unit(1.5, "cm")
      )
    
    legend <- get_legend(legend_plot)
  } else {
    legend <- ggdraw()
  }
  
  # Combine title, main plot, and legend
  complete_plot <- plot_grid(
    title,
    final_plot,
    legend,
    ncol = 1,
    rel_heights = c(0.08, 0.85, 0.07)
  )
  
  return(complete_plot)
}

# Determine plot title based on filename
plot_title <- if (grepl("total", input_file, ignore.case = TRUE)) {
  "Total Concurrency Analysis"
} else if (grepl("same", input_file, ignore.case = TRUE)) {
  "Same-Process Concurrency Analysis"
} else {
  "Concurrency Analysis"
}

# Create plots
cat("Processing concurrency analysis...\n")
concurrency_plot <- create_concurrency_plots(df_top, plot_title)

if (!is.null(concurrency_plot)) {
  cat("Saving concurrency plot to:", output_file, "\n")
  ggsave(output_file, concurrency_plot, width = 20, height = 16, dpi = 300)
} else {
  cat("No concurrency plot created\n")
}

cat("Analysis complete!\n")