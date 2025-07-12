#!/usr/bin/env Rscript

# CPI Histogram Analysis for Hyperthread Contention
# This script creates probability density plots showing the distribution of 
# cycles per instruction (CPI) for the top 20 processes, weighted by instructions
# and categorized by peer hyperthread activity.

library(nanoparquet)
library(dplyr)
library(ggplot2)
library(tidyr)
library(stringr)

# Command line argument parsing
args <- commandArgs(trailingOnly = TRUE)
if (length(args) != 1) {
  cat("Usage: Rscript cpi_histogram.R <input_parquet_file>\n")
  cat("Input should be the output from trace-analysis (augmented with hyperthread counters)\n")
  quit(status = 1)
}

input_file <- args[1]
output_file <- str_replace(input_file, "\\.parquet$", "_cpi_histogram.pdf")

cat("Reading parquet file:", input_file, "\n")

# Read the augmented trace data
df <- nanoparquet::read_parquet(input_file)

cat("Loaded", nrow(df), "rows\n")

# Calculate CPI (cycles per instruction)
# Filter out rows with zero instructions to avoid division by zero
df <- df %>%
  filter(instructions > 0) %>%
  mutate(cpi = cycles / instructions)

cat("After filtering zero instructions:", nrow(df), "rows\n")

# Find top 20 processes by total instructions
top_processes <- df %>%
  group_by(process_name) %>%
  summarise(total_instructions = sum(instructions), .groups = 'drop') %>%
  arrange(desc(total_instructions)) %>%
  head(20) %>%
  pull(process_name)

cat("Top 20 processes by instruction count:\n")
print(top_processes)

# Filter data to only include top 20 processes
df_top <- df %>%
  filter(process_name %in% top_processes) %>%
  # Only include rows where at least one hyperthread counter is non-zero
  filter(ns_peer_same_process > 0 | ns_peer_different_process > 0 | ns_peer_kernel > 0)

cat("After filtering to top processes with hyperthread data:", nrow(df_top), "rows\n")

# Calculate CPU seconds for each process and category for statistical significance
cpu_seconds_summary <- df_top %>%
  select(process_name, ns_peer_same_process, ns_peer_different_process, ns_peer_kernel) %>%
  group_by(process_name) %>%
  summarise(
    cpu_seconds_same = sum(ns_peer_same_process) / 1e9,
    cpu_seconds_different = sum(ns_peer_different_process) / 1e9,
    cpu_seconds_kernel = sum(ns_peer_kernel) / 1e9,
    .groups = 'drop'
  ) %>%
  mutate(
    # Create formatted title with CPU seconds
    process_title = sprintf("%s\nSame: %.2fs | Diff: %.2fs | Kernel: %.2fs",
                           process_name,
                           cpu_seconds_same,
                           cpu_seconds_different, 
                           cpu_seconds_kernel)
  )

# Reshape data for the three hyperthread categories
# For each category, calculate weight = nanoseconds / CPI (proportional to instructions)
df_long <- df_top %>%
  select(process_name, cpi, ns_peer_same_process, ns_peer_different_process, ns_peer_kernel) %>%
  pivot_longer(cols = starts_with("ns_peer_"), 
               names_to = "peer_category", 
               values_to = "nanoseconds") %>%
  filter(nanoseconds > 0) %>%  # Only include non-zero categories
  mutate(
    # Clean up category names
    peer_category = case_when(
      peer_category == "ns_peer_same_process" ~ "Same Process",
      peer_category == "ns_peer_different_process" ~ "Different Process", 
      peer_category == "ns_peer_kernel" ~ "Kernel",
      TRUE ~ peer_category
    ),
    # Calculate weight proportional to instructions
    instruction_weight = nanoseconds / cpi
  ) %>%
  # Add process titles with CPU seconds
  left_join(cpu_seconds_summary, by = "process_name")

cat("After reshaping:", nrow(df_long), "rows\n")

# Create the plot
cat("Creating plot...\n")

p <- ggplot(df_long, aes(x = cpi, weight = instruction_weight, color = peer_category)) +
  geom_density(alpha = 0.7, linewidth = 1) +
  facet_wrap(~ process_title, scales = "free_y", ncol = 4) +
  coord_cartesian(xlim = c(0, 10)) +
  scale_color_manual(values = c("Same Process" = "#2E8B57", 
                               "Different Process" = "#FF6347", 
                               "Kernel" = "#4169E1")) +
  labs(
    title = "CPI Distribution by Peer Hyperthread Activity", 
    subtitle = "Top 20 processes by instruction count, weighted by instructions\nProcess titles show CPU seconds for statistical significance assessment\n[Display limited to CPI ≤ 10]",
    x = "Cycles Per Instruction (CPI)",
    y = "Density (Instruction-weighted)",
    color = "Peer Hyperthread"
  ) +
  theme_minimal() +
  theme(
    plot.title = element_text(size = 16, hjust = 0.5),
    plot.subtitle = element_text(size = 10, hjust = 0.5),
    axis.text.x = element_text(angle = 45, hjust = 1, size = 8),
    axis.text.y = element_text(size = 8),
    legend.position = "bottom",
    strip.text = element_text(size = 7),
    panel.grid.minor = element_blank()
  ) +
  guides(color = guide_legend(override.aes = list(size = 2)))

# Save the plot
ggsave(output_file, plot = p, width = 16, height = 12, dpi = 300)

cat("Plot saved to:", output_file, "\n")

# Print CPU seconds summary for statistical significance assessment
cat("\nCPU seconds by process and category (for statistical significance):\n")
cpu_seconds_clean <- cpu_seconds_summary %>% select(-process_title)
print(cpu_seconds_clean)

cat("\nAnalysis complete!\n")