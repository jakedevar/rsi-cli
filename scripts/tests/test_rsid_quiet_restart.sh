#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
source "$ROOT/scripts/rsid-quiet-restart.sh"
TEMP_DIR="$(mktemp -d /tmp/rqr.XXXXXX)"
LISTENER_PID=""
trap '[[ -z "$LISTENER_PID" ]] || kill "$LISTENER_PID" 2>/dev/null || true; rm -rf "$TEMP_DIR"' EXIT

# The caller supplies rsid_running_pids; the test controls it.
RSID_PIDS=""
rsid_running_pids() { printf '%s' "$RSID_PIDS"; }

mock_rpc() {
    local name="$1" body="$2"
    cat >"$TEMP_DIR/$name" <<MOCK
#!/usr/bin/env bash
printf '%s\n' "\$*" >"$TEMP_DIR/$name.args"
printf '%s\n' '$body'
MOCK
    chmod +x "$TEMP_DIR/$name"
}

status_of() {
    local rc=0
    "$@" >"$TEMP_DIR/out" 2>"$TEMP_DIR/err" || rc=$?
    echo "$rc"
}

# A live socket: a unix listener that accepts and holds connections.
SOCKET="$TEMP_DIR/live.sock"
python3 - "$SOCKET" <<'PY' &
import socket, sys, time
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(sys.argv[1])
server.listen(16)
while True:
    time.sleep(1)
PY
LISTENER_PID=$!
for _ in $(seq 50); do [[ -S "$SOCKET" ]] && break; sleep 0.1; done
[[ -S "$SOCKET" ]]

# A staged request: pending, the caller must not restart directly.
mock_rpc pending '{"jsonrpc":"2.0","id":1,"result":{"pending":true,"sha":"abc","release_by":"2026-10-03T00:00:00Z"}}'
[[ "$(status_of request_quiet_restart "$TEMP_DIR/pending" "$SOCKET" /build 600)" -eq 0 ]]
grep -q 'Restart pending' "$TEMP_DIR/out"
grep -q 'release by 2026-10-03T00:00:00Z' "$TEMP_DIR/out"
grep -q 'RequestOperatorRestart' "$TEMP_DIR/pending.args"
grep -q '"binaries_dir": "/build"' "$TEMP_DIR/pending.args"
grep -q '"max_wait_secs": 600' "$TEMP_DIR/pending.args"

# Answered "not under the supervisor": a direct restart is the old behaviour.
mock_rpc unsupervised '{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"deploy_needs_supervisor"}}'
[[ "$(status_of request_quiet_restart "$TEMP_DIR/unsupervised" "$SOCKET" /build)" -eq 10 ]]

# A reachable daemon whose reply is empty, malformed or not JSON-RPC is
# ambiguous: never restart it, name NOW=1.
mock_rpc empty ''
mock_rpc malformed 'not json at all'
mock_rpc notrpc '{"hello":"world"}'
for name in empty malformed notrpc; do
    [[ "$(status_of request_quiet_restart "$TEMP_DIR/$name" "$SOCKET" /build)" -eq 1 ]]
    grep -q 'NOW=1' "$TEMP_DIR/err"
done

# A missing rsi-rpc binary against a live socket is ambiguous too.
[[ "$(status_of request_quiet_restart "$TEMP_DIR/missing-rpc" "$SOCKET" /build)" -eq 1 ]]
grep -q 'NOW=1' "$TEMP_DIR/err"

# An RPC that never answers (timeout) is ambiguous; shorten the wait.
cat >"$TEMP_DIR/hang" <<'MOCK'
#!/usr/bin/env bash
sleep 30
MOCK
chmod +x "$TEMP_DIR/hang"
SLOW_SCRIPT="$(sed 's/timeout=120/timeout=1/' "$ROOT/scripts/rsid-quiet-restart.sh")"
(
    eval "$SLOW_SCRIPT"
    [[ "$(status_of request_quiet_restart "$TEMP_DIR/hang" "$SOCKET" /build)" -eq 1 ]]
    grep -q 'NOW=1' "$TEMP_DIR/err"
)

# Another refusal stops the install and names NOW=1.
mock_rpc busy '{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"deploy_already_in_progress"}}'
[[ "$(status_of request_quiet_restart "$TEMP_DIR/busy" "$SOCKET" /build)" -eq 1 ]]
grep -q 'deploy_already_in_progress' "$TEMP_DIR/err"
grep -q 'NOW=1' "$TEMP_DIR/err"

# Positive evidence the daemon is down (socket absent AND no rsid process):
# a direct restart is safe, and the RPC is never attempted.
mock_rpc never '{"jsonrpc":"2.0","id":1,"result":{}}'
rm -f "$TEMP_DIR/never.args"
RSID_PIDS=""
[[ "$(status_of request_quiet_restart "$TEMP_DIR/never" "$TEMP_DIR/absent.sock" /build)" -eq 10 ]]
[[ ! -e "$TEMP_DIR/never.args" ]]

# A stale socket file that refuses connections is down evidence as well.
python3 - "$TEMP_DIR/stale.sock" <<'PY'
import socket, sys
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(sys.argv[1])
server.close()
PY
[[ "$(status_of request_quiet_restart "$TEMP_DIR/never" "$TEMP_DIR/stale.sock" /build)" -eq 10 ]]

# Socket down but an rsid process lives: ambiguous, never restart.
RSID_PIDS="4242"
[[ "$(status_of request_quiet_restart "$TEMP_DIR/never" "$TEMP_DIR/absent.sock" /build)" -eq 1 ]]
grep -q 'NOW=1' "$TEMP_DIR/err"
[[ ! -e "$TEMP_DIR/never.args" ]]

# The prior installed executable must survive the build. Install paths that
# are symlinks into Cargo's output directory would otherwise be overwritten by
# the build, so the daemon's `.prev` would hold the NEW executable.
BIN="$TEMP_DIR/bin"; REL="$TEMP_DIR/target/release"; STABLE="$TEMP_DIR/install"
mkdir -p "$BIN" "$REL"
printf 'OLD-rsid' >"$REL/rsid"; chmod 755 "$REL/rsid"
printf 'OLD-rpc' >"$REL/rsi-rpc"; chmod 755 "$REL/rsi-rpc"
ln -s "$REL/rsid" "$BIN/rsid"
ln -s "$REL/rsi-rpc" "$BIN/rsi-rpc"
printf 'elsewhere' >"$TEMP_DIR/other-rsi"; chmod 755 "$TEMP_DIR/other-rsi"
ln -s "$TEMP_DIR/other-rsi" "$BIN/rsi"
stabilize_installed_binaries "$BIN" "$REL" "$STABLE"
# Cargo rebuilds into its output directory.
printf 'NEW-rsid' >"$REL/rsid"
printf 'NEW-rpc' >"$REL/rsi-rpc"
[[ "$(cat "$BIN/rsid")" == OLD-rsid ]]
[[ "$(cat "$BIN/rsi-rpc")" == OLD-rpc ]]
[[ "$(readlink -f "$BIN/rsid")" == "$STABLE/rsid" ]]
[[ -x "$STABLE/rsid" ]]
# An install path that never pointed into the build output is left alone.
[[ "$(readlink "$BIN/rsi")" == "$TEMP_DIR/other-rsi" ]]
# Absent install paths are skipped, not created.
[[ ! -e "$BIN/rsi-remote" ]]
# Idempotent: a second run keeps the stable copy.
stabilize_installed_binaries "$BIN" "$REL" "$STABLE"
[[ "$(cat "$BIN/rsid")" == OLD-rsid ]]
# The daemon's swap (rename dest to .prev, install the new file) now keeps the
# genuinely old executable as the rollback target.
DEST="$(readlink -f "$BIN/rsid")"
mv "$DEST" "$DEST.prev"
cp "$REL/rsid" "$DEST"
[[ "$(cat "$DEST.prev")" == OLD-rsid ]]
[[ "$(cat "$BIN/rsid")" == NEW-rsid ]]

echo 'rsid quiet restart request passed'
