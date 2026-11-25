#!/usr/bin/env bash
# ==============================================================================
# SoundHop Turnkey Cross-Distro Installer (Rust Release Build)
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

VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$REPO_DIR/Cargo.toml" | head -n1)"
VERSION="${VERSION:-2.7.0}"

echo -e "${BOLD}${CYAN}=== SoundHop v${VERSION} Installer ===${NC}"
echo "Building and installing native Rust binaries..."
echo

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

if ! command_exists cargo; then
    echo -e "${RED}Error: cargo (Rust toolchain) is required to build from source.${NC}"
    exit 1
fi

XDG_DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$XDG_DATA_HOME/applications"
ICON_DIR_SVG="$XDG_DATA_HOME/icons/hicolor/scalable/apps"
ICON_DIR_PNG="$XDG_DATA_HOME/icons/hicolor/256x256/apps"
mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR_SVG" "$ICON_DIR_PNG"

echo -e "${BOLD}Compiling SoundHop v${VERSION} (--release --locked)...${NC}"
(
    cd "$REPO_DIR"
    cargo build --release --workspace --locked
)

TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_DIR/target}"
install -Dm755 "$TARGET_DIR/release/soundhop" "$BIN_DIR/soundhop"
export PATH="$BIN_DIR:$PATH"

if [[ -f "$REPO_DIR/assets/soundhop.svg" ]]; then
    install -Dm644 "$REPO_DIR/assets/soundhop.svg" "$ICON_DIR_SVG/soundhop.svg"
fi
if [[ -f "$REPO_DIR/assets/soundhop.png" ]]; then
    install -Dm644 "$REPO_DIR/assets/soundhop.png" "$ICON_DIR_PNG/soundhop.png"
fi

if [[ -f "$REPO_DIR/packaging/io.github.soundhop.desktop" ]]; then
    ESCAPED_BIN_DIR="$(printf '%s\n' "$BIN_DIR" | sed 's/[\\&|]/\\&/g')"
    sed "s|Exec=soundhop gui|Exec=${ESCAPED_BIN_DIR}/soundhop gui|g" \
        "$REPO_DIR/packaging/io.github.soundhop.desktop" > "$APP_DIR/io.github.soundhop.desktop"
    chmod 644 "$APP_DIR/io.github.soundhop.desktop"
fi

if command_exists systemctl && systemctl --user show-environment >/dev/null 2>&1; then
    echo -e "\n${BOLD}Setting up systemd user service...${NC}"
    "$BIN_DIR/soundhop" service install || true
    echo -e "${GREEN}✓ SoundHop background service enabled and started.${NC}"
fi

echo
echo -e "${GREEN}${BOLD}✓ SoundHop v${VERSION} installation complete!${NC}"
echo -e "  ${CYAN}soundhop${NC}          # Launch interactive TUI dashboard"
echo -e "  ${CYAN}soundhop gui${NC}      # Open SoundHop Control Center"
echo -e "  ${CYAN}soundhop tray${NC}     # Run background system tray"
echo -e "  ${CYAN}soundhop status${NC}   # View status"
