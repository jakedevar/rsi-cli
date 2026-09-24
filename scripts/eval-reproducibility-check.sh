#!/usr/bin/env bash
# RSI-006: verify aggregate-metric reproducibility across repeated eval runs.
# Runs `rsi-eval` three times against the same isolated daemon, captures each
# baseline, computes `(max - min) / median` for every aggregate metric, and
# fails if any spread exceeds the configured threshold ratio.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

RUNS="${RSI_EVAL_RUNS:-3}"
THRESHOLD_RATIO="${RSI_EVAL_REPRO_THRESHOLD_RATIO:-0.05}"
CORPUS="${RSI_EVAL_CORPUS:-default}"
DEFAULT_HARNESS=$(git rev-parse --short HEAD 2>/dev/null || echo "repro-check")
HARNESS="${RSI_EVAL_HARNESS:-$DEFAULT_HARNESS}"

if ! command -v jq >/dev/null 2>&1; then
    echo "error: jq is required for scripts/eval-reproducibility-check.sh" >&2
    exit 1
fi

case "$RUNS" in
    ''|*[!0-9]*)
        echo "error: RSI_EVAL_RUNS must be an integer >= 3 (got $RUNS)" >&2
        exit 1
        ;;
esac

if [ "$RUNS" -lt 3 ]; then
    echo "error: RSI_EVAL_RUNS must be at least 3 (got $RUNS)" >&2
    exit 1
fi

if ! awk -v value="$THRESHOLD_RATIO" 'BEGIN { exit !(value == value + 0 && value >= 0) }'; then
    echo "error: RSI_EVAL_REPRO_THRESHOLD_RATIO must be a non-negative number (got $THRESHOLD_RATIO)" >&2
    exit 1
fi

if [ -z "${RSI_DAEMON_SOCKET_PATH:-}" ]; then
    echo "error: set RSI_DAEMON_SOCKET_PATH to an isolated daemon socket before running this script" >&2
    exit 1
fi

if [ ! -S "$RSI_DAEMON_SOCKET_PATH" ]; then
    echo "error: RSI_DAEMON_SOCKET_PATH does not point to a live Unix socket: $RSI_DAEMON_SOCKET_PATH" >&2
    exit 1
fi

if [ -n "${RSI_EVAL_ARTIFACT_DIR:-}" ]; then
    ARTIFACT_DIR="$RSI_EVAL_ARTIFACT_DIR"
    mkdir -p "$ARTIFACT_DIR"
else
    ARTIFACT_DIR=$(mktemp -d -t rsi-eval-repro.XXXXXX)
fi

echo "rsi-eval reproducibility check"
echo "  socket:    $RSI_DAEMON_SOCKET_PATH"
echo "  corpus:    $CORPUS"
echo "  harness:   $HARNESS"
echo "  runs:      $RUNS"
echo "  threshold: $THRESHOLD_RATIO"
echo "  artifacts: $ARTIFACT_DIR"

for run in $(seq 1 "$RUNS"); do
    baseline_path="$ARTIFACT_DIR/run-$run.json"
    report_md_path="$ARTIFACT_DIR/run-$run.md"
    report_json_path="$ARTIFACT_DIR/run-$run.report.json"

    echo "[run $run/$RUNS] capturing $baseline_path"
    cargo run -q -p rsi-eval -- \
        --harness "$HARNESS" \
        --corpus "$CORPUS" \
        --capture-baseline "$baseline_path" \
        --report-md "$report_md_path" \
        --report-json "$report_json_path" \
        "$@"
done

summary_path="$ARTIFACT_DIR/summary.json"
jq -s '
def median:
  sort as $sorted
  | length as $n
  | if $n == 0 then
      0
    elif ($n % 2) == 1 then
      $sorted[$n / 2]
    else
      (($sorted[$n / 2 - 1] + $sorted[$n / 2]) / 2)
    end;

map(.aggregate) as $runs
| [
    ($runs[0] | keys[]) as $metric
    | [ $runs[].[$metric] ] as $values
    | ($values | min) as $min
    | ($values | max) as $max
    | ($values | median) as $median
    | {
        metric: $metric,
        values: $values,
        min: $min,
        median: $median,
        max: $max,
        spread: (
          if $median == 0 then
            if $max == $min then 0 else 1000000000 end
          else
            (($max - $min) / $median)
          end
        )
      }
  ]
' "$ARTIFACT_DIR"/run-*.json > "$summary_path"

printf '%-28s %-12s %-12s %-12s %-12s\n' "metric" "min" "median" "max" "spread"
jq -r '
  .[]
  | [
      .metric,
      (.min | tostring),
      (.median | tostring),
      (.max | tostring),
      (.spread | tostring)
    ]
  | @tsv
' "$summary_path" | while IFS=$'\t' read -r metric min median max spread; do
    printf '%-28s %-12s %-12s %-12s %-12s\n' "$metric" "$min" "$median" "$max" "$spread"
done

if jq -e --argjson threshold "$THRESHOLD_RATIO" 'all(.[]; .spread <= $threshold)' "$summary_path" >/dev/null; then
    echo "PASS: all aggregate metrics stayed within the reproducibility threshold"
    exit 0
fi

echo "FAIL: one or more aggregate metrics exceeded the reproducibility threshold" >&2
jq -r --argjson threshold "$THRESHOLD_RATIO" '
  .[]
  | select(.spread > $threshold)
  | "- \(.metric): spread=\(.spread) values=\(.values)"
' "$summary_path" >&2
echo "artifacts preserved at $ARTIFACT_DIR" >&2
exit 1
