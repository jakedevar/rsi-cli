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
cargo build ${args[@]+"${args[@]}"}

# Run the gated TUI E2E test target. E2E_FILTER names one exact test and
# E2E_REPEAT runs it that many times, stopping at the first failure; this is how
# a flaky test is proven stable under the job slice (#2421).
if [ -n "${E2E_FILTER:-}" ]; then
  for run in $(seq 1 "${E2E_REPEAT:-1}"); do
    echo "e2e repeat $run/${E2E_REPEAT:-1}: $E2E_FILTER"
    RSI_E2E=1 cargo test -p rsi --test e2e_tui -- --exact "$E2E_FILTER" --nocapture --test-threads=1
  done
else
  RSI_E2E=1 cargo test -p rsi --test e2e_tui -- --nocapture --test-threads=1
fi
