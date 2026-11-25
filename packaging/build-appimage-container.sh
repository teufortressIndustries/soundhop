#!/usr/bin/env bash
# ==============================================================================
# SoundHop v2.0 AnyLinux AppImage Builder (Arch Linux Container)
# Uses pkgforge-dev/Anylinux-AppImages (quick-sharun + uruntime/DwarFS)
# Produces a distro-agnostic AppImage that runs on glibc, musl, and non-FUSE hosts.
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

echo "=== Building SoundHop v2.0 AnyLinux AppImage using Arch Linux ($CONTAINER_CMD) ==="

IMAGE_NAME="soundhop-anylinux-builder:v4"

# Pre-warm Arch Linux builder image once to eliminate pacman/rustup delays
if ! $CONTAINER_CMD image exists "$IMAGE_NAME" 2>/dev/null; then
    echo "Building cached Arch Linux builder image '$IMAGE_NAME' (one-time setup)..."
    if $CONTAINER_CMD build -t "$IMAGE_NAME" -f - "$REPO_DIR" << 'DOCKERFILE'; then
FROM docker.io/library/archlinux:base-devel
ENV PATH="/root/.cargo/bin:${PATH}"
RUN printf 'Server = https://mirrors.kernel.org/archlinux/$repo/os/$arch\nServer = https://mirror.rackspace.com/archlinux/$repo/os/$arch\nServer = https://geo.mirror.pkgbuild.com/$repo/os/$arch\n' > /etc/pacman.d/mirrorlist \
    && for i in 1 2 3; do \
      pacman -Sy --noconfirm --needed archlinux-keyring && \
      pacman -Syu --noconfirm --needed \
        rustup pkgconf wget curl file strace binutils patchelf zsync \
        squashfs-tools desktop-file-utils appstream xorg-server-xvfb \
        gtk3 libayatana-appindicator xdotool libxcb libxcursor libxi libxrandr \
        libxkbcommon libxkbcommon-x11 wayland mesa \
        gstreamer gst-plugins-base-libs gst-plugins-base gst-plugins-good gst-plugins-bad && break || sleep 3; \
    done \
    && mkdir -p /tmp/debloated-pkgs \
    && host_arch="$(uname -m)" \
    && for pkg in mesa-nano gtk3-mini libxml2-mini icu-mini glycin-mini; do \
        curl --ipv4 --connect-timeout 15 --max-time 180 --proto '=https' --tlsv1.2 --retry 3 --retry-delay 2 -fsSL \
          "https://github.com/pkgforge-dev/archlinux-pkgs-debloated/releases/download/continuous/${pkg}-${host_arch}.pkg.tar.zst" \
          -o "/tmp/debloated-pkgs/${pkg}-${host_arch}.pkg.tar.zst"; \
    done \
    && pacman -U --noconfirm --ask 4 /tmp/debloated-pkgs/*.pkg.tar.zst \
    && rm -rf /tmp/debloated-pkgs \
    && rustup default stable \
    && pacman -Scc --noconfirm
DOCKERFILE
        echo "✓ Arch Linux builder image '$IMAGE_NAME' cached."
    else
        echo "Notice: Could not cache builder image, falling back to base archlinux:base-devel."
        IMAGE_NAME="docker.io/library/archlinux:base-devel"
    fi
fi

chmod +x "$REPO_DIR/packaging/container-build.sh"

TTY_FLAG="-i"
if [ -t 0 ] && [ -t 1 ]; then
    TTY_FLAG="-it"
fi

RUN_ARGS=(--rm "$TTY_FLAG")
if [ -n "${UPDATE_REPO:-}" ]; then
    RUN_ARGS+=(-e "UPDATE_REPO=$UPDATE_REPO")
fi
if [ -n "${VERSION:-}" ]; then
    RUN_ARGS+=(-e "VERSION=$VERSION")
fi

"$CONTAINER_CMD" run "${RUN_ARGS[@]}" \
    -v "$REPO_DIR:/work:Z" \
    -w /work \
    "$IMAGE_NAME" /work/packaging/container-build.sh
