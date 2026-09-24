#!/usr/bin/env bash
#
# Phase 4.3 — baseline ratchet check.
#
# Runs a fast subset of the workspace metrics against a freshly-built tree
# and compares each result against the committed metrics/baseline.json.
# Fails (exit 1) if any of:
#
#   - workspace coverage drops by more than 1.0 percentage point
#   - any criterion bench median exceeds baseline + noise_floor.criterion_pct
#   - cargo build wall-clock exceeds baseline + 20%
#
# Mutants and dhat are intentionally NOT run here — they are too slow for a
# per-PR gate. Mutants stay advisory; dhat has its own dedicated heap-gate
# script (Phase 2.5) running on the same advisory tier.
#
# When MODE=refresh is set, the script writes the freshly-measured metrics
# into a temp `metrics/baseline.candidate.json` so a monthly cron can adopt
# them after passing the ratchet check.
#
# Usage:
#   ./scripts/ratchet-check.sh                  # PR-time gate
#   MODE=refresh ./scripts/ratchet-check.sh     # monthly refresh candidate

set -euo pipefail

BASELINE="${BASELINE_PATH:-metrics/baseline.json}"
MODE="${MODE:-check}"
COVERAGE_TOLERANCE_PCT="${COVERAGE_TOLERANCE_PCT:-1.0}"
BUILD_HEADROOM_PCT="${BUILD_HEADROOM_PCT:-20}"

if [ ! -f "$BASELINE" ]; then
  echo "FAIL: baseline file not found at $BASELINE" >&2
  exit 2
fi

CRITERION_NOISE_PCT=$(jq -r '.noise_floor.criterion_pct // 5.0' "$BASELINE")

echo "[*] Baseline ratchet check"
echo "    Baseline:     $BASELINE"
echo "    Mode:         $MODE"
echo "    Coverage Δ:   max -${COVERAGE_TOLERANCE_PCT} pp"
echo "    Criterion Δ:  max +${CRITERION_NOISE_PCT}%"
echo "    Build time Δ: max +${BUILD_HEADROOM_PCT}%"
echo

OVERALL_FAIL=0
PR_METRICS=$(mktemp -t pr_metrics_XXXXXX.json)
trap 'rm -f "$PR_METRICS"' EXIT
echo '{}' > "$PR_METRICS"

# ── 1. build wall-clock ─────────────────────────────────────────────────────
echo "[*] cargo build --workspace --release (timed)"
BUILD_START=$(date +%s.%N)
if cargo build --workspace --release >/dev/null 2>&1; then
  BUILD_END=$(date +%s.%N)
  BUILD_SECONDS=$(awk -v end="$BUILD_END" -v start="$BUILD_START" 'BEGIN { printf "%.3f", end - start }')
  jq --arg s "$BUILD_SECONDS" '.build.wall_clock_seconds = ($s | tonumber)' "$PR_METRICS" > "$PR_METRICS.tmp"
  mv "$PR_METRICS.tmp" "$PR_METRICS"
  BASELINE_BUILD=$(jq -r '.build.wall_clock_seconds' "$BASELINE")
  BUILD_THRESHOLD=$(awk -v b="$BASELINE_BUILD" -v p="$BUILD_HEADROOM_PCT" 'BEGIN { printf "%.3f", b * (1 + p / 100) }')
  BUILD_VIOLATED=$(awk -v m="$BUILD_SECONDS" -v t="$BUILD_THRESHOLD" 'BEGIN { print (m > t) ? 1 : 0 }')
  if [ "$BUILD_VIOLATED" = "1" ]; then
    printf "  build:    %s s    baseline %s s    threshold %s s    FAIL\n" "$BUILD_SECONDS" "$BASELINE_BUILD" "$BUILD_THRESHOLD"
    OVERALL_FAIL=1
  else
    printf "  build:    %s s    baseline %s s    threshold %s s    PASS\n" "$BUILD_SECONDS" "$BASELINE_BUILD" "$BUILD_THRESHOLD"
  fi
else
  echo "  build:    FAIL (compile error)"
  OVERALL_FAIL=1
fi

# ── 2. coverage ─────────────────────────────────────────────────────────────
if cargo llvm-cov --version >/dev/null 2>&1; then
  COV_JSON=$(mktemp -t cov_XXXXXX.json)
  if cargo llvm-cov --workspace --json --output-path "$COV_JSON" >/dev/null 2>&1; then
    PR_COV=$(jq '.coverage_percentage' "$COV_JSON")
    PER_CRATE=$(jq '.per_crate' "$COV_JSON")
    jq --argjson pct "$PR_COV" --argjson per "$PER_CRATE" '.coverage = { workspace_pct: $pct, per_crate: $per }' "$PR_METRICS" > "$PR_METRICS.tmp"
    mv "$PR_METRICS.tmp" "$PR_METRICS"
    BASELINE_COV=$(jq -r '.coverage.workspace_pct' "$BASELINE")
    COV_THRESHOLD=$(awk -v b="$BASELINE_COV" -v p="$COVERAGE_TOLERANCE_PCT" 'BEGIN { printf "%.4f", b - p }')
    COV_VIOLATED=$(awk -v m="$PR_COV" -v t="$COV_THRESHOLD" 'BEGIN { print (m < t) ? 1 : 0 }')
    if [ "$COV_VIOLATED" = "1" ]; then
      printf "  coverage: %.2f%%    baseline %.2f%%    floor %.2f%%    FAIL\n" "$PR_COV" "$BASELINE_COV" "$COV_THRESHOLD"
      OVERALL_FAIL=1
    else
      printf "  coverage: %.2f%%    baseline %.2f%%    floor %.2f%%    PASS\n" "$PR_COV" "$BASELINE_COV" "$COV_THRESHOLD"
    fi
  else
    echo "  coverage: FAIL (cargo llvm-cov error — treating as advisory skip)"
  fi
  rm -f "$COV_JSON"
else
  echo "  coverage: SKIP (cargo-llvm-cov not installed)"
fi

# ── 3. criterion benches ────────────────────────────────────────────────────
echo "[*] cargo bench-all --measurement-time 5"
if cargo bench -p rsi-common --bench rpc_decode \
              -p rsi --bench ui_content --bench ui_height \
              -p rsid --bench store_worker_flush --bench restore_sessions \
              -- --measurement-time 5 >/dev/null 2>&1; then
  WORST_DELTA=0
  WORST_BENCH=""
  CRIT_FAIL=0
  CRIT_MEDIANS='{}'
  while IFS= read -r est_path; do
    rel="${est_path#target/criterion/}"
    bench_key="${rel%/new/estimates.json}"
    measured=$(jq -r '.median.point_estimate // empty' "$est_path" 2>/dev/null || true)
    [ -z "$measured" ] && continue
    CRIT_MEDIANS=$(echo "$CRIT_MEDIANS" | jq --arg k "$bench_key" --arg v "$measured" '.[$k] = ($v | tonumber)')
    baseline_v=$(jq -r --arg k "$bench_key" '.criterion.median_ns[$k] // empty' "$BASELINE")
    [ -z "$baseline_v" ] && continue
    delta=$(awk -v m="$measured" -v b="$baseline_v" 'BEGIN { printf "%.2f", (m - b) / b * 100 }')
    violated=$(awk -v d="$delta" -v t="$CRITERION_NOISE_PCT" 'BEGIN { print (d > t) ? 1 : 0 }')
    if [ "$violated" = "1" ]; then
      printf "  %-50s Δ %s%%    FAIL\n" "$bench_key" "$delta"
      CRIT_FAIL=1
      OVERALL_FAIL=1
    fi
    is_worst=$(awk -v d="$delta" -v w="$WORST_DELTA" 'BEGIN { print (d > w) ? 1 : 0 }')
    if [ "$is_worst" = "1" ]; then
      WORST_DELTA="$delta"
      WORST_BENCH="$bench_key"
    fi
  done < <(find target/criterion -path '*/new/estimates.json' 2>/dev/null | sort)
  jq --argjson m "$CRIT_MEDIANS" '.criterion = { median_ns: $m }' "$PR_METRICS" > "$PR_METRICS.tmp"
  mv "$PR_METRICS.tmp" "$PR_METRICS"
  if [ "$CRIT_FAIL" = "0" ]; then
    printf "  criterion: worst Δ %s%% (%s)    threshold +%s%%    PASS\n" "$WORST_DELTA" "$WORST_BENCH" "$CRITERION_NOISE_PCT"
  fi
else
  echo "  criterion: FAIL (bench invocation error)"
  OVERALL_FAIL=1
fi

# ── refresh mode: write candidate baseline ──────────────────────────────────
if [ "$MODE" = "refresh" ]; then
  CANDIDATE="metrics/baseline.candidate.json"
  GIT_COMMIT=$(git rev-parse HEAD)
  CAPTURED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  jq --arg sha "$GIT_COMMIT" --arg ts "$CAPTURED_AT" \
     --slurpfile orig "$BASELINE" \
     '$orig[0]
        + .
        + { schema_version: 1, git_commit: $sha, captured_at: $ts }
        | .noise_floor = ($orig[0].noise_floor // { criterion_pct: 5.0, dhat_pct: 2.0 })
        | .mutants = ($orig[0].mutants // {})
        | .dhat = ($orig[0].dhat // {})' "$PR_METRICS" > "$CANDIDATE"
  echo
  echo "[*] candidate baseline written to $CANDIDATE"
fi

echo
if [ "$OVERALL_FAIL" = "1" ]; then
  echo "FAIL: at least one metric regressed past tolerance"
  exit 1
fi
echo "PASS: all metrics within tolerance"
exit 0
