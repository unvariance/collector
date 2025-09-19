############################
# Minimal packaging images (COPY prebuilt binaries)
############################

############################
# Collector runtime image
############################
FROM debian:trixie-slim AS collector
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y \
    libelf1 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Expect a prebuilt binary at dist/collector in the build context
COPY dist/collector /usr/local/bin/collector
ENTRYPOINT ["/usr/local/bin/collector"]

############################
# nri-init runtime image (with nsenter from util-linux)
############################
FROM debian:trixie-slim AS nri-init
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y \
    util-linux \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Expect a prebuilt binary at dist/nri-init in the build context
COPY dist/nri-init /usr/local/bin/nri-init
# Keep a legacy path for compatibility with docs/charts
RUN ln -sf /usr/local/bin/nri-init /bin/nri-init
ENTRYPOINT ["/usr/local/bin/nri-init"]
