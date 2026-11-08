#!/usr/bin/env bash
# ==============================================================================
# Container Inner Build Script for SoundHop v2.0 (Rust Cargo Release Binary)
# ==============================================================================
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"

if [[ ! -f /.dockerenv && ! -f /run/.containerenv && ! -f /etc/debian_version && -z "${CI:-}" ]]; then
    echo "Error: container-build.sh is meant to run inside the Ubuntu container or CI."
    echo "Please run ./build-appimage-container.sh or ./packaging/build-appimage.sh."
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
REPO_DIR="${REPO_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)}"
BUILD_DIR="$REPO_DIR/build/appimage-container"
APP_DIR="$BUILD_DIR/AppDir"
OUTPUT_DIR="$REPO_DIR/dist"

mkdir -p "$BUILD_DIR" "$OUTPUT_DIR"

# Bootstrap system libraries and Rust toolchain if running inside an un-warmed ubuntu:22.04 container
if ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists gtk+-3.0 2>/dev/null || ! command -v zsyncmake >/dev/null 2>&1; then
    echo "0. Installing system build dependencies inside container..."
    SUDO=""
    if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    fi
    $SUDO apt-get update -qq
    $SUDO apt-get install -y -qq --no-install-recommends \
        build-essential pkg-config wget curl ca-certificates file \
        libgtk-3-dev libayatana-appindicator3-dev libxdo-dev \
        libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
        libgl1-mesa-dev libssl-dev zstd zsync binutils squashfs-tools appstream
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "0. Installing Rust stable toolchain via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
    export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"
fi

echo "1. Compiling SoundHop Rust workspace (release)..."
# Use a dedicated target dir for the Ubuntu 22.04 container so host Fedora glibc 2.40 artifacts don't conflict with container glibc 2.35
CARGO_TARGET_DIR="$BUILD_DIR/cargo-target"
export CARGO_TARGET_DIR
(
    cd "$REPO_DIR"
    cargo build --release --workspace
)

VERSION="${VERSION:-$(grep -oP '^version\s*=\s*"\K[^"]+' "$REPO_DIR/Cargo.toml" | head -n1)}"
UPDATE_REPO="${UPDATE_REPO:-teufortressIndustries/soundhop}"
UPDATE_INFO="gh-releases-zsync|${UPDATE_REPO%/*}|${UPDATE_REPO#*/}|latest|SoundHop-*x86_64.AppImage.zsync"

rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/usr/bin"
mkdir -p "$APP_DIR/usr/lib"
mkdir -p "$APP_DIR/usr/share/applications"
mkdir -p "$APP_DIR/usr/share/icons/hicolor/scalable/apps"
mkdir -p "$APP_DIR/usr/share/icons/hicolor/256x256/apps"
mkdir -p "$APP_DIR/usr/share/metainfo"

echo "2. Populating AppDir binaries, non-standard shared libraries & metadata..."
cp "$REPO_DIR/packaging/AppRun" "$APP_DIR/AppRun"
chmod +x "$APP_DIR/AppRun"

cp "$CARGO_TARGET_DIR/release/soundhop" "$APP_DIR/usr/bin/soundhop"
chmod +x "$APP_DIR/usr/bin/soundhop"

# Bundle optional/non-default shared libraries (libxdo, libayatana-appindicator3, libappindicator3, libdbusmenu)
# so target machines do not need xdotool or appindicator packages installed.
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

strip --strip-unneeded "$APP_DIR/usr/bin/soundhop" "$APP_DIR/usr/lib/"*.so* 2>/dev/null || true

# Invariant 1: No duplicate .desktop symlinks
cp "$REPO_DIR/packaging/io.github.soundhop.desktop" "$APP_DIR/io.github.soundhop.desktop"
cp "$REPO_DIR/packaging/io.github.soundhop.desktop" "$APP_DIR/usr/share/applications/io.github.soundhop.desktop"

# Invariant 2: AppStream metainfo placed at io.github.soundhop.appdata.xml
if [ -f "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" ]; then
    cp "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" "$APP_DIR/usr/share/metainfo/io.github.soundhop.appdata.xml"
fi

cp "$REPO_DIR/assets/soundhop.svg" "$APP_DIR/soundhop.svg"
cp "$REPO_DIR/assets/soundhop.svg" "$APP_DIR/usr/share/icons/hicolor/scalable/apps/soundhop.svg"
if [ -f "$REPO_DIR/assets/soundhop.png" ]; then
    cp "$REPO_DIR/assets/soundhop.png" "$APP_DIR/soundhop.png"
    cp "$REPO_DIR/assets/soundhop.png" "$APP_DIR/.DirIcon"
    cp "$REPO_DIR/assets/soundhop.png" "$APP_DIR/usr/share/icons/hicolor/256x256/apps/soundhop.png"
else
    ln -sf soundhop.svg "$APP_DIR/.DirIcon"
fi

for dfile in "$APP_DIR/io.github.soundhop.desktop" "$APP_DIR/usr/share/applications/io.github.soundhop.desktop"; do
    if [ -f "$dfile" ]; then
        if grep -q '^X-AppImage-Version=' "$dfile"; then
            sed -i "s|^X-AppImage-Version=.*|X-AppImage-Version=$VERSION|" "$dfile"
        else
            echo "X-AppImage-Version=$VERSION" >> "$dfile"
        fi
    fi
done

echo "3. Ensuring official runtime & appimagetool..."
cd "$BUILD_DIR"
if [ ! -f "runtime-x86_64" ]; then
    wget -q https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64
    chmod +x runtime-x86_64
fi

if [ ! -f "appimagetool" ]; then
    wget -q https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage -O appimagetool
    chmod +x appimagetool
    ./appimagetool --appimage-extract >/dev/null 2>&1
fi

OUTPUT_FILE="$OUTPUT_DIR/SoundHop-$VERSION-universal-x86_64.AppImage"
echo "4. Generating universal AppImage (version: $VERSION)..."
rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"
trap 'rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"' ERR INT TERM

# Invariant 3: cd "$OUTPUT_DIR" before running appimagetool
cd "$OUTPUT_DIR"
ARCH=x86_64 "$BUILD_DIR/squashfs-root/AppRun" -u "$UPDATE_INFO" --runtime-file "$BUILD_DIR/runtime-x86_64" "$APP_DIR" "$OUTPUT_FILE"
if [ -f "$OUTPUT_FILE" ]; then
    chmod +x "$OUTPUT_FILE"
    trap - ERR INT TERM
    echo "✓ Successfully built: dist/SoundHop-$VERSION-universal-x86_64.AppImage"
else
    trap - ERR INT TERM
    rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"
    echo "Error: AppImage generation failed."
    exit 1
fi
