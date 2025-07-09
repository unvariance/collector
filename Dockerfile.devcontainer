FROM ubuntu:latest

# Avoid prompts during package installation
ENV DEBIAN_FRONTEND=noninteractive

# Add amd64 architecture
RUN dpkg --add-architecture amd64

# Set up repositories for both arm64 and amd64
RUN echo "deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports noble main restricted universe multiverse\n\
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports noble-updates main restricted universe multiverse\n\
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports noble-backports main restricted universe multiverse\n\
deb [arch=arm64] http://ports.ubuntu.com/ubuntu-ports noble-security main restricted universe multiverse\n\
deb [arch=amd64] http://archive.ubuntu.com/ubuntu noble main restricted universe multiverse\n\
deb [arch=amd64] http://archive.ubuntu.com/ubuntu noble-updates main restricted universe multiverse\n\
deb [arch=amd64] http://archive.ubuntu.com/ubuntu noble-backports main restricted universe multiverse\n\
deb [arch=amd64] http://security.ubuntu.com/ubuntu noble-security main restricted universe multiverse" > /etc/apt/sources.list

RUN rm /etc/apt/sources.list.d/ubuntu.sources

# Update and install essential build tools and kernel headers
RUN apt-get update && apt-get install -y \
    build-essential \
    clang-19 \
    libelf-dev \
    pkg-config \
    git \
    vim \
    curl \
    kmod \
    r-base \
    && rm -rf /var/lib/apt/lists/*

    RUN ln -s /usr/bin/clang-19 /usr/bin/clang

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# Set the working directory
WORKDIR /workspace

# Keep container running
CMD ["sleep", "infinity"]