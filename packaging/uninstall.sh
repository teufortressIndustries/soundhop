#!/usr/bin/env bash
# ==============================================================================
# SoundHop Uninstaller
# ==============================================================================
set -euo pipefail

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m'

XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
XDG_DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
XDG_STATE_HOME="${XDG_STATE_HOME:-$HOME/.local/state}"

echo -e "${BOLD}=== SoundHop Uninstaller ===${NC}"

UNIT_PATH="$XDG_CONFIG_HOME/systemd/user/soundhop.service"

# 1. Stop and disable systemd service (even if enabled but currently inactive)
if command -v systemctl >/dev/null 2>&1; then
    if systemctl --user is-active --quiet soundhop.service 2>/dev/null; then
        echo "Stopping SoundHop systemd user service..."
        systemctl --user stop soundhop.service || true
    fi
    if systemctl --user is-enabled --quiet soundhop.service 2>/dev/null || [[ -f "$UNIT_PATH" ]]; then
        echo "Disabling SoundHop systemd user service..."
        systemctl --user disable soundhop.service 2>/dev/null || true
    fi
fi

if [[ -f "$UNIT_PATH" ]]; then
    rm -f "$UNIT_PATH"
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload 2>/dev/null || true
    fi
fi

# 2. Stop any remaining background daemon
if command -v soundhop >/dev/null 2>&1; then
    soundhop stop >/dev/null 2>&1 || true
elif [[ -x "$HOME/.local/bin/soundhop" ]]; then
    "$HOME/.local/bin/soundhop" stop >/dev/null 2>&1 || true
fi

# 3. Remove desktop launcher & icon
rm -f "$XDG_DATA_HOME/applications/io.github.soundhop.desktop"
rm -f "$XDG_CONFIG_HOME/autostart/soundhop-tray.desktop"
rm -f "$XDG_DATA_HOME/icons/hicolor/scalable/apps/soundhop.svg"
rm -f "$XDG_DATA_HOME/icons/hicolor/256x256/apps/soundhop.png"

# 4. Remove binaries / symlinks
rm -f "$HOME/.local/bin/soundhop"
rm -f "$HOME/.local/bin/shop" # legacy alias cleanup

# 5. Remove legacy/shared data directory
INSTALL_DIR="$XDG_DATA_HOME/soundhop"
if [[ -d "$INSTALL_DIR" ]]; then
    echo "Removing installation files at $INSTALL_DIR..."
    rm -rf "$INSTALL_DIR"
fi

# 6. Optional purge of configuration and state
if [[ "${1:-}" == "--purge" ]]; then
    echo "Purging configuration and runtime state..."
    rm -rf "$XDG_CONFIG_HOME/soundhop"
    rm -rf "$XDG_STATE_HOME/soundhop"
else
    echo -e "${YELLOW}Configuration preserved at $XDG_CONFIG_HOME/soundhop (use --purge to delete).${NC}"
fi

echo -e "${GREEN}✓ SoundHop successfully uninstalled.${NC}"
