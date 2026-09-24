#!/bin/bash
set -euo pipefail

# Script: refresh-baseline.sh
# Purpose: Capture comprehensive baseline metrics for the workspace.
# Output: metrics/baseline.json with build, coverage, mutants, criterion, and dhat metrics.

GIT_COMMIT=$(git rev-parse HEAD)
CAPTURED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)

echo "[*] Capturing baseline metrics..."
echo "    Git commit: $GIT_COMMIT"
echo "    Timestamp:  $CAPTURED_AT"

# Initialize metrics object
# Use mktemp so concurrent invocations don't collide on a shared path.
METRICS_TMP=$(mktemp -t baseline_metrics_XXXXXX.json)
trap 'rm -f "$METRICS_TMP"' EXIT
cat > "$METRICS_TMP" << EOF
{
  "schema_version": 1,
  "git_commit": "$GIT_COMMIT",
  "captured_at": "$CAPTURED_AT",
  "build": { "wall_clock_seconds": 0.0 },
  "coverage": { "workspace_pct": 0.0, "per_crate": {} },
  "mutants": { "kill_rate_pct_per_crate": {} },
  "criterion": { "median_ns": {} },
  "dhat": { "total_heap_bytes": 0, "total_blocks": 0 },
  "noise_floor": { "criterion_pct": 5.0, "dhat_pct": 2.0 }
}
EOF

# 1. Build timing
echo "[*] Timing cargo build --workspace --release..."
BUILD_START=$(date +%s.%N)
if cargo build --workspace --release > /dev/null 2>&1; then
  BUILD_END=$(date +%s.%N)
  # Use awk instead of bc — bc is not installed on every system (e.g. minimal
  # Arch installs); awk is POSIX and present everywhere.
  BUILD_SECONDS=$(awk -v end="$BUILD_END" -v start="$BUILD_START" 'BEGIN {printf "%.3f", end - start}')
  echo "    Build completed in $BUILD_SECONDS seconds"
  METRICS_TMP_NEW=$(mktemp)
  jq --arg val "$BUILD_SECONDS" '.build.wall_clock_seconds = ($val | tonumber)' "$METRICS_TMP" > "$METRICS_TMP_NEW"
  mv "$METRICS_TMP_NEW" "$METRICS_TMP"
else
  echo "    [!] cargo build failed"
fi

# 2. Code coverage (llvm-cov)
# Use `cargo` subcommand resolution rather than PATH check — cargo plugins
# install to ~/.cargo/bin which may not be on bash PATH but is auto-resolved
# by cargo for subcommands.
echo "[*] Collecting code coverage..."
if cargo llvm-cov --version &> /dev/null; then
  COV_JSON=$(mktemp -t llvm_cov_XXXXXX.json)
  if cargo llvm-cov --workspace --json --output-path "$COV_JSON" > /dev/null 2>&1; then
    WORKSPACE_COV=$(jq '.coverage_percentage' "$COV_JSON" 2>/dev/null || echo "null")
    PER_CRATE=$(jq '.per_crate' "$COV_JSON" 2>/dev/null || echo "{}")
    echo "    Workspace coverage: $WORKSPACE_COV%"
    METRICS_TMP_NEW=$(mktemp)
    jq --argjson workspace "$WORKSPACE_COV" --argjson crates "$PER_CRATE" \
      '.coverage.workspace_pct = $workspace | .coverage.per_crate = $crates' \
      "$METRICS_TMP" > "$METRICS_TMP_NEW"
    mv "$METRICS_TMP_NEW" "$METRICS_TMP"
  else
    echo "    [!] cargo llvm-cov failed (continuing with null)"
  fi
else
  echo "    [!] cargo-llvm-cov not installed (skipping; cargo install cargo-llvm-cov)"
fi

# 3. Mutation testing (cargo-mutants)
# Note: full-workspace mutation runs are LONG (30+ min). Set CARGO_MUTANTS_SKIP=1
# to skip this step when capturing a quick baseline; the canonical baseline run
# should NOT skip.
echo "[*] Running mutation testing..."
if [ "${CARGO_MUTANTS_SKIP:-0}" = "1" ]; then
  echo "    [!] CARGO_MUTANTS_SKIP=1 — skipping mutation testing"
elif cargo mutants --version &> /dev/null; then
  MUTANTS_JSON=$(mktemp -t mutants_XXXXXX.json)
  if cargo mutants --workspace --json --output target/mutants > "$MUTANTS_JSON" 2>&1; then
    KILL_RATES=$(jq '.per_crate_kill_rates // {}' "$MUTANTS_JSON" 2>/dev/null || echo "{}")
    echo "    Mutation testing completed"
    METRICS_TMP_NEW=$(mktemp)
    jq --argjson kill_rates "$KILL_RATES" \
      '.mutants.kill_rate_pct_per_crate = $kill_rates' \
      "$METRICS_TMP" > "$METRICS_TMP_NEW"
    mv "$METRICS_TMP_NEW" "$METRICS_TMP"
  else
    echo "    [!] cargo mutants failed (continuing with null)"
  fi
else
  echo "    [!] cargo-mutants not installed (skipping; cargo install cargo-mutants)"
fi

# 4. Benchmarks (criterion)
# Per-bench invocation sidesteps the cargo bench --workspace libtest-harness
# conflict on bin crates (documented in
# thoughts/shared/research/2026-04-26-hardening-phase-0-1-execution-summary.md).
# Mirrors the .cargo/config.toml `bench-all` alias.
echo "[*] Running benchmarks..."
if cargo bench -p rsi-common --bench rpc_decode \
     -p rsi --bench ui_content --bench ui_height \
     -p rsid --bench store_worker_flush --bench restore_sessions \
     > /dev/null 2>&1; then
  # Parse criterion results from target/criterion. Criterion uses two layouts:
  #   target/criterion/<bench_id>/new/estimates.json                (single-fn benches)
  #   target/criterion/<group_id>/<sub_id>/new/estimates.json       (groups with parameters)
  # Walk via `find` so both layouts collapse to a flat key namespace.
  MEDIANS='{}'
  if [ -d "target/criterion" ]; then
    while IFS= read -r est_path; do
      # Compute the bench key relative to target/criterion, dropping the
      # trailing "/new/estimates.json". e.g. "ui_content_parse_content/small".
      rel="${est_path#target/criterion/}"
      bench_key="${rel%/new/estimates.json}"
      MEDIAN=$(jq '.median.point_estimate // 0' "$est_path" 2>/dev/null || echo "0")
      MEDIANS=$(echo "$MEDIANS" | jq --arg name "$bench_key" --arg median "$MEDIAN" '.[$name] = ($median | tonumber)')
    done < <(find target/criterion -path '*/new/estimates.json' 2>/dev/null)
  fi
  echo "    Benchmarks completed"
  METRICS_TMP_NEW=$(mktemp)
  jq --argjson medians "$MEDIANS" '.criterion.median_ns = $medians' "$METRICS_TMP" > "$METRICS_TMP_NEW"
  mv "$METRICS_TMP_NEW" "$METRICS_TMP"
else
  echo "    [!] cargo bench failed (continuing with null)"
fi

# 5. dhat heap profiling (optional feature)
echo "[*] Running dhat heap baseline..."
if cargo test --features dhat-heap -p rsid --test dhat_baseline -- --ignored > /dev/null 2>&1; then
  # cargo test runs with cwd = package root, so the test creates
  # `target/dhat/...` under crates/rsid, not the workspace target.
  # Check both locations.
  if [ -f "crates/rsid/target/dhat/dhat-heap-baseline.json" ]; then
    DHAT_JSON="crates/rsid/target/dhat/dhat-heap-baseline.json"
  else
    DHAT_JSON="target/dhat/dhat-heap-baseline.json"
  fi
  if [ -f "$DHAT_JSON" ]; then
    # dhat-rs JSON schema: per-program-point allocation stats live under .pps[];
    # `tb` = total bytes allocated, `tbk` = total blocks allocated.
    # The top-level has no pre-aggregated total — sum across program points.
    TOTAL_BYTES=$(jq '[.pps[].tb] | add // 0' "$DHAT_JSON" 2>/dev/null || echo "0")
    TOTAL_BLOCKS=$(jq '[.pps[].tbk] | add // 0' "$DHAT_JSON" 2>/dev/null || echo "0")
    echo "    dhat heap: $TOTAL_BYTES bytes in $TOTAL_BLOCKS blocks"
    METRICS_TMP_NEW=$(mktemp)
    jq --arg bytes "$TOTAL_BYTES" --arg blocks "$TOTAL_BLOCKS" \
      '.dhat.total_heap_bytes = ($bytes | tonumber) | .dhat.total_blocks = ($blocks | tonumber)' \
      "$METRICS_TMP" > "$METRICS_TMP_NEW"
    mv "$METRICS_TMP_NEW" "$METRICS_TMP"
  fi
else
  echo "    [!] dhat-heap feature not available or test failed (continuing with baseline zeros)"
fi

# 6. Write final metrics
echo "[*] Writing metrics/baseline.json..."
mkdir -p metrics
cp "$METRICS_TMP" metrics/baseline.json
rm -f "$METRICS_TMP"

echo "[✓] Baseline metrics captured successfully"
jq . metrics/baseline.json
