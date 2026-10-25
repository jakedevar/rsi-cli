#!/usr/bin/env bash
# Reproduce RSI Remote reliability failures on a Mac (#350): build and start
# the fault-injecting fake gateway, build the app's model and client for macOS
# with the harness, and run every scenario (or the ones named).
#
# usage: remote/ios/tools/reliability/run.sh [scenario ...]
#        remote/ios/tools/reliability/run.sh soak <host> <minutes>
#   RSI_RELIABILITY_STATUS=1   also print the fixed app's status lines
#   RSI_RELIABILITY_APP=<dir>  the RSIRemote sources to build (default: this tree's)
#   RSI_FAKE_PORT=8443         port for the fake gateway (localhost only)
#
# `soak` reads a real gateway instead, read-only, one refresh cycle per 30 s,
# and logs each failed request by method, status and UTC time (no ids).
# Needs openssl and the Xcode toolchain (swiftc). Touches nothing outside a
# temporary directory; the fake gateway is stopped on exit.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
app="${RSI_RELIABILITY_APP:-$here/../../RSIRemote}"
port="${RSI_FAKE_PORT:-8443}"
work=$(mktemp -d "${TMPDIR:-/tmp}/rsi-reliability.XXXXXX")
gateway_pid=""

cleanup() {
    if [ -n "$gateway_pid" ]; then
        kill "$gateway_pid" 2> /dev/null || true
        wait "$gateway_pid" 2> /dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

flags=()
[ "${RSI_RELIABILITY_STATUS:-0}" = 1 ] && flags+=(-D RELIABILITY_STATUS)
swiftc -swift-version 6 -parse-as-library ${flags[@]+"${flags[@]}"} \
    "$app"/Model/*.swift "$app"/Net/*.swift \
    "$app"/App/AppModel.swift "$app"/App/ManagerChat.swift "$app"/App/AnswerDrafts.swift \
    "$here/Harness.swift" -o "$work/harness"

if [ "${1:-}" = soak ]; then
    "$work/harness" "$@"
    exit
fi

"$here/fake-gateway.sh" "$work" "$port"
gateway_pid=$(cat "$work/gateway.pid")
RSI_FAKE_GATEWAY="localhost:$port" "$work/harness" "$@"
