#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

# Build the required binaries: rsi plus every sibling listed in the one shared
# definition (scripts/e2e-prebuild-bins.txt, #1651).
cargo build -p rsi --bin rsi
args=()
packages=" "
while read -r package binary _; do
  case "$package" in ''|'#'*) continue ;; esac
  case "$packages" in *" $package "*) ;; *) packages="$packages$package "; args+=(-p "$package") ;; esac
  args+=(--bin "$binary")
done < scripts/e2e-prebuild-bins.txt
cargo build "${args[@]}"

# Run the gated TUI E2E test target
RSI_E2E=1 cargo test -p rsi --test e2e_tui -- --nocapture --test-threads=1
