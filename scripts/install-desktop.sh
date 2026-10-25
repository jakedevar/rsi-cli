#!/usr/bin/env bash
# Build the RSI desktop UI (Tauri) and install it next to rsid/rsi.
#
#   scripts/install-desktop.sh                build + install (soft: a missing
#                                             toolchain only warns and exits 0);
#                                             never called by `make release-install`
#                                             (#3454), use `make desktop-install`
#   scripts/install-desktop.sh --build-only   build only (`make desktop`)
#   scripts/install-desktop.sh --required     fail instead of skipping
#
# Installs (all under the environment's HOME / XDG_DATA_HOME, so a temp HOME
# gives a throwaway install):
#   ~/.rsi/install/rsi-desktop      the binary (atomic copy)
#   ~/.local/bin/rsi-desktop        symlink to it (RSI_INSTALL_BIN_DIR overrides)
#   ~/.local/share/applications/rsi-desktop.desktop
#   ~/.local/share/icons/hicolor/<size>/apps/rsi-desktop.png
# and on macOS, instead of the two freedesktop entries:
#   ~/Applications/RSI Desktop.app  an unsigned app bundle around the same
#                                   binary (RSI_DESKTOP_APP_DIR overrides the
#                                   folder); `--build-only` leaves it at
#                                   <target>/release/bundle/macos/
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
DESKTOP_DIR="$ROOT/desktop"
TAURI_DIR="$DESKTOP_DIR/src-tauri"
BIN_DIR="${RSI_INSTALL_BIN_DIR:-$HOME/.local/bin}"
INSTALL_DIR="${RSI_DESKTOP_INSTALL_DIR:-$HOME/.rsi/install}"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
APP_DIR="${RSI_DESKTOP_APP_DIR:-$HOME/Applications}"

BUILD_ONLY=0
REQUIRED=0
for arg in "$@"; do
    case "$arg" in
        --build-only) BUILD_ONLY=1; REQUIRED=1 ;;
        --required) REQUIRED=1 ;;
        *) echo "Usage: $0 [--build-only] [--required]" >&2; exit 1 ;;
    esac
done

skip() {
    if [[ "$REQUIRED" -eq 1 ]]; then
        echo "error: $1" >&2
        exit 1
    fi
    echo "warning: skipping the RSI desktop UI: $1" >&2
    exit 0
}

command -v node >/dev/null 2>&1 || skip "node is not installed"
command -v npm >/dev/null 2>&1 || skip "npm is not installed"
command -v cargo >/dev/null 2>&1 || skip "cargo is not installed"
PLATFORM="$(uname -s)"
case "$PLATFORM" in
    Linux)
        if ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists webkit2gtk-4.1; then
            skip "webkit2gtk-4.1 development files are not installed (pkg-config webkit2gtk-4.1)"
        fi
        ;;
    Darwin)
        # Tauri uses macOS's system WebKit; WebKitGTK is Linux-only.
        command -v xcrun >/dev/null 2>&1 && xcrun --find clang >/dev/null 2>&1 \
            || skip "Xcode command line tools are not installed (xcode-select --install)"
        ;;
    *) skip "unsupported platform: $PLATFORM" ;;
esac

cd "$DESKTOP_DIR"
# Reinstall when node_modules is missing or older than the lockfile.
if [[ ! -d node_modules || package-lock.json -nt node_modules ]]; then
    npm ci --no-audit --no-fund
fi
npm run build

# The target cargo builds into: a global `build.target-dir` or CARGO_TARGET_DIR may be
# shared with other worktrees. Cargo's freshness check for the workspace crate
# (`rsi-desktop`) is mtime-based, so a tree whose sources are older than another
# tree's last build there would link that tree's crate. The release-target guard
# (#938, #2177) records which worktree built `release/` and removes this workspace's
# members when a different one builds, under one lock per target.
# json_string <file> <key>: the first string value of a key in a JSON file, read as plain text.
json_string() {
    sed -n "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$1" | head -n 1
}
# `target_directory` is a top-level string of the metadata JSON: plain text, no interpreter (#465).
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps --manifest-path "$TAURI_DIR/Cargo.toml" \
    | sed -n 's/.*"target_directory"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
# shellcheck source=scripts/release-target-guard.sh
source "$ROOT/scripts/release-target-guard.sh"
RELEASE_GUARD_MARKER=".rsi-desktop-source"
RELEASE_GUARD_LOCK=".rsi-desktop.lock"
RELEASE_GUARD_MANIFEST="$TAURI_DIR/Cargo.toml"
release_guard_lock "$TARGET_DIR"
release_guard_prepare "$ROOT" "$TARGET_DIR"
# `build.rs` watches ../dist, so a plain cargo build re-embeds frontend changes.
# fd 8 holds the lock; a long-lived child (sccache server) must not inherit it.
cargo build --release --manifest-path "$TAURI_DIR/Cargo.toml" 8>&-
release_guard_finish "$ROOT" "$TARGET_DIR"
BUILT_BIN="$TARGET_DIR/release/rsi-desktop"
[[ -x "$BUILT_BIN" ]] || { echo "error: $BUILT_BIN was not built" >&2; exit 1; }

# macOS app bundle, assembled by hand so the install needs no tauri-cli: the
# same layout `cargo tauri build --bundles app` writes (Contents/MacOS binary,
# Info.plist, Resources/icon.icns). It is not signed or notarized; a locally
# built bundle carries no quarantine flag, so it opens without Gatekeeper's
# prompt. Signing for distribution is the operator's step (desktop/README.md).
BUILT_APP=""
if [[ "$PLATFORM" == Darwin ]]; then
    APP_NAME="$(json_string "$TAURI_DIR/tauri.conf.json" productName)"
    APP_ID="$(json_string "$TAURI_DIR/tauri.conf.json" identifier)"
    APP_VERSION="$(json_string "$TAURI_DIR/tauri.conf.json" version)"
    [[ -n "$APP_NAME" && -n "$APP_ID" && -n "$APP_VERSION" ]] || {
        echo "error: $TAURI_DIR/tauri.conf.json must define productName, identifier and version" >&2
        exit 1
    }
    BUNDLE_DIR="$TARGET_DIR/release/bundle/macos"
    BUILT_APP="$BUNDLE_DIR/$APP_NAME.app"
    rm -rf "$BUILT_APP"
    mkdir -p "$BUILT_APP/Contents/MacOS" "$BUILT_APP/Contents/Resources"
    cp -p "$BUILT_BIN" "$BUILT_APP/Contents/MacOS/rsi-desktop"
    cp -p "$TAURI_DIR/icons/icon.icns" "$BUILT_APP/Contents/Resources/icon.icns"
    cat > "$BUILT_APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleExecutable</key><string>rsi-desktop</string>
  <key>CFBundleIconFile</key><string>icon.icns</string>
  <key>CFBundleIdentifier</key><string>$APP_ID</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$APP_VERSION</string>
  <key>CFBundleVersion</key><string>$APP_VERSION</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
    if command -v plutil >/dev/null 2>&1; then
        plutil -lint -s "$BUILT_APP/Contents/Info.plist"
    fi
fi

if [[ "$BUILD_ONLY" -eq 1 ]]; then
    echo "built: $BUILT_BIN"
    [[ -n "$BUILT_APP" ]] && echo "built: $BUILT_APP"
    exit 0
fi

# Binary: atomic rename so a running instance is replaced, never overwritten.
mkdir -p "$INSTALL_DIR" "$BIN_DIR"
temp="$(mktemp "$INSTALL_DIR/.rsi-desktop.XXXXXX")"
cp -p "$BUILT_BIN" "$temp"
chmod 755 "$temp"
mv -f "$temp" "$INSTALL_DIR/rsi-desktop"
ln -sfn "$INSTALL_DIR/rsi-desktop" "$BIN_DIR/rsi-desktop"

if [[ "$PLATFORM" == Darwin ]]; then
    # Replace the bundle by rename: a running copy keeps its open binary.
    mkdir -p "$APP_DIR"
    staged="$(mktemp -d "$APP_DIR/.rsi-desktop-app.XXXXXX")"
    cp -Rp "$BUILT_APP" "$staged/"
    rm -rf "$APP_DIR/$APP_NAME.app"
    mv "$staged/$APP_NAME.app" "$APP_DIR/$APP_NAME.app"
    rmdir "$staged"
    echo "Desktop UI installed:"
    echo "  $BIN_DIR/rsi-desktop -> $INSTALL_DIR/rsi-desktop"
    echo "  $APP_DIR/$APP_NAME.app"
    exit 0
fi

# Freedesktop entry (absolute Exec: launchers do not share this shell's PATH).
APPS_DIR="$DATA_HOME/applications"
mkdir -p "$APPS_DIR"
sed "s|@BIN@|$BIN_DIR/rsi-desktop|" "$DESKTOP_DIR/packaging/rsi-desktop.desktop" \
    > "$APPS_DIR/rsi-desktop.desktop"
chmod 644 "$APPS_DIR/rsi-desktop.desktop"

# Icons: <source file> -> hicolor size directory.
ICON_ROOT="$DATA_HOME/icons/hicolor"
for pair in 32x32.png:32x32 128x128.png:128x128 icon.png:512x512; do
    src="$TAURI_DIR/icons/${pair%%:*}"
    dest="$ICON_ROOT/${pair##*:}/apps"
    mkdir -p "$dest"
    install -m 644 "$src" "$dest/rsi-desktop.png"
done

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t "$ICON_ROOT" >/dev/null 2>&1 || true
fi

echo "Desktop UI installed:"
echo "  $BIN_DIR/rsi-desktop -> $INSTALL_DIR/rsi-desktop"
echo "  $APPS_DIR/rsi-desktop.desktop"
echo "  $ICON_ROOT/{32x32,128x128,512x512}/apps/rsi-desktop.png"
