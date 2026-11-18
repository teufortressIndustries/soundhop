#!/usr/bin/env bash
# ==============================================================================
# SoundHop v2.0 AnyLinux AppImage Build Script
# Delegates to Arch Linux container on non-Arch hosts (recommended by
# pkgforge-dev/Anylinux-AppImages), or runs natively on Arch Linux.
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"

if [[ ! -f /.dockerenv && ! -f /run/.containerenv && -z "${GITHUB_ACTIONS:-}" && -z "${ALLOW_HOST_MUTATION:-}" ]]; then
    echo "Building AnyLinux AppImage in Arch Linux container (set ALLOW_HOST_MUTATION=1 on Arch to run natively)..."
    exec "$SCRIPT_DIR/build-appimage-container.sh"
fi

exec "$SCRIPT_DIR/container-build.sh"
