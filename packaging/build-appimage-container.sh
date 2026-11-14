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

IMAGE_NAME="soundhop-anylinux-builder:v3"

# Pre-warm Arch Linux builder image once to eliminate pacman/rustup delays
if ! $CONTAINER_CMD image exists "$IMAGE_NAME" 2>/dev/null; then
    echo "Building cached Arch Linux builder image '$IMAGE_NAME' (one-time setup)..."
    if $CONTAINER_CMD build -t "$IMAGE_NAME" -f - "$REPO_DIR" << 'DOCKERFILE'; then
FROM docker.io/library/archlinux:base-devel
ENV PATH="/root/.cargo/bin:${PATH}"
ARG ANYLINUX_COMMIT=f7e9cb26818bdeb3d64a0fe055fdd51cbc887a81
ARG GET_DEBLOATED_SHA256=d215f7676785f1a1bb8784e69085d66c1df81488ef31f1ba7198325a5be05130
RUN printf 'Server = https://mirrors.kernel.org/archlinux/$repo/os/$arch\nServer = https://mirror.rackspace.com/archlinux/$repo/os/$arch\nServer = https://geo.mirror.pkgbuild.com/$repo/os/$arch\n' > /etc/pacman.d/mirrorlist \
    && for i in 1 2 3; do \
      pacman -Sy --noconfirm --needed archlinux-keyring && \
      pacman -Syu --noconfirm --needed \
        rustup pkgconf wget curl file strace binutils patchelf zsync \
        squashfs-tools desktop-file-utils appstream xorg-server-xvfb \
        gtk3 libayatana-appindicator xdotool libxcb libxcursor libxi libxrandr \
        libxkbcommon libxkbcommon-x11 wayland mesa openssl && break || sleep 3; \
    done \
    && curl --proto '=https' --tlsv1.2 --retry 3 --retry-delay 2 -fsSL "https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/${ANYLINUX_COMMIT}/useful-tools/get-debloated-pkgs.sh" -o /tmp/get-debloated-pkgs.sh \
    && printf '%s  %s\n' "$GET_DEBLOATED_SHA256" /tmp/get-debloated-pkgs.sh | sha256sum -c - \
    && chmod +x /tmp/get-debloated-pkgs.sh \
    && /tmp/get-debloated-pkgs.sh --add-opengl --prefer-nano \
        gtk3-mini libxml2-mini icu-mini glycin-mini \
    && rm -f /tmp/get-debloated-pkgs.sh \
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

TTY_FLAGS="-i"
if [ -t 0 ] && [ -t 1 ]; then
    TTY_FLAGS="-it"
fi

$CONTAINER_CMD run --rm $TTY_FLAGS \
    ${UPDATE_REPO:+-e UPDATE_REPO="$UPDATE_REPO"} \
    ${VERSION:+-e VERSION="$VERSION"} \
    -v "$REPO_DIR:/work:Z" \
    -w /work \
    "$IMAGE_NAME" /work/packaging/container-build.sh
