# Stage 1: pull the osxcross distroless image just to get the SDK + toolchain.
FROM crazymax/osxcross:latest-ubuntu AS osxcross

# Stage 2: Ubuntu base with bash, Rust, and the osxcross tools copied in.
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential curl ca-certificates git \
        libssl-dev pkg-config \
        libclang-dev clang cmake \
        libx11-dev libxrandr-dev libxinerama-dev libxcursor-dev libxi-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy the entire osxcross installation from the distroless image.
# It lives at /osxcross in that image.
COPY --from=osxcross /osxcross /osxcross

# Put the cross toolchain on PATH and register its shared libs.
ENV PATH=/osxcross/bin:$PATH
ENV SDKROOT=/osxcross/SDK/MacOSX26.1.sdk

RUN echo /osxcross/lib > /etc/ld.so.conf.d/osxcross.conf && ldconfig

# Install Rust (stable) and the macOS cross-compilation targets.
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y
ENV PATH=/root/.cargo/bin:$PATH
RUN rustup default stable \
    && rustup target add x86_64-apple-darwin aarch64-apple-darwin || true

# Tell the `cc` crate which compiler to use for each darwin target.
# The crate checks CC_<target_underscored> first.
ENV CC_x86_64_apple_darwin=x86_64-apple-darwin25.1-clang
ENV CXX_x86_64_apple_darwin=x86_64-apple-darwin25.1-clang++
ENV AR_x86_64_apple_darwin=x86_64-apple-darwin25.1-ar
ENV CC_aarch64_apple_darwin=aarch64-apple-darwin25.1-clang
ENV CXX_aarch64_apple_darwin=aarch64-apple-darwin25.1-clang++
ENV AR_aarch64_apple_darwin=aarch64-apple-darwin25.1-ar

# Ensure Cargo/rustc uses the osxcross wrapper as the linker for the
# darwin targets. This makes rustc invoke the correct cross-compiler
# which understands macOS-specific flags like `-framework` and `-arch`.
ENV CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER=x86_64-apple-darwin25.1-clang
ENV CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=aarch64-apple-darwin25.1-clang

WORKDIR /work
