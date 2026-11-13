#!/usr/bin/env bash
# ==============================================================================
# SoundHop v2.0 AnyLinux AppImage Build Script
# Delegates to Arch Linux container on non-Arch hosts (recommended by
# pkgforge-dev/Anylinux-AppImages), or runs natively on Arch Linux.
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"

if [[ ! -f /etc/arch-release && -z "${ALLOW_NON_ARCH:-}" ]]; then
    echo "Non-Arch host detected; building AnyLinux AppImage in Arch Linux container..."
    exec "$SCRIPT_DIR/build-appimage-container.sh"
fi

exec "$SCRIPT_DIR/container-build.sh"
