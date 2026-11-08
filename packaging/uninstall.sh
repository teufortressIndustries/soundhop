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

echo -e "${BOLD}=== SoundHop Uninstaller ===${NC}"

# 1. Stop and disable systemd service
if command -v systemctl >/dev/null 2>&1 && systemctl --user is-active --quiet soundhop.service 2>/dev/null; then
    echo "Stopping SoundHop systemd user service..."
    systemctl --user stop soundhop.service || true
    systemctl --user disable soundhop.service || true
fi

UNIT_PATH="$HOME/.config/systemd/user/soundhop.service"
if [[ -f "$UNIT_PATH" ]]; then
    rm -f "$UNIT_PATH"
    systemctl --user daemon-reload || true
fi

# 2. Stop any remaining background daemon
if command -v soundhop >/dev/null 2>&1; then
    soundhop stop >/dev/null 2>&1 || true
fi

# 3. Remove desktop launcher & icon
rm -f "$HOME/.local/share/applications/io.github.soundhop.desktop"
rm -f "$HOME/.config/autostart/soundhop-tray.desktop"
rm -f "$HOME/.local/share/icons/hicolor/scalable/apps/soundhop.svg"

# 4. Remove binaries / symlinks
rm -f "$HOME/.local/bin/soundhop"
rm -f "$HOME/.local/bin/shop" # legacy alias cleanup

# 5. Remove venv
INSTALL_DIR="$HOME/.local/share/soundhop"
if [[ -d "$INSTALL_DIR" ]]; then
    echo "Removing installation files at $INSTALL_DIR..."
    rm -rf "$INSTALL_DIR"
fi

# 6. Optional purge of configuration and state
if [[ "${1:-}" == "--purge" ]]; then
    echo "Purging configuration and runtime state..."
    rm -rf "$HOME/.config/soundhop"
    rm -rf "$HOME/.local/state/soundhop"
else
    echo -e "${YELLOW}Configuration preserved at ~/.config/soundhop (use --purge to delete).${NC}"
fi

echo -e "${GREEN}✓ SoundHop successfully uninstalled.${NC}"
