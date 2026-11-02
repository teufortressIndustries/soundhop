#!/usr/bin/env bash
# ==============================================================================
# Container Inner Build Script (Runs inside Ubuntu 22.04)
# ==============================================================================
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

if [[ ! -f /.dockerenv && ! -f /run/.containerenv && ! -f /etc/debian_version && -z "${CI:-}" ]]; then
    echo "Error: container-build.sh is meant to run inside the Ubuntu container."
    echo "Please run the container launcher instead:"
    echo "  ./build-appimage-container.sh"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
REPO_DIR="${REPO_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)}"
BUILD_DIR="$REPO_DIR/build/appimage-container"
APP_DIR="$BUILD_DIR/AppDir"
OUTPUT_DIR="$REPO_DIR/dist"
BASE_VENV="$BUILD_DIR/cached-venv"
export APP_DIR

mkdir -p "$BUILD_DIR" "$OUTPUT_DIR"

if ! command -v python3 >/dev/null 2>&1 || ! command -v zstd >/dev/null 2>&1 || ! command -v strip >/dev/null 2>&1; then
    echo "1. Installing base dependencies inside container..."
    SUDO=""
    if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
    fi
    $SUDO apt-get update -qq
    $SUDO apt-get install -y -qq --no-install-recommends \
        python3 python3-pip python3-venv wget curl ca-certificates file libgl1 libegl1 libxkbcommon0 libpulse0 zstd binutils
else
    echo "1. Base system dependencies already present."
fi

echo "2. Preparing Python environment..."
if [ ! -d "$BASE_VENV" ] || [ ! -f "$BASE_VENV/bin/python3" ]; then
    echo "  Creating cached Python runtime with PySide6 (one-time download)..."
    rm -rf "$BASE_VENV"
    python3 -m venv "$BASE_VENV"
    "$BASE_VENV/bin/pip" install --upgrade pip setuptools wheel
    "$BASE_VENV/bin/pip" install "PySide6-Essentials>=6.5" zeroconf
else
    echo "  Using cached Python runtime (skipping wheel downloads)."
fi

rm -rf "$APP_DIR"
mkdir -p "$APP_DIR"
echo "  Deploying Python runtime to AppDir..."
cp -a "$BASE_VENV" "$APP_DIR/usr"

echo "3. Installing current SoundHop package..."
"$APP_DIR/usr/bin/pip" install --no-cache-dir --no-deps "$REPO_DIR"

echo "4. Optimizing and tree-shaking PySide6..."
python3 - << 'PYEOF'
import os
import glob
import shutil

keep_modules = {"QtCore", "QtGui", "QtWidgets", "QtSvg", "QtSvgWidgets"}
app_dir = os.environ.get("APP_DIR", "/work/build/appimage-container/AppDir")
pyside_matches = glob.glob(f"{app_dir}/usr/lib*/python3*/site-packages/PySide6")
if pyside_matches:
    pyside_dir = pyside_matches[0]

    # Remove unused Python bindings (.so, .pyi)
    for f in os.listdir(pyside_dir):
        if f == "Qt":
            continue  # NEVER delete the Qt directory! That holds the core libraries and plugins.
        if f.startswith("Qt") and not any(f.startswith(k + ".") or f == k for k in keep_modules):
            path = os.path.join(pyside_dir, f)
            if os.path.isfile(path) or os.path.islink(path):
                os.remove(path)
            elif os.path.isdir(path):
                shutil.rmtree(path, ignore_errors=True)

    # Prune explicitly unused heavy Qt shared libraries (preserving OpenGL, Wayland, Xcb, DBus, etc.)
    qt_lib_dir = os.path.join(pyside_dir, "Qt", "lib")
    if os.path.exists(qt_lib_dir):
        remove_prefixes = (
            "libQt6Qml",
            "libQt6Quick",
            "libQt6Pdf",
            "libQt6Sql",
            "libQt6Designer",
            "libQt6Help",
            "libQt6UiTools",
            "libQt6Test",
            "libQt6Labs",
            "libQt6Lottie",
        )
        for f in os.listdir(qt_lib_dir):
            if any(f.startswith(prefix) for prefix in remove_prefixes):
                p = os.path.join(qt_lib_dir, f)
                if os.path.isfile(p) or os.path.islink(p):
                    os.remove(p)

    # Prune unused Qt plugins, keeping platforms, wayland, xcb, and display themes
    plugins_dir = os.path.join(pyside_dir, "Qt", "plugins")
    if os.path.exists(plugins_dir):
        keep_plugins = {
            "platforms",
            "wayland-shell-integration",
            "wayland-decoration-client",
            "wayland-graphics-integration-client",
            "iconengines",
            "imageformats",
            "platformthemes",
            "xcbglintegrations",
            "generic",
            "egldeviceintegrations",
        }
        for d in os.listdir(plugins_dir):
            if d not in keep_plugins:
                shutil.rmtree(os.path.join(plugins_dir, d), ignore_errors=True)

    # Remove translations, QML, docs, headers, and type stubs (.pyi)
    for unused in ["qml", "translations", "include", "doc", "examples", "glue", "typesystems"]:
        shutil.rmtree(os.path.join(pyside_dir, "Qt", unused), ignore_errors=True)
PYEOF

# Remove pip, setuptools, wheel build tools from final bundle
rm -rf "$APP_DIR/usr/lib"/python3*/site-packages/{pip,setuptools,wheel,_distutils_hack}
find "$APP_DIR/usr" -type d -name "__pycache__" -exec rm -rf {} + 2>/dev/null || true
find "$APP_DIR/usr" -type f \( -name "*.pyc" -o -name "*.pyi" \) -delete 2>/dev/null || true

# Wire up qt.conf and library/plugin symlinks in AppDir
mkdir -p "$APP_DIR/usr/bin" "$APP_DIR/usr/lib" "$APP_DIR/usr/plugins"
cat << 'QTEOF' > "$APP_DIR/usr/bin/qt.conf"
[Paths]
Prefix = ..
Libraries = lib
Plugins = plugins
QTEOF
cp "$APP_DIR/usr/bin/qt.conf" "$APP_DIR/qt.conf"

for pdir in "$APP_DIR"/usr/lib*/python3*/site-packages/PySide6; do
    if [ -d "$pdir/Qt/lib" ]; then
        cp -as "$pdir"/Qt/lib/* "$APP_DIR/usr/lib/" 2>/dev/null || true
    fi
    if [ -d "$pdir/Qt/plugins" ]; then
        cp -as "$pdir"/Qt/plugins/* "$APP_DIR/usr/plugins/" 2>/dev/null || true
    fi
    cp "$APP_DIR/usr/bin/qt.conf" "$pdir/qt.conf" 2>/dev/null || true
done

# Strip debug symbols from all shared libraries
echo "Stripping binary symbols..."
find "$APP_DIR/usr" -type f \( -name "*.so" -o -name "*.so.*" \) -exec strip --strip-unneeded {} + 2>/dev/null || true

echo "5. Populating AppDir metadata..."
cp "$REPO_DIR/packaging/AppRun" "$APP_DIR/AppRun"
chmod +x "$APP_DIR/AppRun"
cp "$REPO_DIR/packaging/io.github.soundhop.desktop" "$APP_DIR/io.github.soundhop.desktop"
ln -sf io.github.soundhop.desktop "$APP_DIR/soundhop.desktop"
cp "$REPO_DIR/src/soundhop/assets/soundhop.svg" "$APP_DIR/soundhop.svg"
if [ -f "$REPO_DIR/src/soundhop/assets/soundhop.png" ]; then
    cp "$REPO_DIR/src/soundhop/assets/soundhop.png" "$APP_DIR/soundhop.png"
    cp "$REPO_DIR/src/soundhop/assets/soundhop.png" "$APP_DIR/.DirIcon"
else
    ln -sf soundhop.svg "$APP_DIR/.DirIcon"
fi

mkdir -p "$APP_DIR/usr/share/applications" "$APP_DIR/usr/share/icons/hicolor/scalable/apps" "$APP_DIR/usr/share/icons/hicolor/256x256/apps"
cp "$REPO_DIR/packaging/io.github.soundhop.desktop" "$APP_DIR/usr/share/applications/io.github.soundhop.desktop"
ln -sf io.github.soundhop.desktop "$APP_DIR/usr/share/applications/soundhop.desktop"
cp "$REPO_DIR/src/soundhop/assets/soundhop.svg" "$APP_DIR/usr/share/icons/hicolor/scalable/apps/soundhop.svg"
if [ -f "$REPO_DIR/src/soundhop/assets/soundhop.png" ]; then
    cp "$REPO_DIR/src/soundhop/assets/soundhop.png" "$APP_DIR/usr/share/icons/hicolor/256x256/apps/soundhop.png"
fi

echo "6. Ensuring official runtime & appimagetool..."
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

VERSION="${VERSION:-$(grep -oP 'VERSION\s*=\s*"\K[^"]+' "$REPO_DIR/src/soundhop/constants.py" 2>/dev/null || python3 -c "import sys; sys.path.insert(0, '$REPO_DIR/src'); from soundhop.constants import VERSION; print(VERSION)")}"
echo "7. Generating universal AppImage (version: $VERSION)..."
TMP_OUTPUT="$OUTPUT_DIR/.SoundHop-$VERSION-universal-x86_64.AppImage.tmp"
rm -f "$TMP_OUTPUT"
ARCH=x86_64 "$BUILD_DIR/squashfs-root/AppRun" --runtime-file "$BUILD_DIR/runtime-x86_64" "$APP_DIR" "$TMP_OUTPUT"
chmod +x "$TMP_OUTPUT"
mv -f "$TMP_OUTPUT" "$OUTPUT_DIR/SoundHop-$VERSION-universal-x86_64.AppImage"
echo "✓ Successfully built: dist/SoundHop-$VERSION-universal-x86_64.AppImage"
