#!/usr/bin/env bash
# Type-check the desktop's Rust backend for Windows from macOS or Linux
# (hub #3016 slice 14; the Windows lane is hub #1229 / #3001).
#
#   desktop/scripts/windows-check.sh          cargo check --all-targets
#                                             for x86_64-pc-windows-msvc
#
# It needs only the free Rust target (`rustup target add
# x86_64-pc-windows-msvc`) and a host C compiler (clang or cc) for the
# resource preprocessing step. It is a type check, not a build: tauri-build
# compiles the Windows resource file (icon, version info, manifest) with
# llvm-rc when it is not on Windows, and `cargo check` never links it, so a
# stub stands in for llvm-rc and writes an empty .lib. A real Windows build
# runs on Windows with MSVC (see desktop/README.md "Platforms").
set -euo pipefail

TARGET=x86_64-pc-windows-msvc
TAURI_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../src-tauri" && pwd)"

if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
    echo "error: the $TARGET Rust target is not installed (rustup target add $TARGET)" >&2
    exit 1
fi

STUB_DIR="$(mktemp -d)"
trap 'rm -rf "$STUB_DIR"' EXIT
cat > "$STUB_DIR/llvm-rc" <<'STUB'
#!/bin/sh
# llvm-rc stand-in for `cargo check` only: answers the probe, writes an empty
# output file for `/fo <file>`.
for arg in "$@"; do
    if [ "$arg" = "/?" ]; then
        echo "OVERVIEW: LLVM Resource Converter"
        echo "  /no-preprocess  stub for desktop/scripts/windows-check.sh"
        exit 0
    fi
done
while [ "$#" -gt 0 ]; do
    if [ "$1" = "/fo" ] && [ "$#" -gt 1 ]; then
        : > "$2"
        exit 0
    fi
    shift
done
echo "llvm-rc stub: no /fo output given" >&2
exit 1
STUB
chmod 755 "$STUB_DIR/llvm-rc"

cd "$TAURI_DIR"
env "RC_$TARGET=$STUB_DIR/llvm-rc" cargo check --target "$TARGET" --all-targets "$@"
