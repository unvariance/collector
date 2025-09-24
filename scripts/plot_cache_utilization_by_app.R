#!/usr/bin/env Rscript

# Plot stacked LLC cache utilization (CMT) by application, aggregating pods.
# Mirrors aesthetics and CLI of plot_memory_usage.R

if (!requireNamespace("nanoparquet", quietly = TRUE)) install.packages("nanoparquet", repos = "https://cloud.r-project.org/")
if (!requireNamespace("ggplot2", quietly = TRUE)) install.packages("ggplot2", repos = "https://cloud.r-project.org/")
if (!requireNamespace("dplyr", quietly = TRUE)) install.packages("dplyr", repos = "https://cloud.r-project.org/")
if (!requireNamespace("tidyr", quietly = TRUE)) install.packages("tidyr", repos = "https://cloud.r-project.org/")

library(nanoparquet)
library(ggplot2)
library(dplyr)
library(tidyr)

NS_PER_SEC <- 1e9
BYTES_PER_MB <- 1024^2

# Args: <parquet_file> <start_time_offset_sec> <window_size_sec> <output_prefix> <top_n_apps>
args <- commandArgs(trailingOnly = TRUE)
input_file <- if (length(args) >= 1) args[1] else "scripts/resctrl.parquet"
start_time_offset <- if (length(args) >= 2) as.numeric(args[2]) else 110
window_size <- if (length(args) >= 3) as.numeric(args[3]) else 1
output_prefix <- if (length(args) >= 4) args[4] else "cache_utilization_by_app"
top_n_apps <- if (length(args) >= 5) as.numeric(args[5]) else 20

# Derive application name from Kubernetes pod name by stripping replica/hash suffixes
derive_app_name <- function(pod_name) {
  if (is.na(pod_name) || pod_name == "") return(NA_character_)
  # First try: remove two trailing dash-separated tokens (deployment pattern)
  app <- sub("-[a-z0-9]+-[a-z0-9]+$", "", pod_name)
  # If nothing changed, try removing a single trailing token (statefulset pattern)
  if (identical(app, pod_name)) app <- sub("-[a-z0-9]+$", "", pod_name)
  app
}

load_and_window <- function(path, start_offset_s, window_s) {
  message("Reading parquet: ", path)
  df <- nanoparquet::read_parquet(path)

  # Validate required columns
  required <- c("start_timestamp", "timestamp", "llc_occupancy_bytes")
  missing <- setdiff(required, colnames(df))
  if (length(missing) > 0) stop("Missing columns in resctrl parquet: ", paste(missing, collapse = ", "))

  # Use pod_name if present to derive app; otherwise fall back to resctrl_group UID match
  if (!"pod_name" %in% names(df)) df$pod_name <- NA_character_

  # Normalize time using start_timestamp so epochs align
  min_ts <- suppressWarnings(min(df$start_timestamp, na.rm = TRUE))
  if (!is.finite(min_ts)) stop("Invalid timestamps in parquet")

  window_start_ns <- as.numeric(start_offset_s) * NS_PER_SEC
  window_end_ns   <- as.numeric(start_offset_s + window_s) * NS_PER_SEC

  df$epoch_start_ns <- df$start_timestamp - min_ts

  message(sprintf("Filtering window: %.3fs to %.3fs (absolute)", window_start_ns/NS_PER_SEC, window_end_ns/NS_PER_SEC))

  win <- df %>%
    filter(epoch_start_ns >= window_start_ns, epoch_start_ns <= window_end_ns)

  # Derive application name
  win$app_name <- vapply(win$pod_name, derive_app_name, FUN.VALUE = character(1))
  # For rows without pod_name, try to infer from resctrl_group path (contains pod_<uid>) — leave as NA if unknown
  win$app_name[is.na(win$app_name) | win$app_name == ""] <- NA_character_

  # Drop rows without app name; they are usually early bootstrap with NA pod_name
  win <- win %>% filter(!is.na(app_name))

  attr(win, "window_start_s") <- window_start_ns / NS_PER_SEC
  attr(win, "window_end_s") <- window_end_ns / NS_PER_SEC
  win
}

prepare_plot_data <- function(win, n_top = top_n_apps) {
  if (nrow(win) == 0) stop("No rows in selected window after filtering")

  # Compute epoch widths as time until next epoch; last epoch uses median gap
  uniq_epochs <- sort(unique(win$epoch_start_ns))
  if (length(uniq_epochs) >= 2) {
    gaps <- diff(uniq_epochs)
    median_gap_ns <- median(gaps[is.finite(gaps)], na.rm = TRUE)
  } else {
    median_gap_ns <- window_size * NS_PER_SEC
  }
  epoch_widths <- data.frame(
    epoch_start_ns = uniq_epochs,
    epoch_width_ns = c(diff(uniq_epochs), median_gap_ns)
  )

  # Select top apps by total occupancy across the window
  app_totals <- win %>%
    group_by(app_name) %>%
    summarise(total_bytes = sum(llc_occupancy_bytes, na.rm = TRUE), .groups = "drop") %>%
    arrange(desc(total_bytes))

  top_apps <- head(app_totals$app_name, n_top)

  coverage <- sum(app_totals$total_bytes[app_totals$app_name %in% top_apps], na.rm = TRUE) /
              max(1, sum(app_totals$total_bytes, na.rm = TRUE)) * 100
  message(sprintf("Top %d apps cover %.2f%% of occupancy bytes in window", length(top_apps), coverage))

  plot_df <- win %>%
    mutate(app_group = ifelse(app_name %in% top_apps, app_name, "other")) %>%
    group_by(epoch_start_ns, app_group) %>%
    summarise(occupancy_bytes = sum(llc_occupancy_bytes, na.rm = TRUE), .groups = "drop") %>%
    # Attach width per epoch
    left_join(epoch_widths, by = c("epoch_start_ns" = "epoch_start_ns")) %>%
    mutate(
      # Map to seconds relative to window start
      x_center_s = (epoch_start_ns - (attr(win, "window_start_s") * NS_PER_SEC)) / NS_PER_SEC + (epoch_width_ns / NS_PER_SEC)/2,
      width_s = pmax(1e-6, epoch_width_ns / NS_PER_SEC)  # avoid zero-width
    )

  # Color palette similar to plot_memory_usage.R
  base_cols <- c("#E41A1C", "#377EB8", "#4DAF4A", "#984EA3", "#FF7F00",
                 "#FFFF33", "#A65628", "#F781BF", "#999999")
  colors <- colorRampPalette(base_cols)(length(unique(plot_df$app_group)))
  names(colors) <- unique(plot_df$app_group)

  if ("other" %in% names(colors)) {
    colors["other"] <- "#CCCCCC"
    groups <- setdiff(names(colors), "other")
    plot_df$app_group <- factor(plot_df$app_group, levels = c(groups, "other"))
  }

  list(data = plot_df, colors = colors)
}

create_plot <- function(plot_data) {
  ws <- attr(plot_data$data, "window_start_s")
  we <- attr(plot_data$data, "window_end_s")
  subtitle <- sprintf("%.1fs window at %.1fs after experiment start (top apps aggregated)",
                      we - ws, ws)

  df <- plot_data$data %>% mutate(occupancy_mb = occupancy_bytes / BYTES_PER_MB)

  ggplot(df, aes(x = x_center_s, y = occupancy_mb, fill = app_group, width = width_s)) +
    geom_col(position = "stack", alpha = 0.85) +
    scale_fill_manual(values = plot_data$colors) +
    scale_y_continuous(labels = function(x) sprintf("%.1f", x)) +
    labs(
      title = "LLC Cache Utilization by Application",
      subtitle = subtitle,
      x = "Time (seconds)",
      y = "Megabytes (LLC occupancy)",
      fill = "Application"
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
}

main <- function() {
  tryCatch({
    if (!file.exists(input_file)) stop("Input file does not exist: ", input_file)

    win <- load_and_window(input_file, start_time_offset, window_size)
    if (nrow(win) < 10) stop("Not enough rows in the selected window; try a different offset/size")

    pd <- prepare_plot_data(win, top_n_apps)
    # Carry window attributes forward for subtitle
    attr(pd$data, "window_start_s") <- attr(win, "window_start_s")
    attr(pd$data, "window_end_s") <- attr(win, "window_end_s")

    p <- create_plot(pd)

    png_file <- paste0(output_prefix, ".png")
    pdf_file <- paste0(output_prefix, ".pdf")
    message("Saving PNG: ", png_file)
    ggsave(png_file, p, width = 16, height = 9, dpi = 300)
    message("Saving PDF: ", pdf_file)
    ggsave(pdf_file, p, width = 16, height = 9)
    message("Done")
  }, error = function(e) {
    message("Error: ", e$message)
    quit(status = 1)
  })
}

main()
