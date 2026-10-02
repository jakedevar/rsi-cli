#!/usr/bin/env bash
# Write a tar stream of log excerpts for every failed lane of one cloud sweep.
#
#   cloud-sweep-excerpts.sh RESULT_DIR > excerpts.tar
#
# RESULT_DIR is the host's sweeps/results/SHA (status/LANE.tsv holds
# "LANE<TAB>EXIT", logs/LANE.log holds the lane output). The gate host is
# destroyed right after collect, and the full lane logs stay on it, so
# `cloud-sweep.sh collect` pipes this through ssh and unpacks the tar into
# results/SHA/failure-logs/. One excerpt per failed lane: the failing test
# names, every panic with context, and the log tail (Issue #1060). The full
# log rides along gzipped as LANE.log.gz so the laptop can classify every red
# with rsi-known-failure (Issue #1016).
set -euo pipefail
[[ $# -eq 1 && -d "$1/status" ]] || { echo 'usage: cloud-sweep-excerpts.sh RESULT_DIR' >&2; exit 2; }
result="$1"
out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
for status in "$result"/status/*.tsv; do
  [[ -f "$status" ]] || continue
  IFS=$'\t' read -r label code <"$status" || true
  [[ -n "${label:-}" && "${code:-0}" != 0 ]] || continue
  log="$result/logs/$label.log"
  {
    echo "# lane=$label exit=$code"
    if [[ -f "$log" ]]; then
      echo "# failing tests"
      grep -E '^\s*(FAIL|SIGABRT|SIGSEGV|TIMEOUT|LEAK)\b|^test .* FAILED$|^    [A-Za-z0-9_:]+$' "$log" | sort -u | head -100 || true
      echo "# panics (with context)"
      grep -n -B2 -A14 'panicked at' "$log" | head -600 || true
      echo "# tail"
      tail -n 300 "$log"
    else
      echo "# no log file for this lane"
    fi
  } >"$out/$label.txt" 2>&1 || true
  [[ ! -f "$log" ]] || gzip -c "$log" >"$out/$label.log.gz" 2>/dev/null || true
done
tar -C "$out" -cf - .
