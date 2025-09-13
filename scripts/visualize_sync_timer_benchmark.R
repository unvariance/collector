#!/usr/bin/env Rscript

# Check for required packages and install if missing
required_packages <- c("ggplot2", "dplyr", "tidyr", "optparse", "scales")
new_packages <- required_packages[!(required_packages %in% installed.packages()[,"Package"])]
if(length(new_packages)) install.packages(new_packages)

library(ggplot2)
library(dplyr)
library(tidyr)
library(optparse)
library(scales)

# Parse command line arguments
option_list = list(
    make_option(c("-i", "--input"), type="character", default=NULL, 
                help="Input CSV file", metavar="character"),
    make_option(c("-p", "--prefix"), type="character", default="benchmark",
                help="Output file prefix [default= %default]", metavar="character"),
    make_option(c("-w", "--width"), type="numeric", default=16,
                help="Plot width in inches [default= %default]", metavar="number"),
    make_option(c("-h", "--height"), type="numeric", default=9,
                help="Plot height in inches [default= %default]", metavar="number")
)

opt_parser = OptionParser(option_list=option_list, add_help_option=FALSE)
opt = parse_args(opt_parser)

if (is.null(opt$input)) {
    print_help(opt_parser)
    stop("Input CSV file must be specified.", call.=FALSE)
}

# Read data
data <- read.csv(opt$input)

# Convert timestamp to relative seconds from start and other unit conversions
data <- data %>%
    group_by(experiment) %>%
    mutate(
        relative_time = (timestamp - min(timestamp)) / 1e9,  # Convert ns to s
        mean_delay_us = mean_delay / 1000,  # Convert ns to μs
        min_delay_us = min_delay / 1000,
        max_delay_us = max_delay / 1000,
        range_us = max_delay_us - min_delay_us
    )

# Common theme settings
base_theme <- theme_minimal() +
    theme(
        panel.grid.minor=element_blank(),
        strip.text=element_text(size=12, face="bold"),
        plot.title=element_text(size=14, face="bold"),
        plot.subtitle=element_text(size=10)
    )

# Color palette
experiment_colors <- scale_color_brewer(palette="Set2")
experiment_fills <- scale_fill_brewer(palette="Set2")

# Function to create time series plot
create_time_series <- function(data, title_suffix="") {
    ggplot(data, aes(x=relative_time)) +
        # Min-max lines with minimum width for visibility
        geom_segment(aes(xend=relative_time, y=min_delay_us, yend=max_delay_us, color=experiment),
                    linewidth=0.2) +
        # Mean points
        geom_point(aes(y=mean_delay_us, color=experiment), size=0.5, alpha=0.7) +
        labs(x="Time (seconds)",
             y="Timer Delay (\u00B5s)",
             title=paste("Sync Timer Benchmark Results", title_suffix),
             subtitle="Points show mean delay, vertical lines show min-max range") +
        base_theme +
        experiment_colors +
        theme(
            legend.position="none",
            panel.spacing=unit(1, "lines"),
            strip.text.x=element_text(margin=margin(b=10)),
            plot.margin=margin(t=10, r=10, b=10, l=10)
        )
}

# 1. All data, hybrid layout (2x4 grid)
p_hybrid <- create_time_series(data) +
    facet_wrap(~experiment, nrow=2, scales="free_y")

ggsave(paste0(opt$prefix, "_all.pdf"), p_hybrid, width=opt$width, height=opt$height)

# 2. Short duration plot, hybrid layout (500ms)
data_short <- data %>%
    group_by(experiment) %>%
    filter(relative_time <= 0.5)  # 500ms

p_short_hybrid <- create_time_series(data_short, "(500ms)") +
    facet_wrap(~experiment, nrow=2, scales="free_y")

ggsave(paste0(opt$prefix, "_short.pdf"), p_short_hybrid, width=opt$width, height=opt$height)

# 3. CDF/SDF plots
# Prepare data for survival distribution function plots
density_data <- data %>%
    select(experiment, mean_delay_us, max_delay_us, range_us) %>%
    gather(metric, value, -experiment) %>%
    group_by(experiment, metric) %>%
    arrange(value) %>%
    mutate(
        # Calculate survival function (1 - CDF)
        # Use (n+1) in denominator to avoid exactly 0
        sdf = (n() - row_number() + 1) / (n() + 1),
        # For log scale plotting, ensure we don't hit exactly 0
        sdf_log = pmax((n() - row_number() + 1) / (n() + 1), 1e-6)
    )

# Create labels for metrics
metric_labels <- c(
    mean_delay_us = "Mean Delay SDF",
    max_delay_us = "Maximum Delay SDF",
    range_us = "Delay Range SDF"
)

# Common SDF plot settings
sdf_common <- list(
    facet_wrap(~metric, nrow=1, scales="fixed", 
               labeller=labeller(metric=metric_labels)),
    scale_x_log10(
        limits=c(1, 500),
        breaks=c(1, 2, 5, 10, 20, 50, 100, 200, 500),
        labels=function(x) paste0(x, " \u00B5s")
    ),
    base_theme,
    experiment_colors,
    theme(
        panel.spacing=unit(2, "lines"),
        strip.text.x=element_text(margin=margin(b=10)),
        plot.margin=margin(t=10, r=10, b=10, l=10)
    )
)

# Linear y-axis SDF
p_sdf <- ggplot(density_data, aes(x=value, y=sdf, color=experiment)) +
    geom_step(linewidth=1) +
    scale_y_continuous(
        labels=scales::percent,
        breaks=c(0, 0.001, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0),
        trans="reverse"
    ) +
    labs(x="Delay",
         y="P(Delay > x)",
         title="Sync Timer Delay Survival Distributions",
         subtitle="Survival distribution functions (log-x scale)") +
    sdf_common

ggsave(paste0(opt$prefix, "_sdf.pdf"), p_sdf, width=opt$width, height=opt$height)

# Create a custom transformation that combines log and reverse
log_reverse_trans <- function() {
    trans_new(
        "log-reverse",
        transform = function(x) -log10(x),
        inverse = function(x) 10^(-x),
        breaks = log10_trans()$breaks,
        domain = c(1e-100, Inf)
    )
}

# Log-y SDF for better tail visualization
p_sdf_log <- ggplot(density_data, aes(x=value, y=sdf_log, color=experiment)) +
    geom_step(linewidth=1) +
    scale_y_continuous(
        breaks=c(1, 0.5, 0.1, 0.05, 0.01, 0.005, 0.001, 1e-4, 1e-5),
        labels=scales::percent,
        expand=expansion(mult=c(0.1, 0.1)),
        trans=log_reverse_trans()
    ) +
    labs(x="Delay",
         y="P(Delay > x)",
         title="Sync Timer Delay Survival Distributions",
         subtitle="Survival distribution functions (log-log scale)") +
    sdf_common

ggsave(paste0(opt$prefix, "_sdf_log.pdf"), p_sdf_log, width=opt$width, height=opt$height)

cat(sprintf("Plots saved with prefix: %s\n", opt$prefix)) 