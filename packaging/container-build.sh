#!/usr/bin/env bash
# ==============================================================================
# Container Inner Build Script for SoundHop v2.0 AnyLinux AppImage
# Uses pkgforge-dev/Anylinux-AppImages (quick-sharun + uruntime/DwarFS)
# ==============================================================================
set -euo pipefail
export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"

if [[ ! -f /etc/arch-release && -z "${ALLOW_NON_ARCH:-}" ]]; then
    echo "Error: container-build.sh is meant to run inside an Arch Linux container or Arch host."
    echo "Please run ./packaging/build-appimage-container.sh or ./packaging/build-appimage.sh."
    exit 1
fi

if [[ ! -f /.dockerenv && ! -f /run/.containerenv && -z "${GITHUB_ACTIONS:-}" && -z "${ALLOW_HOST_MUTATION:-}" ]]; then
    echo "Error: container-build.sh mutates system packages (mesa-nano/gtk3-mini), /usr, and \$HOME/.wgetrc."
    echo "Run ./packaging/build-appimage-container.sh instead, or set ALLOW_HOST_MUTATION=1 if you explicitly want to run on this host."
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
DEBLOATED_BASE_URL="${DEBLOATED_BASE_URL:-https://github.com/pkgforge-dev/archlinux-pkgs-debloated/releases/download/continuous}"
QUICK_SHARUN="$BUILD_DIR/quick-sharun.sh"

# Ensure wget (used internally by quick-sharun.sh) avoids hanging on unrouted container IPv6 or read stalls
if [[ ! -f "$HOME/.wgetrc" ]] || ! grep -q "inet4_only" "$HOME/.wgetrc" 2>/dev/null; then
    printf 'inet4_only = on\ntimeout = 20\ntries = 5\n' >> "$HOME/.wgetrc"
fi

fetch_verified_script() {
    local url="$1"
    local dest="$2"
    local expected_sha256="$3"
    curl --ipv4 --connect-timeout 15 --max-time 180 --proto '=https' --tlsv1.2 --retry 3 --retry-delay 2 -fsSL "$url" -o "$dest"
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
if ! command -v patchelf >/dev/null 2>&1 || \
   ! command -v zsyncmake >/dev/null 2>&1 || \
   ! pkg-config --exists gtk+-3.0 xcursor xi xrandr gstreamer-1.0 gstreamer-audio-1.0 gstreamer-rtp-1.0 gstreamer-app-1.0 2>/dev/null || \
   ! [[ -f /usr/lib/gstreamer-1.0/libgstsrtp.so ]]; then
    echo "0. Installing Arch Linux build dependencies..."
    for i in 1 2 3; do
        $SUDO pacman -Sy --noconfirm --needed archlinux-keyring && \
        $SUDO pacman -Syu --noconfirm --needed \
            base-devel rustup pkgconf wget curl file strace binutils patchelf zsync \
            squashfs-tools desktop-file-utils appstream xorg-server-xvfb \
            gtk3 libayatana-appindicator xdotool libxcb libxcursor libxi libxrandr \
            libxkbcommon libxkbcommon-x11 wayland mesa \
            gstreamer gst-plugins-base-libs gst-plugins-base gst-plugins-good gst-plugins-bad && break || sleep 3
    done
fi

if ! pacman -Q mesa-mini >/dev/null 2>&1 && ! pacman -Q mesa-nano >/dev/null 2>&1; then
    echo "0b. Installing debloated Mesa/GTK3 packages (no LLVM, no Qt6/GTK4/Vulkan)..."
    host_arch="$(uname -m)"
    debloat_dir="$BUILD_DIR/debloated-pkgs"
    rm -rf "$debloat_dir"
    mkdir -p "$debloat_dir"
    for pkg in mesa-nano gtk3-mini libxml2-mini icu-mini glycin-mini; do
        pkg_file="${pkg}-${host_arch}.pkg.tar.zst"
        pkg_url="${DEBLOATED_BASE_URL}/${pkg_file}"
        echo "  -> Downloading ${pkg_file}..."
        curl --ipv4 --connect-timeout 15 --max-time 180 --proto '=https' --tlsv1.2 --retry 3 --retry-delay 2 -fsSL "$pkg_url" -o "$debloat_dir/$pkg_file"
    done
    for i in 1 2 3; do
        $SUDO pacman -U --noconfirm --ask 4 "$debloat_dir"/*.pkg.tar.zst && break || sleep 5
    done
    rm -rf "$debloat_dir"
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

VERSION="${VERSION:-$(awk '
    /^\[workspace\.package\]/ { in_sec=1; next }
    /^\[/ { in_sec=0 }
    in_sec && /^version[[:space:]]*=/ {
        sub(/^[^"]*"/, "")
        sub(/".*$/, "")
        print
        exit
    }
' "$REPO_DIR/Cargo.toml")}"
if [[ -z "$VERSION" || "${VERSION,,}" == "unknown" ]]; then
    echo "Error: Could not determine a valid VERSION (got '${VERSION:-}')."
    exit 1
fi
export VERSION
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
if grep -q '^X-AppImage-UpdateURL=' /usr/share/applications/io.github.soundhop.desktop; then
    $SUDO sed -i "s|^X-AppImage-UpdateURL=.*|X-AppImage-UpdateURL=https://github.com/$UPDATE_REPO|" /usr/share/applications/io.github.soundhop.desktop
else
    echo "X-AppImage-UpdateURL=https://github.com/$UPDATE_REPO" | $SUDO tee -a /usr/share/applications/io.github.soundhop.desktop >/dev/null
fi

if [ -f "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" ]; then
    $SUDO install -Dm644 "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" /usr/share/metainfo/io.github.soundhop.metainfo.xml
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
export UPINFO="gh-releases-zsync|${UPDATE_REPO%/*}|${UPDATE_REPO#*/}|latest|SoundHop-*-universal-${ARCH}.AppImage.zsync"
export DEPLOY_GTK=1
export USE_HOST_DRIVERS="${USE_HOST_DRIVERS:-1}"
EXTRA_GL_LIBS=()
if [ "$USE_HOST_DRIVERS" != "1" ]; then
    export DEPLOY_OPENGL=1
    EXTRA_GL_LIBS=(
        /usr/lib/libGL.so*
        /usr/lib/libEGL.so*
        /usr/lib/libGLX.so*
    )
fi

OUTPUT_FILE="$OUTPUT_DIR/$OUTNAME"
rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"
trap 'rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"' ERR INT TERM

echo "4. Deploying SoundHop & libraries with quick-sharun..."
GST_PLUGIN_DIR="/usr/lib/gstreamer-1.0"
GST_REQUIRED_PLUGINS=(
    libgstcoreelements.so
    libgstaudioconvert.so
    libgstaudioresample.so
    libgstvolume.so
    libgstopus.so
    libgstrtp.so
    libgstrtpmanager.so
    libgstsrtp.so
    libgstudp.so
    libgstpulseaudio.so
    libgstaudiotestsrc.so
    libgstapp.so
)
GST_PLUGIN_PATHS=()
for plugin in "${GST_REQUIRED_PLUGINS[@]}"; do
    plugin_path="$GST_PLUGIN_DIR/$plugin"
    if [[ ! -f "$plugin_path" ]]; then
        echo "Error: Required GStreamer plugin not found in container: $plugin_path" >&2
        exit 1
    fi
    GST_PLUGIN_PATHS+=("$plugin_path")
done

# Pass soundhop plus dlopened tray, X11, Wayland, and minimal GStreamer plugin libraries
# so quick-sharun deploys both the plugins and all of their transitive shared library deps.
"$QUICK_SHARUN" \
    /usr/bin/soundhop \
    /usr/lib/libayatana-appindicator3.so* \
    /usr/lib/libxdo.so* \
    "${EXTRA_GL_LIBS[@]}" \
    /usr/lib/libXcursor.so* \
    /usr/lib/libXi.so* \
    /usr/lib/libXrandr.so* \
    /usr/lib/libxkbcommon-x11.so* \
    /usr/lib/libwayland-egl.so* \
    /usr/lib/libwayland-cursor.so* \
    "${GST_PLUGIN_PATHS[@]}"

# Ensure all required GStreamer plugins are accessible under $APP_DIR/lib/gstreamer-1.0/
# regardless of whether quick-sharun preserved the gstreamer-1.0/ subdirectory or flattened it.
mkdir -p "$APP_DIR/lib/gstreamer-1.0"
for plugin in "${GST_REQUIRED_PLUGINS[@]}"; do
    if [[ -e "$APP_DIR/lib/gstreamer-1.0/$plugin" ]]; then
        continue
    elif [[ -e "$APP_DIR/shared/lib/gstreamer-1.0/$plugin" ]]; then
        ln -sf "../../shared/lib/gstreamer-1.0/$plugin" "$APP_DIR/lib/gstreamer-1.0/$plugin"
    elif [[ -e "$APP_DIR/lib/$plugin" ]]; then
        ln -sf "../$plugin" "$APP_DIR/lib/gstreamer-1.0/$plugin"
    elif [[ -e "$APP_DIR/shared/lib/$plugin" ]]; then
        ln -sf "../../shared/lib/$plugin" "$APP_DIR/lib/gstreamer-1.0/$plugin"
    else
        echo "Error: Required GStreamer plugin was not deployed into AppDir: $plugin" >&2
        exit 1
    fi
done

# Preserve quick-sharun's generated AppRun.sh, AppRun.lib, and path-mapping hooks;
# add SoundHop's GStreamer environment and GUI/status default-dispatch hooks in bin/.
cat << EOF > "$APP_DIR/bin/soundhop-gst.hook"
#!/bin/sh
# Point GStreamer at the AppImage's bundled plugins and disable registry forking
# so cross-distro hosts never load ABI-incompatible system GStreamer plugins.
_sh_appdir="\${APPDIR:-\${CURRENTDIR:-}}"
if [ -n "\$_sh_appdir" ] && [ -d "\$_sh_appdir/lib/gstreamer-1.0" ]; then
    export GST_PLUGIN_SYSTEM_PATH_1_0=""
    export GST_PLUGIN_SYSTEM_PATH=""
    if [ -z "\${GST_PLUGIN_PATH_1_0:-}" ]; then
        export GST_PLUGIN_PATH_1_0="\$_sh_appdir/lib/gstreamer-1.0"
    fi
    export GST_REGISTRY_FORK=no
    if [ -z "\${GST_REGISTRY_1_0:-}" ]; then
        _sh_cache="\${XDG_CACHE_HOME:-\${HOME:-/tmp}/.cache}/soundhop"
        mkdir -p "\$_sh_cache" 2>/dev/null || true
        export GST_REGISTRY_1_0="\$_sh_cache/gst-registry-appimage-${VERSION}.bin"
    fi
fi
EOF
chmod +x "$APP_DIR/bin/soundhop-gst.hook"

cat << 'EOF' > "$APP_DIR/bin/soundhop-dispatch.hook"
#!/bin/sh
# Default to launching the Control Center (which also starts the tray) on graphical
# sessions or status on headless sessions when invoked with no CLI arguments.
if [ $# -eq 0 ]; then
    if [ -n "${WAYLAND_DISPLAY:-}" ] || [ -n "${DISPLAY:-}" ]; then
        set -- gui
    else
        set -- status
    fi
fi
EOF
chmod +x "$APP_DIR/bin/soundhop-dispatch.hook"

# Ship AppStream metainfo and hicolor icons inside AppDir for AppManager / desktop integration
if [ -f "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" ]; then
    install -Dm644 "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" "$APP_DIR/usr/share/metainfo/io.github.soundhop.metainfo.xml"
    install -Dm644 "$REPO_DIR/packaging/io.github.soundhop.metainfo.xml" "$APP_DIR/share/metainfo/io.github.soundhop.metainfo.xml"
fi
install -Dm644 "$REPO_DIR/assets/soundhop.svg" "$APP_DIR/usr/share/icons/hicolor/scalable/apps/soundhop.svg"
install -Dm644 "$REPO_DIR/assets/soundhop.svg" "$APP_DIR/share/icons/hicolor/scalable/apps/soundhop.svg"
if [ -f "$REPO_DIR/assets/soundhop.png" ]; then
    install -Dm644 "$REPO_DIR/assets/soundhop.png" "$APP_DIR/usr/share/icons/hicolor/256x256/apps/soundhop.png"
    install -Dm644 "$REPO_DIR/assets/soundhop.png" "$APP_DIR/share/icons/hicolor/256x256/apps/soundhop.png"
fi

echo "5. Generating AnyLinux AppImage (uruntime + DwarFS, version: $VERSION)..."
"$QUICK_SHARUN" --make-appimage

if [ -f "$OUTPUT_FILE" ]; then
    if [ ! -f "$OUTPUT_FILE.zsync" ]; then
        if [ -f "$REPO_DIR/$OUTNAME.zsync" ]; then
            mv -f "$REPO_DIR/$OUTNAME.zsync" "$OUTPUT_FILE.zsync"
        elif command -v zsyncmake >/dev/null 2>&1; then
            (cd "$OUTPUT_DIR" && zsyncmake -u "$OUTNAME" -o "$OUTNAME.zsync" "$OUTNAME")
        fi
    fi
    chmod +x "$OUTPUT_FILE"
    if [[ "${SKIP_VERIFY:-0}" != "1" ]]; then
        if [[ ! -f "$SCRIPT_DIR/verify-appimage.sh" ]]; then
            echo "Error: Verification script not found: $SCRIPT_DIR/verify-appimage.sh"
            exit 1
        fi
        UPDATE_REPO="$UPDATE_REPO" bash "$SCRIPT_DIR/verify-appimage.sh" "$OUTPUT_FILE" "$VERSION"
    fi
    trap - ERR INT TERM
    echo "✓ Successfully built: dist/$OUTNAME"
else
    trap - ERR INT TERM
    rm -f "$OUTPUT_FILE" "$OUTPUT_FILE.zsync"
    echo "Error: AnyLinux AppImage generation failed."
    exit 1
fi
