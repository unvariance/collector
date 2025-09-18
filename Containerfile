############################
# Base toolchain image with deps
############################
FROM rust:1.86-slim-bookworm AS base

ARG DEBIAN_FRONTEND=noninteractive

# Ensure cargo is always on PATH for all subsequent stages
ENV PATH="/usr/local/cargo/bin:${PATH}"

# Use a consistent working directory across all build stages
WORKDIR /app

RUN apt-get update && apt-get install -y \
    clang-19 \
    libelf-dev \
    make \
    pkg-config \
    ca-certificates \
    curl \
    git \
    unzip \
    && ln -sf /usr/bin/clang-19 /usr/bin/clang \
    && rm -rf /var/lib/apt/lists/*

# Rust developer tooling
RUN rustup component add rustfmt clippy && \
    cargo install cargo-chef --locked

############################
# Planner stage (cargo-chef)
############################
FROM base AS planner
COPY . .
# Generate the cargo-chef recipe into /app/recipe.json
RUN cargo chef prepare --recipe-path /app/recipe.json

############################
# Builder base (cached deps)
############################
FROM base AS builder
COPY --from=planner /app/recipe.json /app/recipe.json
# Pre-build dependency layer using cargo-chef to speed up subsequent builds
RUN cargo chef cook --release --recipe-path /app/recipe.json

############################
# Collector build stage
############################
FROM builder AS collector-build
COPY . /app
RUN cargo build --release --bin collector

############################
# Collector runtime image
############################
FROM debian:bookworm-slim AS collector
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y \
    libelf1 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && addgroup --system collector \
    && adduser --system --ingroup collector collector
COPY --from=collector-build /app/target/release/collector /usr/local/bin/collector
USER collector:collector
ENTRYPOINT ["/usr/local/bin/collector"]

############################
# nri-init build stage
############################
FROM builder AS nri-init-build
COPY . /app
RUN cargo build --release -p nri-init

############################
# nri-init runtime image (with nsenter from util-linux)
############################
FROM debian:bookworm-slim AS nri-init
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y \
    util-linux \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=nri-init-build /app/target/release/nri-init /usr/local/bin/nri-init
# Keep a legacy path for compatibility with docs/charts
RUN ln -sf /usr/local/bin/nri-init /bin/nri-init
ENTRYPOINT ["/usr/local/bin/nri-init"]
