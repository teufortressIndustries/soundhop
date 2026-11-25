#!/usr/bin/env bash
# ==============================================================================
# Verify SoundHop AppImage metadata, AppStream metainfo, icons, and zsync update
# compatibility (AppManager / AppImageUpdate).
#
# Usage:
#   ./packaging/verify-appimage.sh <path-to-AppImage> [expected-version]
# ==============================================================================
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
    echo "Usage: $0 <path-to-AppImage> [expected-version]" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
APPIMAGE_INPUT="$1"
EXPECTED_VERSION="${2:-}"
UPDATE_REPO="${UPDATE_REPO:-teufortressIndustries/soundhop}"

if [[ -z "$EXPECTED_VERSION" ]]; then
    EXPECTED_VERSION="$(awk '
        /^\[workspace\.package\]/ { in_sec=1; next }
        /^\[/ { in_sec=0 }
        in_sec && /^version[[:space:]]*=/ {
            sub(/^[^"]*"/, "")
            sub(/".*$/, "")
            print
            exit
        }
    ' "$REPO_DIR/Cargo.toml")"
fi

if [[ -z "$EXPECTED_VERSION" || "${EXPECTED_VERSION,,}" == "unknown" ]]; then
    echo "FAIL: Expected version is empty or UNKNOWN (got '${EXPECTED_VERSION:-}')." >&2
    exit 1
fi

if [[ ! -f "$APPIMAGE_INPUT" ]]; then
    echo "FAIL: AppImage not found: $APPIMAGE_INPUT" >&2
    exit 1
fi

APPIMAGE_ABS="$(readlink -f "$APPIMAGE_INPUT")"
APPIMAGE_BASENAME="$(basename "$APPIMAGE_ABS")"
ZSYNC_FILE="${APPIMAGE_ABS}.zsync"
ZSYNC_BASENAME="${APPIMAGE_BASENAME}.zsync"

if [[ ! -x "$APPIMAGE_ABS" ]]; then
    echo "FAIL: AppImage is not executable: $APPIMAGE_ABS" >&2
    exit 1
fi

FAILURES=0
pass() { echo "  [PASS] $*"; }
warn() { echo "  [WARN] $*"; }
fail() { echo "  [FAIL] $*" >&2; FAILURES=$((FAILURES + 1)); }

echo "=== Verifying AppImage: $APPIMAGE_BASENAME (expected version: $EXPECTED_VERSION) ==="

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

# 1. Extract AppImage
echo "1. Extracting AppImage..."
if ! (cd "$TMP_DIR" && "$APPIMAGE_ABS" --appimage-extract >/dev/null 2>&1); then
    echo "FAIL: Could not extract AppImage with --appimage-extract." >&2
    exit 1
fi

if [[ ! -e "$TMP_DIR/squashfs-root" ]]; then
    echo "FAIL: Expected squashfs-root after --appimage-extract." >&2
    exit 1
fi

EXTRACTED_DIR="$(readlink -f "$TMP_DIR/squashfs-root")"
if [[ -x "$EXTRACTED_DIR/AppRun" ]]; then
    pass "Executable AppRun present at AppDir root"
else
    fail "Missing or non-executable AppRun at AppDir root"
fi

# 2. Verify root .desktop file
echo "2. Checking root .desktop file..."
DESKTOP_FILE="$EXTRACTED_DIR/io.github.soundhop.desktop"
if [[ ! -f "$DESKTOP_FILE" ]]; then
    fail "Missing root desktop file: io.github.soundhop.desktop"
else
    desktop_get() {
        local key="$1"
        grep -E "^${key}=" "$DESKTOP_FILE" | head -n1 | cut -d= -f2-
    }

    version_count="$(grep -cE '^X-AppImage-Version=' "$DESKTOP_FILE" || true)"
    desktop_version="$(desktop_get 'X-AppImage-Version')"
    if [[ "$version_count" -ne 1 ]]; then
        fail "Expected exactly 1 X-AppImage-Version= entry in root .desktop, found $version_count"
    elif [[ -z "$desktop_version" || "${desktop_version,,}" == "unknown" ]]; then
        fail "X-AppImage-Version is empty or UNKNOWN (got '$desktop_version')"
    elif [[ "$desktop_version" != "$EXPECTED_VERSION" ]]; then
        fail "X-AppImage-Version mismatch: expected '$EXPECTED_VERSION', got '$desktop_version'"
    else
        pass "X-AppImage-Version=$desktop_version"
    fi

    desktop_name="$(desktop_get 'Name')"
    if [[ "$desktop_name" == "SoundHop" ]]; then
        pass "Name=$desktop_name"
    else
        fail "Unexpected or missing Name= (expected 'SoundHop', got '${desktop_name:-}')"
    fi

    desktop_exec="$(desktop_get 'Exec')"
    if [[ -n "$desktop_exec" ]]; then
        pass "Exec=$desktop_exec"
    else
        fail "Missing Exec= in root .desktop"
    fi

    desktop_icon="$(desktop_get 'Icon')"
    if [[ -n "$desktop_icon" ]]; then
        pass "Icon=$desktop_icon"
    else
        fail "Missing Icon= in root .desktop"
    fi

    desktop_update_url="$(desktop_get 'X-AppImage-UpdateURL')"
    expected_update_url="https://github.com/${UPDATE_REPO}"
    if [[ "$desktop_update_url" == "$expected_update_url" ]]; then
        pass "X-AppImage-UpdateURL=$desktop_update_url"
    else
        fail "X-AppImage-UpdateURL mismatch: expected '$expected_update_url', got '${desktop_update_url:-}'"
    fi

    desktop_wmclass="$(desktop_get 'StartupWMClass')"
    if [[ -n "$desktop_wmclass" ]]; then
        pass "StartupWMClass=$desktop_wmclass"
    else
        fail "Missing StartupWMClass= in root .desktop"
    fi

    if command -v desktop-file-validate >/dev/null 2>&1; then
        if desktop-file-validate "$DESKTOP_FILE"; then
            pass "desktop-file-validate passed"
        else
            fail "desktop-file-validate failed on $DESKTOP_FILE"
        fi
    else
        warn "desktop-file-validate not installed; skipping"
    fi
fi

# 3. Verify AppStream metainfo
echo "3. Checking AppStream metainfo..."
METAINFO_FILE="$EXTRACTED_DIR/usr/share/metainfo/io.github.soundhop.metainfo.xml"
if [[ ! -s "$METAINFO_FILE" ]]; then
    fail "Missing or empty usr/share/metainfo/io.github.soundhop.metainfo.xml"
else
    pass "Found usr/share/metainfo/io.github.soundhop.metainfo.xml"
    if grep -Fq '<launchable type="desktop-id">io.github.soundhop.desktop</launchable>' "$METAINFO_FILE"; then
        pass "Metainfo <launchable> matches io.github.soundhop.desktop"
    else
        fail "Metainfo missing <launchable type=\"desktop-id\">io.github.soundhop.desktop</launchable>"
    fi

    if grep -Fq "<release version=\"${EXPECTED_VERSION}\"" "$METAINFO_FILE"; then
        pass "Metainfo contains <release version=\"${EXPECTED_VERSION}\">"
    else
        fail "Metainfo missing <release version=\"${EXPECTED_VERSION}\">"
    fi

    if command -v appstreamcli >/dev/null 2>&1; then
        if appstreamcli validate --no-net "$METAINFO_FILE"; then
            pass "appstreamcli validate passed"
        else
            fail "appstreamcli validate failed on $METAINFO_FILE"
        fi
    else
        warn "appstreamcli not installed; skipping"
    fi
fi

# 4. Verify icons (.DirIcon + hicolor)
echo "4. Checking icons..."
if [[ -s "$EXTRACTED_DIR/.DirIcon" ]]; then
    pass ".DirIcon present and non-empty"
else
    fail "Missing or empty .DirIcon at AppDir root"
fi

if [[ -s "$EXTRACTED_DIR/usr/share/icons/hicolor/256x256/apps/soundhop.png" ]] || \
   [[ -s "$EXTRACTED_DIR/usr/share/icons/hicolor/scalable/apps/soundhop.svg" ]]; then
    pass "Hicolor icon(s) present under usr/share/icons/hicolor/"
else
    fail "Missing hicolor icon under usr/share/icons/hicolor/{256x256,scalable}/apps/soundhop.{png,svg}"
fi

# 5. Verify embedded update information (.upd_info)
echo "5. Checking embedded update information..."
UPD_INFO="$("$APPIMAGE_ABS" --appimage-updateinformation 2>/dev/null | tr -d '\r' | head -n1 || true)"
EXPECTED_UPD_PREFIX="gh-releases-zsync|${UPDATE_REPO%/*}|${UPDATE_REPO#*/}|latest|"

if [[ -z "$UPD_INFO" ]]; then
    fail "--appimage-updateinformation returned empty output"
else
    if [[ "$UPD_INFO" == "${EXPECTED_UPD_PREFIX}"*.AppImage.zsync ]]; then
        pass "--appimage-updateinformation: $UPD_INFO"
    else
        fail "Unexpected update information: '$UPD_INFO' (expected prefix '${EXPECTED_UPD_PREFIX}' ending in '.AppImage.zsync')"
    fi

    UPD_PATTERN="${UPD_INFO##*|}"
    # Intentional unquoted glob on RHS to test fnmatch-style matching against .zsync basename
    # shellcheck disable=SC2053
    if [[ "$ZSYNC_BASENAME" == $UPD_PATTERN ]]; then
        pass "Update pattern '$UPD_PATTERN' matches '$ZSYNC_BASENAME'"
    else
        fail "Update pattern '$UPD_PATTERN' does NOT match '$ZSYNC_BASENAME'"
    fi

    if command -v readelf >/dev/null 2>&1; then
        ELF_UPD_INFO="$(readelf -p .upd_info "$APPIMAGE_ABS" 2>/dev/null | sed -n 's/^[[:space:]]*\[[[:space:]]*0\][[:space:]]*//p' | head -n1 || true)"
        if [[ "$ELF_UPD_INFO" == "$UPD_INFO" ]]; then
            pass "ELF .upd_info section matches --appimage-updateinformation"
        else
            fail "ELF .upd_info mismatch: got '${ELF_UPD_INFO:-}', expected '$UPD_INFO'"
        fi
    fi
fi

# 6. Verify .AppImage.zsync
echo "6. Checking .zsync metadata..."
if [[ ! -s "$ZSYNC_FILE" ]]; then
    fail "Missing or empty zsync file: $ZSYNC_FILE"
else
    ZSYNC_HEADER="$(LC_ALL=C awk '/^\r?$/{exit} {sub(/\r$/, ""); print}' "$ZSYNC_FILE")"
    zsync_get() {
        local key="$1"
        printf '%s\n' "$ZSYNC_HEADER" | awk -F': ' -v k="$key" '$1 == k { print substr($0, length(k) + 3); exit }'
    }

    zsync_filename="$(zsync_get 'Filename')"
    zsync_url="$(zsync_get 'URL')"
    zsync_sha1="$(zsync_get 'SHA-1')"
    actual_sha1="$(sha1sum "$APPIMAGE_ABS" | awk '{print $1}')"

    if [[ "$zsync_filename" == "$APPIMAGE_BASENAME" ]]; then
        pass "zsync Filename: $zsync_filename"
    else
        fail "zsync Filename mismatch: expected '$APPIMAGE_BASENAME', got '${zsync_filename:-}'"
    fi

    if [[ "$zsync_url" == "$APPIMAGE_BASENAME" ]]; then
        pass "zsync URL: $zsync_url"
    else
        fail "zsync URL mismatch: expected '$APPIMAGE_BASENAME', got '${zsync_url:-}'"
    fi

    if [[ -z "$zsync_sha1" ]]; then
        fail "zsync missing required SHA-1 header"
    elif [[ "$zsync_sha1" != "$actual_sha1" ]]; then
        fail "zsync SHA-1 mismatch: header has '$zsync_sha1', actual sha1sum is '$actual_sha1'"
    else
        pass "zsync SHA-1 matches AppImage ($zsync_sha1)"
    fi
fi

# 7. Verify bundled GStreamer plugins & runtime hook
echo "7. Checking bundled GStreamer plugins..."
GST_PLUGIN_DIR="$EXTRACTED_DIR/lib/gstreamer-1.0"
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
if [[ ! -d "$GST_PLUGIN_DIR" ]]; then
    fail "Missing bundled GStreamer plugin directory: lib/gstreamer-1.0"
else
    missing_plugins=0
    for plugin in "${GST_REQUIRED_PLUGINS[@]}"; do
        if [[ -s "$GST_PLUGIN_DIR/$plugin" ]]; then
            pass "Bundled GStreamer plugin present: lib/gstreamer-1.0/$plugin"
        else
            fail "Missing or empty bundled GStreamer plugin: lib/gstreamer-1.0/$plugin"
            missing_plugins=$((missing_plugins + 1))
        fi
    done
    if [[ "$missing_plugins" -eq 0 ]]; then
        pass "All ${#GST_REQUIRED_PLUGINS[@]} required GStreamer plugins verified"
    fi
fi

GST_HOOK="$EXTRACTED_DIR/bin/soundhop-gst.hook"
if [[ -s "$GST_HOOK" ]] && grep -Fq "GST_PLUGIN_PATH_1_0" "$GST_HOOK" && grep -Fq "GST_REGISTRY_FORK=no" "$GST_HOOK"; then
    pass "Bundled GStreamer environment hook present: bin/soundhop-gst.hook"
else
    fail "Missing or incomplete GStreamer environment hook: bin/soundhop-gst.hook"
fi

if [[ "$FAILURES" -gt 0 ]]; then
    echo "=== Verification FAILED ($FAILURES check(s) failed) ===" >&2
    exit 1
fi

echo "=== All AppImage metadata & update checks PASSED ==="
