#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
source "$ROOT/scripts/rsid-health.sh"
# The probe is the rsi-repo-guard `rsid-health-probe` subcommand (#465).
RSI_REPO_GUARD_BIN="$("$ROOT/scripts/repo-guard" --bin)"
export RSI_REPO_GUARD_BIN
TEMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TEMP_DIR"' EXIT

cat >"$TEMP_DIR/rpc" <<'MOCK'
#!/usr/bin/env bash
count=0
[[ ! -f "$RSI_TEST_COUNT" ]] || count="$(cat "$RSI_TEST_COUNT")"
count=$((count + 1))
printf '%s' "$count" >"$RSI_TEST_COUNT"
if ((count >= 3)); then
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"persistence_queue_capacity":1,"project_cache_size":0}}'
else
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
fi
MOCK
chmod +x "$TEMP_DIR/rpc"
export RSI_TEST_COUNT="$TEMP_DIR/count"
wait_for_rsid_health "$TEMP_DIR/rpc" "$TEMP_DIR/socket" 5 >"$TEMP_DIR/progress"
[[ "$(cat "$RSI_TEST_COUNT")" -eq 3 ]]
grep -q 'Waiting for rsid health:' "$TEMP_DIR/progress"
grep -q 'rsid healthy after' "$TEMP_DIR/progress"

cat >"$TEMP_DIR/bad-rpc" <<'MOCK'
#!/usr/bin/env bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
MOCK
chmod +x "$TEMP_DIR/bad-rpc"
if wait_for_rsid_health "$TEMP_DIR/bad-rpc" "$TEMP_DIR/socket" 1 >"$TEMP_DIR/timeout" 2>&1; then
    echo 'health wait accepted an incomplete RPC response' >&2
    exit 1
fi
grep -q 'within 1s' "$TEMP_DIR/timeout"

# Negative: with no probe binary anywhere the wait fails at once and names the gap;
# a missing probe is never read as healthy.
(
    RSI_REPO_GUARD_BIN="$TEMP_DIR/absent"
    RSI_GUARD_BIN=""
    RSI_GUARD_SCRIPT_DIR="$TEMP_DIR"
    HOME="$TEMP_DIR"
    RSI_HOME_DIR="$TEMP_DIR/home"
    started=$SECONDS
    if wait_for_rsid_health "$TEMP_DIR/rpc" "$TEMP_DIR/socket" 30 >"$TEMP_DIR/noguard" 2>&1; then
        echo 'health wait passed without a probe binary' >&2
        exit 1
    fi
    ((SECONDS - started < 5)) || { echo 'missing probe binary was retried' >&2; exit 1; }
    grep -q 'rsi-repo-guard with rsid-health-probe not found' "$TEMP_DIR/noguard"
)
echo 'rsid health wait passed'
