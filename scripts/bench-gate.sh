#!/usr/bin/env bash
#
# Phase 2.4 — Criterion bench regression gate.
#
# Runs a subset of the workspace's Criterion benches in fast mode
# (--measurement-time 5) and compares each median against the value in
# metrics/baseline.json. Fails (exit 1) if any median exceeds the baseline
# by more than `noise_floor.criterion_pct` percent (default 5%).
#
# Used as both a local pre-merge check and a CI advisory job. Promoted to
# required in a Phase 2 follow-on once CI noise floor is characterized.
#
# Usage:
#   ./scripts/bench-gate.sh                 # full subset
#   NOISE_FLOOR_PCT=10 ./scripts/bench-gate.sh   # override threshold
#   BENCHES="rpc_decode" ./scripts/bench-gate.sh # custom subset

set -euo pipefail

BASELINE="${BASELINE_PATH:-metrics/baseline.json}"
if [ ! -f "$BASELINE" ]; then
  echo "FAIL: baseline file not found at $BASELINE" >&2
  exit 2
fi

# Default subset chosen to mirror scripts/refresh-baseline.sh — five bench
# binaries across three crates that produce the 24 medians in baseline.json.
DEFAULT_BENCHES=(
  "-p rsi-common --bench rpc_decode"
  "-p rsi --bench ui_content"
  "-p rsi --bench ui_height"
  "-p rsid --bench store_worker_flush"
  "-p rsid --bench restore_sessions"
)

NOISE_FLOOR_PCT="${NOISE_FLOOR_PCT:-$(jq -r '.noise_floor.criterion_pct // 5.0' "$BASELINE")}"

echo "[*] Bench regression gate"
echo "    Baseline: $BASELINE"
echo "    Noise floor: ${NOISE_FLOOR_PCT}%"
echo "    Mode: --measurement-time 5 (fast)"
echo

# Run benches in fast mode. We pipe each invocation's stdout/stderr to a log
# file so the table at the end is readable; full output is on disk for debug.
LOG=$(mktemp -t bench_gate_XXXXXX.log)
trap 'rm -f "$LOG"' EXIT

if [ -n "${BENCHES:-}" ]; then
  read -r -a BENCH_LIST <<< "$BENCHES"
else
  BENCH_LIST=("${DEFAULT_BENCHES[@]}")
fi

for spec in "${BENCH_LIST[@]}"; do
  echo "[*] cargo bench $spec -- --measurement-time 5"
  # shellcheck disable=SC2086
  if ! cargo bench $spec -- --measurement-time 5 >>"$LOG" 2>&1; then
    echo "FAIL: bench invocation failed: $spec" >&2
    tail -40 "$LOG" >&2
    exit 1
  fi
done

# Parse target/criterion/*/new/estimates.json — same flat key namespace as
# refresh-baseline.sh.
FAIL=0
PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0

printf "%-60s %-15s %-15s %-10s %s\n" "Bench" "Baseline (ns)" "Measured (ns)" "Δ %" "Verdict"
printf "%-60s %-15s %-15s %-10s %s\n" "-----" "-------------" "-------------" "---" "-------"

while IFS= read -r est_path; do
  rel="${est_path#target/criterion/}"
  bench_key="${rel%/new/estimates.json}"
  measured=$(jq -r '.median.point_estimate // empty' "$est_path" 2>/dev/null || true)
  baseline=$(jq -r --arg k "$bench_key" '.criterion.median_ns[$k] // empty' "$BASELINE")

  if [ -z "$measured" ]; then
    continue
  fi

  if [ -z "$baseline" ]; then
    printf "%-60s %-15s %-15s %-10s %s\n" "$bench_key" "(none)" "$(printf '%.2f' "$measured")" "—" "SKIP"
    SKIP_COUNT=$((SKIP_COUNT + 1))
    continue
  fi

  # delta_pct = (measured - baseline) / baseline * 100
  delta=$(awk -v m="$measured" -v b="$baseline" 'BEGIN { printf "%.2f", (m - b) / b * 100 }')
  threshold_violated=$(awk -v d="$delta" -v t="$NOISE_FLOOR_PCT" 'BEGIN { print (d > t) ? 1 : 0 }')

  if [ "$threshold_violated" = "1" ]; then
    verdict="FAIL"
    FAIL_COUNT=$((FAIL_COUNT + 1))
    FAIL=1
  else
    verdict="PASS"
    PASS_COUNT=$((PASS_COUNT + 1))
  fi

  printf "%-60s %-15.2f %-15.2f %-10s %s\n" "$bench_key" "$baseline" "$measured" "${delta}%" "$verdict"
done < <(find target/criterion -path '*/new/estimates.json' 2>/dev/null | sort)

echo
echo "[*] Summary: $PASS_COUNT pass / $FAIL_COUNT fail / $SKIP_COUNT skip"

if [ "$FAIL" -eq 1 ]; then
  echo "FAIL: at least one bench exceeded baseline + ${NOISE_FLOOR_PCT}%"
  exit 1
fi

echo "PASS: all benches within ${NOISE_FLOOR_PCT}% of baseline"
exit 0
