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
ANYLINUX_COMMIT="${ANYLINUX_COMMIT:-f7e9cb26818bdeb3d64a0fe055fdd51cbc887a81}"
QUICK_SHARUN_URL="${QUICK_SHARUN_URL:-https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/${ANYLINUX_COMMIT}/useful-tools/quick-sharun.sh}"
QUICK_SHARUN_SHA256="${QUICK_SHARUN_SHA256:-0aea2f589658e5db1659a6ccb8d29546c4e03028f1e29b37c6450cece917249a}"
GET_DEBLOATED_URL="${GET_DEBLOATED_URL:-https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/${ANYLINUX_COMMIT}/useful-tools/get-debloated-pkgs.sh}"
GET_DEBLOATED_SHA256="${GET_DEBLOATED_SHA256:-d215f7676785f1a1bb8784e69085d66c1df81488ef31f1ba7198325a5be05130}"
QUICK_SHARUN="$BUILD_DIR/quick-sharun.sh"

fetch_verified_script() {
    local url="$1"
    local dest="$2"
    local expected_sha256="$3"
    curl --proto '=https' --tlsv1.2 --retry 3 --retry-delay 2 -fsSL "$url" -o "$dest"
    if [[ -n "$expected_sha256" && "$expected_sha256" != "skip" ]]; then
        printf '%s  %s\n' "$expected_sha256" "$dest" | sha256sum -c -
    fi
    chmod +x "$dest"
}

mkdir -p "$BUILD_DIR" "$OUTPUT_DIR"

SUDO=""
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
fi

# Bootstrap Arch Linux build dependencies if running in an un-warmed container
if ! command -v patchelf >/dev/null 2>&1 || ! command -v zsyncmake >/dev/null 2>&1 || ! pkg-config --exists gtk+-3.0 xcursor xi xrandr 2>/dev/null; then
    echo "0. Installing Arch Linux build dependencies..."
    for i in 1 2 3; do
        $SUDO pacman -Sy --noconfirm --needed archlinux-keyring && \
        $SUDO pacman -Syu --noconfirm --needed \
            base-devel rustup pkgconf wget curl file strace binutils patchelf zsync \
            squashfs-tools desktop-file-utils appstream xorg-server-xvfb \
            gtk3 libayatana-appindicator xdotool libxcb libxcursor libxi libxrandr \
            libxkbcommon libxkbcommon-x11 wayland mesa openssl && break || sleep 3
    done
fi

if ! pacman -Q mesa-mini >/dev/null 2>&1 && ! pacman -Q mesa-nano >/dev/null 2>&1; then
    echo "0b. Installing debloated Mesa/GTK3 packages (no LLVM, no Qt6/GTK4/Vulkan)..."
    fetch_verified_script "$GET_DEBLOATED_URL" "$BUILD_DIR/get-debloated-pkgs.sh" "$GET_DEBLOATED_SHA256"
    ${SUDO:-} "$BUILD_DIR/get-debloated-pkgs.sh" --add-opengl --prefer-nano \
        gtk3-mini libxml2-mini icu-mini glycin-mini
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
    cargo build --release --workspace --locked
)

VERSION="${VERSION:-$(grep -oP '^version\s*=\s*"\K[^"]+' "$REPO_DIR/Cargo.toml" | head -n1)}"
UPDATE_REPO="${UPDATE_REPO:-teufortressIndustries/soundhop}"
ARCH="$(uname -m)"

echo "2. Installing SoundHop binary & desktop metadata into /usr..."
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
if [ ! -x "$QUICK_SHARUN" ] || { [[ "$QUICK_SHARUN_SHA256" != "skip" ]] && ! printf '%s  %s\n' "$QUICK_SHARUN_SHA256" "$QUICK_SHARUN" | sha256sum -c - >/dev/null 2>&1; }; then
    fetch_verified_script "$QUICK_SHARUN_URL" "$QUICK_SHARUN" "$QUICK_SHARUN_SHA256"
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
export USE_HOST_DRIVERS="${USE_HOST_DRIVERS:-1}"
if [ "$USE_HOST_DRIVERS" != "1" ]; then
    export DEPLOY_OPENGL=1
fi
export STRACE_FLAGS="--settings-dialog"

OUTPUT_FILE="$OUTPUT_DIR/$OUTNAME"
rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"
trap 'rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"' ERR INT TERM

echo "4. Deploying SoundHop & libraries with quick-sharun..."
# Pass soundhop plus dlopened tray, X11, Wayland, and GLVND helper libraries so they are always bundled
"$QUICK_SHARUN" \
    /usr/bin/soundhop \
    /usr/lib/libayatana-appindicator3.so* \
    /usr/lib/libxdo.so* \
    /usr/lib/libGL.so* \
    /usr/lib/libEGL.so* \
    /usr/lib/libGLX.so* \
    /usr/lib/libXcursor.so* \
    /usr/lib/libXi.so* \
    /usr/lib/libXrandr.so* \
    /usr/lib/libxkbcommon-x11.so* \
    /usr/lib/libwayland-egl.so* \
    /usr/lib/libwayland-cursor.so*

# Preserve quick-sharun's generated AppRun.sh, AppRun.lib, and path-mapping hooks;
# add SoundHop's tray/status default-dispatch logic as a hook in bin/.
cat << 'EOF' > "$APP_DIR/bin/soundhop-dispatch.hook"
#!/bin/sh
# Default to launching tray on graphical sessions or status on headless sessions
# when the AppImage is invoked with no CLI arguments.
if [ $# -eq 0 ]; then
    if [ -n "${WAYLAND_DISPLAY:-}" ] || [ -n "${DISPLAY:-}" ]; then
        set -- tray
    else
        set -- status
    fi
fi
EOF
chmod +x "$APP_DIR/bin/soundhop-dispatch.hook"

echo "5. Generating AnyLinux AppImage (uruntime + DwarFS, version: $VERSION)..."
"$QUICK_SHARUN" --make-appimage

if [ -f "$OUTPUT_FILE" ]; then
    if [ ! -f "$OUTPUT_FILE.zsync" ]; then
        if [ -f "$REPO_DIR/$OUTNAME.zsync" ]; then
            mv -f "$REPO_DIR/$OUTNAME.zsync" "$OUTPUT_FILE.zsync"
        elif command -v zsyncmake >/dev/null 2>&1; then
            (cd "$OUTPUT_DIR" && zsyncmake "$OUTNAME")
        fi
    fi
    chmod +x "$OUTPUT_FILE"
    trap - ERR INT TERM
    echo "✓ Successfully built: dist/$OUTNAME"
else
    trap - ERR INT TERM
    rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"
    echo "Error: AnyLinux AppImage generation failed."
    exit 1
fi
