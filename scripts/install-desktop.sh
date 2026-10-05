#!/usr/bin/env bash
# Build the RSI desktop UI (Tauri) and install it next to rsid/rsi.
#
#   scripts/install-desktop.sh                build + install (soft: a missing
#                                             toolchain only warns and exits 0)
#   scripts/install-desktop.sh --build-only   build only (`make desktop`)
#   scripts/install-desktop.sh --required     fail instead of skipping
#
# Installs (all under the environment's HOME / XDG_DATA_HOME, so a temp HOME
# gives a throwaway install):
#   ~/.rsi/install/rsi-desktop      the binary (atomic copy)
#   ~/.local/bin/rsi-desktop        symlink to it (RSI_INSTALL_BIN_DIR overrides)
#   ~/.local/share/applications/rsi-desktop.desktop
#   ~/.local/share/icons/hicolor/<size>/apps/rsi-desktop.png
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
DESKTOP_DIR="$ROOT/desktop"
TAURI_DIR="$DESKTOP_DIR/src-tauri"
BIN_DIR="${RSI_INSTALL_BIN_DIR:-$HOME/.local/bin}"
INSTALL_DIR="${RSI_DESKTOP_INSTALL_DIR:-$HOME/.rsi/install}"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"

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
if ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists webkit2gtk-4.1; then
    skip "webkit2gtk-4.1 development files are not installed (pkg-config webkit2gtk-4.1)"
fi

cd "$DESKTOP_DIR"
# Reinstall when node_modules is missing or older than the lockfile.
if [[ ! -d node_modules || package-lock.json -nt node_modules ]]; then
    npm ci --no-audit --no-fund
fi
npm run build

# `build.rs` watches ../dist, so a plain cargo build re-embeds frontend changes.
cargo build --release --manifest-path "$TAURI_DIR/Cargo.toml"
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps --manifest-path "$TAURI_DIR/Cargo.toml" \
    | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
BUILT_BIN="$TARGET_DIR/release/rsi-desktop"
[[ -x "$BUILT_BIN" ]] || { echo "error: $BUILT_BIN was not built" >&2; exit 1; }

if [[ "$BUILD_ONLY" -eq 1 ]]; then
    echo "built: $BUILT_BIN"
    exit 0
fi

# Binary: atomic rename so a running instance is replaced, never overwritten.
mkdir -p "$INSTALL_DIR" "$BIN_DIR"
temp="$(mktemp "$INSTALL_DIR/.rsi-desktop.XXXXXX")"
cp -p "$BUILT_BIN" "$temp"
chmod 755 "$temp"
mv -f "$temp" "$INSTALL_DIR/rsi-desktop"
ln -sfn "$INSTALL_DIR/rsi-desktop" "$BIN_DIR/rsi-desktop"

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
