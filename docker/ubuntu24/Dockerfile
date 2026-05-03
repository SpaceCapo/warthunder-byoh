FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential curl ca-certificates git python3-pip pkg-config libssl-dev llvm clang cmake pkg-config \
    libx11-dev libxrandr-dev libxinerama-dev libxcursor-dev libxi-dev libgl1-mesa-dev libdbus-1-dev \
    mingw-w64 musl-tools && \
    rm -rf /var/lib/apt/lists/*

# Install rustup and set default toolchain
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup default stable

# Preinstall common cross targets to speed up CI builds
RUN rustup target add x86_64-pc-windows-gnu x86_64-unknown-linux-musl || true

WORKDIR /work

CMD ["/bin/bash"]
