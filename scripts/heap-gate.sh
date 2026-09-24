#!/usr/bin/env bash
#
# Phase 2.5 — dhat heap regression gate.
#
# Runs the rsid `dhat_baseline` integration test under the `dhat-heap` feature
# and compares the measured total_heap_bytes against metrics/baseline.json.
# Fails (exit 1) if measured > baseline * (1 + HEADROOM_PCT/100).
#
# Default headroom = 10%, vs the local 2% noise floor — heap measurements are
# noisier on shared CI runners (allocator tuning differences). Override via
# HEADROOM_PCT for tighter local gates.
#
# Usage:
#   ./scripts/heap-gate.sh                  # default 10% headroom
#   HEADROOM_PCT=20 ./scripts/heap-gate.sh  # CI override
#
# Implementation note: dhat's JSON has a `pps[].tb` array (total bytes per
# allocation point). Sum via `jq '[.pps[] | .tb] | add'`. This matches the
# extraction logic in scripts/refresh-baseline.sh.

set -euo pipefail

BASELINE="${BASELINE_PATH:-metrics/baseline.json}"
if [ ! -f "$BASELINE" ]; then
  echo "FAIL: baseline file not found at $BASELINE" >&2
  exit 2
fi

BASELINE_BYTES=$(jq -r '.dhat.total_heap_bytes // empty' "$BASELINE")
if [ -z "$BASELINE_BYTES" ] || [ "$BASELINE_BYTES" = "null" ] || [ "$BASELINE_BYTES" = "0" ]; then
  echo "FAIL: baseline has no dhat.total_heap_bytes (run scripts/refresh-baseline.sh first)" >&2
  exit 2
fi

HEADROOM_PCT="${HEADROOM_PCT:-10}"
THRESHOLD=$(awk -v b="$BASELINE_BYTES" -v p="$HEADROOM_PCT" 'BEGIN { printf "%d", b * (1 + p / 100) }')

echo "[*] Heap regression gate"
echo "    Baseline: $BASELINE"
echo "    Baseline bytes: $BASELINE_BYTES"
echo "    Headroom: ${HEADROOM_PCT}%"
echo "    Threshold: $THRESHOLD bytes"
echo

# Clean any prior dhat output so we always read the freshest snapshot.
rm -f target/dhat/dhat-heap-baseline.json target/dhat/dhat-heap-*.json 2>/dev/null || true

# Ignored test — must be invoked with `-- --ignored`.
echo "[*] cargo test --features dhat-heap -p rsid --test dhat_baseline -- --ignored"
if ! cargo test --features dhat-heap -p rsid --test dhat_baseline -- --ignored; then
  echo "FAIL: dhat baseline test failed" >&2
  exit 1
fi

# Locate the most recent dhat output. The dhat_baseline test writes a
# canonical filename `dhat-heap-baseline.json`; older test variants may have
# used the timestamped pattern. Take the canonical name first, fall back to
# the most recently modified `dhat-heap-*.json`.
SNAPSHOT="target/dhat/dhat-heap-baseline.json"
if [ ! -f "$SNAPSHOT" ]; then
  SNAPSHOT=$(find target/dhat -maxdepth 1 -name 'dhat-heap-*.json' -printf '%T@ %p\n' 2>/dev/null \
    | sort -rn | head -1 | cut -d' ' -f2-)
fi

if [ -z "$SNAPSHOT" ] || [ ! -f "$SNAPSHOT" ]; then
  echo "FAIL: no dhat output found under target/dhat/" >&2
  exit 1
fi

MEASURED=$(jq -r '[.pps[] | .tb] | add // 0' "$SNAPSHOT")
if [ -z "$MEASURED" ] || [ "$MEASURED" = "null" ] || [ "$MEASURED" = "0" ]; then
  echo "FAIL: could not parse total_bytes (.pps[].tb sum) from $SNAPSHOT" >&2
  exit 1
fi

DELTA_PCT=$(awk -v m="$MEASURED" -v b="$BASELINE_BYTES" 'BEGIN { printf "%.2f", (m - b) / b * 100 }')

echo "    Measured: $MEASURED bytes (Δ ${DELTA_PCT}%)"

if [ "$MEASURED" -gt "$THRESHOLD" ]; then
  echo
  echo "FAIL: heap $MEASURED bytes > threshold $THRESHOLD bytes"
  echo "      (baseline $BASELINE_BYTES + ${HEADROOM_PCT}% headroom)"
  exit 1
fi

echo "PASS: heap $MEASURED bytes <= threshold $THRESHOLD bytes"
exit 0
