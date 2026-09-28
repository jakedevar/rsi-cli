#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
source "$ROOT/scripts/rsid-health.sh"
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
echo 'rsid health wait passed'
