#!/usr/bin/env bash
# ==============================================================================
# SoundHop Turnkey Cross-Distro Installer
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
NC='\033[0m' # No Color

echo -e "${BOLD}${CYAN}=== SoundHop Installer ===${NC}"
echo "Setting up seamless local audio sharing..."
echo

# 1. Distro Detection & Dependency Check
DISTRO="unknown"
if [[ -f /etc/os-release ]]; then
    # shellcheck disable=SC1091
    source /etc/os-release
    DISTRO="${ID:-unknown}"
fi

echo -e "Detected OS: ${BOLD}${NAME:-$DISTRO}${NC}"

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

# Check Python 3.10+
if ! command_exists python3; then
    echo -e "${RED}Error: python3 is required but not installed.${NC}"
    exit 1
fi

PY_VER=$(python3 -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')
PY_OK=$(python3 -c 'import sys; print(int(sys.version_info >= (3, 10)))')
if [[ "$PY_OK" != "1" ]]; then
    echo -e "${RED}Error: Python 3.10 or newer is required (found Python $PY_VER).${NC}"
    exit 1
fi

# Check for pactl (PipeWire / PulseAudio)
if ! command_exists pactl; then
    echo -e "${YELLOW}Warning: 'pactl' was not found on your system.${NC}"
    case "$DISTRO" in
        fedora)
            echo "You can install it with: sudo dnf install pipewire-pulseaudio"
            ;;
        cachyos|arch|manjaro)
            echo "You can install it with: sudo pacman -S pipewire-pulse"
            ;;
        ubuntu|debian|pop)
            echo "You can install it with: sudo apt install pipewire-pulse pulseaudio-utils"
            ;;
        opensuse*)
            echo "You can install it with: sudo zypper install pipewire-pulseaudio"
            ;;
    esac
fi

# 2. Virtual environment / installation target
INSTALL_DIR="$HOME/.local/share/soundhop"
VENV_DIR="$INSTALL_DIR/venv"
BIN_DIR="$HOME/.local/bin"

mkdir -p "$INSTALL_DIR" "$BIN_DIR"

echo -e "\n${BOLD}Installing SoundHop into $VENV_DIR...${NC}"
if [[ ! -d "$VENV_DIR" ]]; then
    python3 -m venv "$VENV_DIR"
fi

# Install with pip into venv
"$VENV_DIR/bin/pip" install --upgrade pip >/dev/null 2>&1 || true
echo "Installing package and extras (mDNS, GUI)..."
if ! "$VENV_DIR/bin/pip" install -e "$REPO_DIR[all]" >/dev/null 2>&1; then
    echo "Optional extras installation had notices; installing core package..."
    "$VENV_DIR/bin/pip" install -e "$REPO_DIR"
fi

# Create symlinks in ~/.local/bin
ln -sf "$VENV_DIR/bin/soundhop" "$BIN_DIR/soundhop"
ln -sf "$VENV_DIR/bin/shop" "$BIN_DIR/shop"

# Ensure ~/.local/bin is in PATH for this session if not already
export PATH="$BIN_DIR:$PATH"

# 3. Desktop Entry and Icon Installation
APP_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
mkdir -p "$APP_DIR" "$ICON_DIR"

if [[ -f "$REPO_DIR/src/soundhop/assets/soundhop.svg" ]]; then
    cp "$REPO_DIR/src/soundhop/assets/soundhop.svg" "$ICON_DIR/soundhop.svg"
fi

if [[ -f "$REPO_DIR/packaging/io.github.soundhop.desktop" ]]; then
    sed "s|Exec=soundhop tray|Exec=$BIN_DIR/soundhop tray|g" "$REPO_DIR/packaging/io.github.soundhop.desktop" > "$APP_DIR/io.github.soundhop.desktop"
    chmod 644 "$APP_DIR/io.github.soundhop.desktop"
    ln -sf io.github.soundhop.desktop "$APP_DIR/soundhop.desktop"
fi

# 4. User Systemd Service Installation
if command_exists systemctl && systemctl --user show-environment >/dev/null 2>&1; then
    echo -e "\n${BOLD}Setting up systemd user service...${NC}"
    "$BIN_DIR/soundhop" service install --no-start || true
    systemctl --user enable --now soundhop.service || true
    echo -e "${GREEN}✓ SoundHop background service enabled and started.${NC}"
else
    echo -e "\n${YELLOW}systemd user service unavailable; starting daemon in background...${NC}"
    "$BIN_DIR/soundhop" daemon >/dev/null 2>&1 &
fi

# 5. Firewall Configuration
echo -e "\n${BOLD}Firewall Check...${NC}"
"$BIN_DIR/soundhop" firewall status || true

echo
echo -e "${GREEN}${BOLD}✓ Installation complete!${NC}"
echo
echo -e "${BOLD}Quick Usage:${NC}"
echo -e "  ${CYAN}soundhop share on${NC}       # Enable audio receiver on this device"
echo -e "  ${CYAN}soundhop status${NC}         # View network devices and connection status"
echo -e "  ${CYAN}soundhop connect${NC}        # Interactively connect to any discovered machine"
echo -e "  ${CYAN}soundhop tray${NC}           # Launch the system tray icon (KDE / GNOME)"
echo -e "  ${CYAN}soundhop doctor${NC}         # Run diagnostics"
echo
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo -e "${YELLOW}Note: Add ~/.local/bin to your PATH in ~/.bashrc or ~/.config/fish/config.fish${NC}"
    echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
fi
