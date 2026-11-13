#!/usr/bin/env bash
# ==============================================================================
# Container Inner Build Script for SoundHop v2.0 AnyLinux AppImage
# Uses pkgforge-dev/Anylinux-AppImages (quick-sharun + uruntime/DwarFS)
# ==============================================================================
set -euo pipefail
export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"

if [[ ! -f /etc/arch-release && -z "${ALLOW_NON_ARCH:-}" ]]; then
    echo "Error: container-build.sh is meant to run inside an Arch Linux container or Arch host."
    echo "Please run ./build-appimage-container.sh or ./packaging/build-appimage.sh."
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
REPO_DIR="${REPO_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)}"
BUILD_DIR="$REPO_DIR/build/appimage-container"
APP_DIR="$BUILD_DIR/AppDir"
OUTPUT_DIR="$REPO_DIR/dist"
QUICK_SHARUN_URL="${QUICK_SHARUN_URL:-https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/refs/heads/main/useful-tools/quick-sharun.sh}"
QUICK_SHARUN="$BUILD_DIR/quick-sharun.sh"

mkdir -p "$BUILD_DIR" "$OUTPUT_DIR"

# Bootstrap Arch Linux build dependencies if running in an un-warmed container
if ! command -v patchelf >/dev/null 2>&1 || ! command -v zsyncmake >/dev/null 2>&1 || ! pkg-config --exists gtk+-3.0 2>/dev/null; then
    echo "0. Installing Arch Linux build dependencies..."
    SUDO=""
    if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    fi
    for i in 1 2 3; do
        $SUDO pacman -Syu --noconfirm --needed \
            base-devel rustup pkgconf wget curl file strace binutils patchelf zsync \
            squashfs-tools desktop-file-utils appstream xorg-server-xvfb \
            gtk3 libayatana-appindicator xdotool libxcb libxkbcommon libxkbcommon-x11 \
            wayland mesa openssl && break || sleep 3
    done
fi

if ! pacman -Q mesa-mini >/dev/null 2>&1 && ! pacman -Q mesa-nano >/dev/null 2>&1; then
    echo "0b. Installing debloated Mesa/GTK packages (no LLVM)..."
    GET_DEBLOATED_URL="${GET_DEBLOATED_URL:-https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/refs/heads/main/useful-tools/get-debloated-pkgs.sh}"
    curl -fsSL "$GET_DEBLOATED_URL" -o "$BUILD_DIR/get-debloated-pkgs.sh"
    chmod +x "$BUILD_DIR/get-debloated-pkgs.sh"
    ${SUDO:-} "$BUILD_DIR/get-debloated-pkgs.sh" --add-common --prefer-nano
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "0. Initializing Rust stable toolchain via rustup..."
    rustup default stable
    export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"
fi

echo "1. Compiling SoundHop Rust workspace (release)..."
CARGO_TARGET_DIR="$BUILD_DIR/cargo-target"
export CARGO_TARGET_DIR
(
    cd "$REPO_DIR"
    cargo build --release --workspace
)

VERSION="${VERSION:-$(grep -oP '^version\s*=\s*"\K[^"]+' "$REPO_DIR/Cargo.toml" | head -n1)}"
UPDATE_REPO="${UPDATE_REPO:-teufortressIndustries/soundhop}"
ARCH="$(uname -m)"

echo "2. Installing SoundHop binary & desktop metadata into /usr..."
SUDO=""
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
fi

$SUDO install -Dm755 "$CARGO_TARGET_DIR/release/soundhop" /usr/bin/soundhop
$SUDO install -Dm644 "$REPO_DIR/packaging/io.github.soundhop.desktop" /usr/share/applications/io.github.soundhop.desktop
if grep -q '^X-AppImage-Version=' /usr/share/applications/io.github.soundhop.desktop; then
    $SUDO sed -i "s|^X-AppImage-Version=.*|X-AppImage-Version=$VERSION|" /usr/share/applications/io.github.soundhop.desktop
else
    echo "X-AppImage-Version=$VERSION" | $SUDO tee -a /usr/share/applications/io.github.soundhop.desktop >/dev/null
fi

if [ -f "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" ]; then
    $SUDO install -Dm644 "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" /usr/share/metainfo/io.github.soundhop.appdata.xml
fi

$SUDO install -Dm644 "$REPO_DIR/assets/soundhop.svg" /usr/share/icons/hicolor/scalable/apps/soundhop.svg
if [ -f "$REPO_DIR/assets/soundhop.png" ]; then
    $SUDO install -Dm644 "$REPO_DIR/assets/soundhop.png" /usr/share/icons/hicolor/256x256/apps/soundhop.png
    ICON_PATH="/usr/share/icons/hicolor/256x256/apps/soundhop.png"
else
    ICON_PATH="/usr/share/icons/hicolor/scalable/apps/soundhop.svg"
fi

echo "3. Fetching quick-sharun.sh..."
if [ ! -x "$QUICK_SHARUN" ]; then
    curl -fsSL "$QUICK_SHARUN_URL" -o "$QUICK_SHARUN"
    chmod +x "$QUICK_SHARUN"
fi

rm -rf "$APP_DIR"
export ARCH
export APPDIR="$APP_DIR"
export OUTPATH="$OUTPUT_DIR"
export OUTNAME="SoundHop-$VERSION-universal-$ARCH.AppImage"
export DESKTOP="/usr/share/applications/io.github.soundhop.desktop"
export ICON="$ICON_PATH"
export UPINFO="gh-releases-zsync|${UPDATE_REPO%/*}|${UPDATE_REPO#*/}|latest|SoundHop-*$ARCH.AppImage.zsync"
export DEPLOY_GTK=1
export DEPLOY_OPENGL=1

OUTPUT_FILE="$OUTPUT_DIR/$OUTNAME"
rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"
trap 'rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"' ERR INT TERM

echo "4. Deploying SoundHop & libraries with quick-sharun..."
# Pass soundhop plus dlopened tray/X11 helper libraries so they are always bundled
"$QUICK_SHARUN" \
    /usr/bin/soundhop \
    /usr/lib/libayatana-appindicator3.so* \
    /usr/lib/libxdo.so*

# Preserve SoundHop's tray/status/CLI dispatch entrypoint via sharun's AppRun.sh hook
install -Dm755 "$REPO_DIR/packaging/AppRun" "$APP_DIR/AppRun.sh"

echo "5. Generating AnyLinux AppImage (uruntime + DwarFS, version: $VERSION)..."
"$QUICK_SHARUN" --make-appimage

if [ -f "$OUTPUT_FILE" ]; then
    chmod +x "$OUTPUT_FILE"
    trap - ERR INT TERM
    echo "✓ Successfully built: dist/$OUTNAME"
else
    trap - ERR INT TERM
    rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"
    echo "Error: AnyLinux AppImage generation failed."
    exit 1
fi
