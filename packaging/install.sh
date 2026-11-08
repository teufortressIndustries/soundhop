#!/usr/bin/env bash
# ==============================================================================
# SoundHop v2.0 Turnkey Cross-Distro Installer (Rust Release Build)
# Works on Fedora, CachyOS, Arch Linux, Ubuntu/Debian, openSUSE, and more.
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

BOLD='\033[1m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BOLD}${CYAN}=== SoundHop v2.0 Installer ===${NC}"
echo "Building and installing native Rust binaries..."
echo

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

if ! command_exists cargo; then
    echo -e "${RED}Error: cargo (Rust toolchain) is required to build from source.${NC}"
    exit 1
fi

BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"

echo -e "${BOLD}Compiling SoundHop v2.0 (--release)...${NC}"
(
    cd "$REPO_DIR"
    cargo build --release --workspace
)

install -Dm755 "$REPO_DIR/target/release/soundhop" "$BIN_DIR/soundhop"
export PATH="$BIN_DIR:$PATH"

if [[ -f "$REPO_DIR/assets/soundhop.svg" ]]; then
    cp "$REPO_DIR/assets/soundhop.svg" "$ICON_DIR/soundhop.svg"
fi

if [[ -f "$REPO_DIR/packaging/io.github.soundhop.desktop" ]]; then
    sed "s|Exec=soundhop tray|Exec=$BIN_DIR/soundhop tray|g" "$REPO_DIR/packaging/io.github.soundhop.desktop" > "$APP_DIR/io.github.soundhop.desktop"
    chmod 644 "$APP_DIR/io.github.soundhop.desktop"
fi

if command_exists systemctl && systemctl --user show-environment >/dev/null 2>&1; then
    echo -e "\n${BOLD}Setting up systemd user service...${NC}"
    "$BIN_DIR/soundhop" service install || true
    echo -e "${GREEN}✓ SoundHop background service enabled and started.${NC}"
fi

echo
echo -e "${GREEN}${BOLD}✓ SoundHop v2.0 installation complete!${NC}"
echo -e "  ${CYAN}soundhop${NC}          # Launch interactive TUI dashboard"
echo -e "  ${CYAN}soundhop tray${NC}     # Launch native system tray"
echo -e "  ${CYAN}soundhop status${NC}   # View status"
