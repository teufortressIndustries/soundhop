#!/usr/bin/env bash
# ==============================================================================
# SoundHop AppImage Build Script
# Creates a standalone, self-contained SoundHop-x86_64.AppImage
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$REPO_DIR/build/appimage"
APP_DIR="$BUILD_DIR/AppDir"
OUTPUT_DIR="$REPO_DIR/dist"

BOLD='\033[1m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BOLD}${CYAN}=== Building SoundHop AppImage ===${NC}"

# Check Python 3.10+
if ! command -v python3 >/dev/null 2>&1; then
    echo -e "${RED}Error: python3 is required to build the AppImage.${NC}"
    exit 1
fi

PY_VER="$(python3 -c 'import sys; print(f"python{sys.version_info.major}.{sys.version_info.minor}")')"
echo "Host Python environment: $PY_VER"

mkdir -p "$BUILD_DIR" "$OUTPUT_DIR"

# 1. Ensure official static-pie runtime-x86_64
RUNTIME_FILE="$BUILD_DIR/runtime-x86_64"
if [[ ! -f "$RUNTIME_FILE" || $(stat -c%s "$RUNTIME_FILE" 2>/dev/null || echo 0) -lt 500000 ]]; then
    echo "Downloading official AppImage runtime..."
    RUNTIME_URL="https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64"
    if command -v curl >/dev/null 2>&1; then
        curl -sL "$RUNTIME_URL" -o "$RUNTIME_FILE" || true
    elif command -v wget >/dev/null 2>&1; then
        wget -q "$RUNTIME_URL" -O "$RUNTIME_FILE" || true
    fi
fi

if [[ ! -f "$RUNTIME_FILE" || $(stat -c%s "$RUNTIME_FILE" 2>/dev/null || echo 0) -lt 500000 ]]; then
    echo -e "${RED}Error: Could not retrieve a valid runtime-x86_64 file.${NC}"
    exit 1
fi
chmod +x "$RUNTIME_FILE"

# 2. Ensure appimagetool (extracted to avoid FUSE issues during build)
APPIMAGETOOL_DIR="$BUILD_DIR/appimagetool.extracted"
APPIMAGETOOL_EXEC="$APPIMAGETOOL_DIR/AppRun"

if [[ ! -x "$APPIMAGETOOL_EXEC" ]]; then
    RAW_TOOL="$BUILD_DIR/appimagetool"
    if [[ ! -f "$RAW_TOOL" ]]; then
        echo "Downloading appimagetool..."
        TOOL_URL="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"
        if command -v curl >/dev/null 2>&1; then
            curl -sL "$TOOL_URL" -o "$RAW_TOOL"
        elif command -v wget >/dev/null 2>&1; then
            wget -q "$TOOL_URL" -O "$RAW_TOOL"
        fi
        chmod +x "$RAW_TOOL"
    fi
    echo "Preparing appimagetool..."
    (
        cd "$BUILD_DIR"
        rm -rf appimagetool.extracted squashfs-root
        "$RAW_TOOL" --appimage-extract >/dev/null 2>&1
        mv squashfs-root appimagetool.extracted
    )
fi

# Clean previous AppDir
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR"

# 3. Structure the AppDir
echo "Populating AppDir root files..."
cp "$REPO_DIR/packaging/AppRun" "$APP_DIR/AppRun"
chmod +x "$APP_DIR/AppRun"

cp "$REPO_DIR/packaging/io.github.soundhop.desktop" "$APP_DIR/io.github.soundhop.desktop"
ln -sf io.github.soundhop.desktop "$APP_DIR/soundhop.desktop"
cp "$REPO_DIR/src/soundhop/assets/soundhop.svg" "$APP_DIR/soundhop.svg"

# Generate PNG icon for .DirIcon if missing
PNG_ICON="$REPO_DIR/src/soundhop/assets/soundhop.png"
if [[ ! -f "$PNG_ICON" ]]; then
    python3 -c "
from PySide6.QtGui import QImage, QPainter
from PySide6.QtSvg import QSvgRenderer
renderer = QSvgRenderer('$REPO_DIR/src/soundhop/assets/soundhop.svg')
image = QImage(256, 256, QImage.Format_ARGB32)
image.fill(0)
painter = QPainter(image)
renderer.render(painter)
painter.end()
image.save('$PNG_ICON')
" 2>/dev/null || true
fi

if [[ -f "$PNG_ICON" ]]; then
    cp "$PNG_ICON" "$APP_DIR/soundhop.png"
    ln -sf soundhop.png "$APP_DIR/.DirIcon"
else
    ln -sf soundhop.svg "$APP_DIR/.DirIcon"
fi

mkdir -p "$APP_DIR/usr/bin"
mkdir -p "$APP_DIR/usr/lib/$PY_VER/site-packages"
mkdir -p "$APP_DIR/usr/lib64/$PY_VER/site-packages"
mkdir -p "$APP_DIR/usr/share/applications"
mkdir -p "$APP_DIR/usr/share/icons/hicolor/scalable/apps"
mkdir -p "$APP_DIR/usr/share/icons/hicolor/256x256/apps"

cp "$REPO_DIR/packaging/io.github.soundhop.desktop" "$APP_DIR/usr/share/applications/io.github.soundhop.desktop"
ln -sf io.github.soundhop.desktop "$APP_DIR/usr/share/applications/soundhop.desktop"
cp "$REPO_DIR/src/soundhop/assets/soundhop.svg" "$APP_DIR/usr/share/icons/hicolor/scalable/apps/soundhop.svg"
if [[ -f "$PNG_ICON" ]]; then
    cp "$PNG_ICON" "$APP_DIR/usr/share/icons/hicolor/256x256/apps/soundhop.png"
fi

# 4. Bundle Python Environment into AppDir
echo "Setting up Python environment inside AppDir..."
# Create venv with system site-packages so it does not block system libraries
python3 -m venv --system-site-packages "$APP_DIR/usr"

# Install SoundHop directly without compiling heavy packages from source
echo "Installing SoundHop into AppDir..."
"$APP_DIR/usr/bin/python3" -m pip install --no-deps "$REPO_DIR" >/dev/null 2>&1 || {
    cp -r "$REPO_DIR/src/soundhop" "$APP_DIR/usr/lib/$PY_VER/site-packages/"
}

# 5. Bundle PySide6 and shiboken6 into AppDir
echo "Bundling PySide6 Qt GUI libraries..."
python3 -c "
import os, sys, shutil
try:
    import PySide6
    src_pyside = os.path.dirname(PySide6.__file__)
    dst_pyside = '$APP_DIR/usr/lib/$PY_VER/site-packages/PySide6'
    if not os.path.exists(dst_pyside) and os.path.exists(src_pyside):
        shutil.copytree(src_pyside, dst_pyside, dirs_exist_ok=True, symlinks=True, ignore_dangling_symlinks=True)
        print('  Bundled PySide6 package')
except Exception as e:
    print('  Notice: PySide6 bundling:', e)

try:
    import shiboken6
    src_shiboken = os.path.dirname(shiboken6.__file__)
    dst_shiboken = '$APP_DIR/usr/lib/$PY_VER/site-packages/shiboken6'
    if not os.path.exists(dst_shiboken) and os.path.exists(src_shiboken):
        shutil.copytree(src_shiboken, dst_shiboken, dirs_exist_ok=True)
        print('  Bundled shiboken6 package')
except Exception as e:
    pass
"

# Copy C shared libraries for PySide6 and shiboken6
mkdir -p "$APP_DIR/usr/lib64"
for lib in /lib64/libpyside6* /lib64/libshiboken6* /usr/lib64/libpyside6* /usr/lib64/libshiboken6*; do
    if [[ -f "$lib" || -L "$lib" ]]; then
        cp -a "$lib" "$APP_DIR/usr/lib64/" 2>/dev/null || true
    fi
done

# Try installing pure-python zeroconf if missing
if ! "$APP_DIR/usr/bin/python3" -c "import zeroconf" >/dev/null 2>&1; then
    echo "Installing zeroconf (mDNS)..."
    "$APP_DIR/usr/bin/python3" -m pip install --no-warn-script-location zeroconf >/dev/null 2>&1 || true
fi

# Ensure python binary symlink
ln -sf python3 "$APP_DIR/usr/bin/python"

# 6. Generate the AppImage
VERSION="${VERSION:-$(grep -oP 'VERSION\s*=\s*"\K[^"]+' "$REPO_DIR/src/soundhop/constants.py" 2>/dev/null || python3 -c "import sys; sys.path.insert(0, '$REPO_DIR/src'); from soundhop.constants import VERSION; print(VERSION)")}"
OUTPUT_FILE="$OUTPUT_DIR/SoundHop-$VERSION-x86_64.AppImage"
TMP_OUTPUT="$OUTPUT_DIR/.SoundHop-$VERSION-x86_64.AppImage.tmp"
rm -f "$TMP_OUTPUT"

echo -e "\n${BOLD}Packaging AppImage...${NC}"
ARCH=x86_64 "$APPIMAGETOOL_EXEC" --runtime-file "$RUNTIME_FILE" "$APP_DIR" "$TMP_OUTPUT"

if [[ -f "$TMP_OUTPUT" ]]; then
    chmod +x "$TMP_OUTPUT"
    mv -f "$TMP_OUTPUT" "$OUTPUT_FILE"
    echo -e "\n${GREEN}${BOLD}✓ Successfully generated:${NC} $OUTPUT_FILE"
    echo -e "You can run it directly: ${CYAN}$OUTPUT_FILE${NC}"
    echo -e "Or double-click it on any Linux desktop to launch the system tray."
else
    echo -e "${RED}Error: AppImage generation failed.${NC}"
    exit 1
fi
