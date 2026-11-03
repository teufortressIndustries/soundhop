#!/usr/bin/env bash
# ==============================================================================
# SoundHop Portable AppImage Builder (Ubuntu Container)
# Builds a truly universal AppImage on Ubuntu 22.04 LTS (glibc 2.35 / standard Python)
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

echo "=== Building Universal SoundHop AppImage using Ubuntu 22.04 ($CONTAINER_CMD) ==="

IMAGE_NAME="soundhop-builder:latest"

# Check if pre-warmed builder image exists; build it once to eliminate apt-get delays
if ! $CONTAINER_CMD image exists "$IMAGE_NAME" 2>/dev/null; then
    echo "Building cached builder image '$IMAGE_NAME' (one-time setup)..."
    if $CONTAINER_CMD build -t "$IMAGE_NAME" -f - "$REPO_DIR" << 'DOCKERFILE'; then
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
    python3 python3-pip python3-venv wget curl ca-certificates file libgl1 libegl1 libxkbcommon0 libpulse0 zstd binutils \
    && rm -rf /var/lib/apt/lists/*
DOCKERFILE
        echo "✓ Builder image cached."
    else
        echo "Notice: Could not cache builder image, falling back to base ubuntu:22.04."
        IMAGE_NAME="ubuntu:22.04"
    fi
fi

chmod +x "$REPO_DIR/packaging/container-build.sh"

$CONTAINER_CMD run --rm -it \
    ${UPDATE_REPO:+-e UPDATE_REPO="$UPDATE_REPO"} \
    -v "$REPO_DIR:/work:Z" \
    -w /work \
    "$IMAGE_NAME" /work/packaging/container-build.sh
