#!/usr/bin/env bash
# The `desktop-e2e-graph` job recipe (#3279, #3000 criterion 4): run the desktop `graph-run` E2E scenario
# against an ISOLATED real rsid built with the matching rsi-e2e-seed binary, and keep a receipt that
# separates "executed and passed" from "skipped".
#
#   AgentSubmitJob {"kind":"test","params":{"recipe":"desktop-e2e-graph"}}
#
# The npm/Cargo aggregate skips the real-rsid test unless RSI_E2E_RSID is set, so its green is no proof.
# This job builds `cargo build -p rsid --features e2e-seed --bin rsid --bin rsi-e2e-seed` (debug: the
# feature is refused in release), builds the desktop dist, runs
# `node scripts/smoke.mjs --isolated-write --only graph-run`, then judges e2e-report.json with
# desktop/scripts/graph-run-receipt.mjs: graph-seeded-daemon, graph-seeded-render and
# graph-seeded-readback must all have status `pass`. The retained receipt (source SHA, daemon sha256,
# command, exit code, step statuses) is graph-run-receipt.json next to e2e-report.json in the
# directory printed as `graph-run-out=`. The default directory is under the job's TMPDIR, which the job runner
# removes after the job ends, so BOTH the raw e2e-report.json and the receipt JSON are printed into the job log
# (the durable record, between `--- e2e-report.json ... ---` markers); set RSI_GRAPH_E2E_OUT to also keep the
# files on disk. No live model/provider run and no network: the isolated rsid runs offline.
#
# Takes no argument: what runs is fixed. Any argument is refused.
set -euo pipefail

if [[ $# -ne 0 ]]; then
    echo "desktop_e2e_graph_job_args_refused: this job takes no arguments (got: $*)" >&2
    exit 2
fi

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd -- "$HERE/.." && pwd)"
cd "$REPO"

OUT="${RSI_GRAPH_E2E_OUT:-$(mktemp -d "${TMPDIR:-/tmp}/rsi-graph-e2e-out.XXXXXX")}"
mkdir -p "$OUT"
echo "graph-run-out=$OUT"
SOURCE_SHA="$(git -C "$REPO" rev-parse HEAD)"
echo "source-sha=$SOURCE_SHA"

TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
BUILD_CMD="cargo build -p rsid --features e2e-seed --bin rsid --bin rsi-e2e-seed"
echo "+ $BUILD_CMD"
$BUILD_CMD

RSID="$TARGET_DIR/debug/rsid"
SEEDER="$TARGET_DIR/debug/rsi-e2e-seed"
for bin in "$RSID" "$SEEDER"; do
    [[ -x "$bin" ]] || { echo "error: $bin was not built" >&2; exit 1; }
done

cd "$REPO/desktop"
if [[ ! -d node_modules || package-lock.json -nt node_modules ]]; then
    npm ci --no-audit --no-fund
fi
npm run build

SMOKE_CMD="node scripts/smoke.mjs --isolated-write --rsid $RSID --only graph-run --out $OUT"
echo "+ $SMOKE_CMD"
status=0
RSI_E2E_RSID="$RSID" RSI_E2E_SEEDER="$SEEDER" \
    node scripts/smoke.mjs --isolated-write --rsid "$RSID" --only graph-run --out "$OUT" || status=$?

# The default OUT lives under the job's TMPDIR, removed when the job ends: echo the raw report into the durable log.
echo "--- e2e-report.json (retained in this log) ---"
if [[ -f "$OUT/e2e-report.json" ]]; then cat "$OUT/e2e-report.json"; else echo "e2e-report.json missing"; fi
echo
echo "--- end e2e-report.json ---"

judge=0
node scripts/graph-run-receipt.mjs --report "$OUT/e2e-report.json" --out "$OUT/graph-run-receipt.json" \
    --source-sha "$SOURCE_SHA" --command "$SMOKE_CMD" --exit-code "$status" --rsid "$RSID" || judge=$?
echo "--- graph-run-receipt.json (retained in this log) ---"
cat "$OUT/graph-run-receipt.json" || true
echo "--- end receipt ---"
echo "desktop-e2e-graph-job: smoke_exit=$status receipt_exit=$judge"
if [[ "$status" -ne 0 ]]; then exit "$status"; fi
exit "$judge"
