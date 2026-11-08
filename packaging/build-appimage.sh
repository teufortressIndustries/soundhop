#!/usr/bin/env bash
# ==============================================================================
# SoundHop v2.0 AppImage Build Script (Rust Cargo Release Binary)
# Creates a standalone, self-contained SoundHop-<version>-x86_64.AppImage
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
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BOLD}${CYAN}=== Building SoundHop v2.0 Rust AppImage ===${NC}"

if ! command -v cargo >/dev/null 2>&1; then
    echo -e "${RED}Error: cargo (Rust toolchain) is required to build the AppImage.${NC}"
    exit 1
fi

VERSION="${VERSION:-$(grep -oP '^version\s*=\s*"\K[^"]+' "$REPO_DIR/Cargo.toml" | head -n1)}"
UPDATE_REPO="${UPDATE_REPO:-teufortressIndustries/soundhop}"
UPDATE_INFO="gh-releases-zsync|${UPDATE_REPO%/*}|${UPDATE_REPO#*/}|latest|SoundHop-*x86_64.AppImage.zsync"

mkdir -p "$BUILD_DIR" "$OUTPUT_DIR"

# 1. Build optimized Rust release binaries
echo "Compiling Rust workspace in release mode..."
(
    cd "$REPO_DIR"
    cargo build --release --workspace
)

# 2. Ensure official static-pie runtime-x86_64
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

# 3. Ensure appimagetool (extracted to avoid FUSE issues during build)
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
mkdir -p "$APP_DIR/usr/bin"
mkdir -p "$APP_DIR/usr/lib"
mkdir -p "$APP_DIR/usr/share/applications"
mkdir -p "$APP_DIR/usr/share/metainfo"
mkdir -p "$APP_DIR/usr/share/icons/hicolor/scalable/apps"
mkdir -p "$APP_DIR/usr/share/icons/hicolor/256x256/apps"

echo "Populating AppDir with compiled Rust binaries, shared libraries, and desktop metadata..."
cp "$REPO_DIR/packaging/AppRun" "$APP_DIR/AppRun"
chmod +x "$APP_DIR/AppRun"

cp "$REPO_DIR/target/release/soundhop" "$APP_DIR/usr/bin/soundhop"
chmod +x "$APP_DIR/usr/bin/soundhop"

# Bundle optional/non-default shared libraries (libxdo, libayatana-appindicator3, libappindicator3, libdbusmenu)
for libpattern in \
    "libxdo.so*" \
    "libayatana-appindicator3.so*" \
    "libappindicator3.so*" \
    "libayatana-indicator3.so*" \
    "libindicator3.so*" \
    "libayatana-ido3-0.4.so*" \
    "libdbusmenu-gtk3.so*" \
    "libdbusmenu-glib.so*"; do
    find /usr/lib /lib /usr/lib64 /lib64 -name "$libpattern" 2>/dev/null | while read -r libpath; do
        cp -a "$libpath" "$APP_DIR/usr/lib/" 2>/dev/null || true
    done
done

# Invariant 1: Copy io.github.soundhop.desktop ONLY as io.github.soundhop.desktop (never create duplicate .desktop symlinks)
cp "$REPO_DIR/packaging/io.github.soundhop.desktop" "$APP_DIR/io.github.soundhop.desktop"
cp "$REPO_DIR/packaging/io.github.soundhop.desktop" "$APP_DIR/usr/share/applications/io.github.soundhop.desktop"

# Invariant 2: Install AppStream metainfo to usr/share/metainfo/io.github.soundhop.appdata.xml
if [[ -f "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" ]]; then
    cp "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" "$APP_DIR/usr/share/metainfo/io.github.soundhop.appdata.xml"
fi

cp "$REPO_DIR/assets/soundhop.svg" "$APP_DIR/soundhop.svg"
cp "$REPO_DIR/assets/soundhop.svg" "$APP_DIR/usr/share/icons/hicolor/scalable/apps/soundhop.svg"

if [[ -f "$REPO_DIR/assets/soundhop.png" ]]; then
    cp "$REPO_DIR/assets/soundhop.png" "$APP_DIR/soundhop.png"
    cp "$REPO_DIR/assets/soundhop.png" "$APP_DIR/.DirIcon"
    cp "$REPO_DIR/assets/soundhop.png" "$APP_DIR/usr/share/icons/hicolor/256x256/apps/soundhop.png"
else
    ln -sf soundhop.svg "$APP_DIR/.DirIcon"
fi

for dfile in "$APP_DIR/io.github.soundhop.desktop" "$APP_DIR/usr/share/applications/io.github.soundhop.desktop"; do
    if [[ -f "$dfile" ]]; then
        if grep -q '^X-AppImage-Version=' "$dfile"; then
            sed -i "s|^X-AppImage-Version=.*|X-AppImage-Version=$VERSION|" "$dfile"
        else
            echo "X-AppImage-Version=$VERSION" >> "$dfile"
        fi
    fi
done

OUTPUT_FILE="$OUTPUT_DIR/SoundHop-$VERSION-x86_64.AppImage"
rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"
trap 'rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"' ERR INT TERM

echo -e "\n${BOLD}Packaging AppImage...${NC}"
# Invariant 3: Always cd "$OUTPUT_DIR" before running appimagetool so .zsync lands in dist/
(
    cd "$OUTPUT_DIR"
    ARCH=x86_64 "$APPIMAGETOOL_EXEC" -u "$UPDATE_INFO" --runtime-file "$RUNTIME_FILE" "$APP_DIR" "$OUTPUT_FILE"
)

if [[ -f "$OUTPUT_FILE" ]]; then
    chmod +x "$OUTPUT_FILE"
    trap - ERR INT TERM
    echo -e "\n${GREEN}${BOLD}✓ Successfully generated:${NC} $OUTPUT_FILE"
else
    trap - ERR INT TERM
    echo -e "${RED}Error: AppImage generation failed.${NC}"
    exit 1
fi
