# The Linux packages' builder (installer/linux/deb/build.sh): Debian 12, the oldest system
# the packages are for, so the programs need no newer C library than it has (glibc 2.36:
# Debian 12, Ubuntu 24.04 and 26.04 all run them). The repository, the cargo cache, FFmpeg
# and the Vulkan headers are mounted at build time; nothing of them is in the image.
FROM debian:12
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential pkg-config clang libclang-dev cmake curl ca-certificates xz-utils git \
      libasound2-dev libgtk-3-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev \
      libxkbcommon-dev libwayland-dev libudev-dev glslc patchelf \
    && rm -rf /var/lib/apt/lists/*
ARG RUST=1.99.1
ENV RUSTUP_HOME=/opt/rustup PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | CARGO_HOME=/opt/cargo sh -s -- -y --profile minimal \
      --default-toolchain $RUST --no-modify-path
