#!/usr/bin/env bash
# ==============================================================================
# SoundHop v2.0 Portable AppImage Builder (Ubuntu 22.04 Container)
# Builds a universal AppImage on Ubuntu 22.04 LTS (glibc 2.35 + Rust toolchain)
# Runs cleanly across all Linux distributions (Ubuntu, Debian, Fedora, Arch, CachyOS).
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

CONTAINER_CMD=""
if command -v podman >/dev/null 2>&1; then
    CONTAINER_CMD="podman"
elif command -v docker >/dev/null 2>&1; then
    CONTAINER_CMD="docker"
else
    echo "Error: Neither podman nor docker was found. Please install podman or docker."
    exit 1
fi

echo "=== Building Universal SoundHop v2.0 AppImage using Ubuntu 22.04 ($CONTAINER_CMD) ==="

IMAGE_NAME="soundhop-rust-builder:v2"

# Check if pre-warmed Rust v2 builder image exists; build it once to eliminate apt-get/rustup delays
if ! $CONTAINER_CMD image exists "$IMAGE_NAME" 2>/dev/null; then
    echo "Building cached Rust builder image '$IMAGE_NAME' (one-time setup)..."
    if $CONTAINER_CMD build -t "$IMAGE_NAME" -f - "$REPO_DIR" << 'DOCKERFILE'; then
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
ENV PATH="/root/.cargo/bin:${PATH}"
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
    build-essential pkg-config wget curl ca-certificates file \
    libgtk-3-dev libayatana-appindicator3-dev libxdo-dev \
    libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
    libgl1-mesa-dev libssl-dev zstd zsync binutils squashfs-tools appstream \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal \
    && rm -rf /var/lib/apt/lists/*
DOCKERFILE
        echo "✓ Rust builder image '$IMAGE_NAME' cached."
    else
        echo "Notice: Could not cache builder image, falling back to base ubuntu:22.04."
        IMAGE_NAME="ubuntu:22.04"
    fi
fi

chmod +x "$REPO_DIR/packaging/container-build.sh"

TTY_FLAGS="-i"
if [ -t 0 ] && [ -t 1 ]; then
    TTY_FLAGS="-it"
fi

$CONTAINER_CMD run --rm $TTY_FLAGS \
    ${UPDATE_REPO:+-e UPDATE_REPO="$UPDATE_REPO"} \
    -v "$REPO_DIR:/work:Z" \
    -w /work \
    "$IMAGE_NAME" /work/packaging/container-build.sh
