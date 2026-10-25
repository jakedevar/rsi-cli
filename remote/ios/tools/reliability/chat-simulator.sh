#!/usr/bin/env bash
# Run the real app in a throwaway iOS simulator against the fake gateway and
# screenshot the manager conversation (#502): dark, new manager output streaming
# in at the bottom, light, and a very long conversation.
#
# usage: remote/ios/tools/reliability/chat-simulator.sh <RSIRemote.app> <out-dir>
#   Build the app first, as simulator.sh says.
#
# The simulator, its trusted test CA and the gateway exist only for this run;
# nothing is installed on a device and no other simulator is touched.
set -euo pipefail

app=${1:?usage: chat-simulator.sh <RSIRemote.app> <out-dir>}
out=${2:?usage: chat-simulator.sh <RSIRemote.app> <out-dir>}
here=$(cd "$(dirname "$0")" && pwd)
port="${RSI_FAKE_PORT:-8462}"
os="${RSI_SIM_OS:-26.5}"
device="${RSI_SIM_DEVICE:-iPhone 17 Pro}"
work=$(mktemp -d "${TMPDIR:-/tmp}/rsi-chat-sim.XXXXXX")
gateway_pid=""
sim=""
mkdir -p "$out"

cleanup() {
    if [ -n "$gateway_pid" ]; then
        kill "$gateway_pid" 2> /dev/null || true
        wait "$gateway_pid" 2> /dev/null || true
    fi
    if [ -n "$sim" ]; then
        xcrun simctl shutdown "$sim" > /dev/null 2>&1 || true
        xcrun simctl delete "$sim" > /dev/null 2>&1 || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=rsi chat test CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign" \
    -keyout "$work/ca.key" -out "$work/ca.pem" > /dev/null 2>&1
"$here/fake-gateway.sh" "$work" "$port" "$work/ca.pem" "$work/ca.key"
gateway_pid=$(cat "$work/gateway.pid")
control() { curl -sk -m 5 -X POST -d "$1" "https://localhost:$port/__control" > /dev/null; }

type_id=$(xcrun simctl list devicetypes | grep -F "$device (com.apple" | head -1 |
    sed -E 's/.*\((com\.apple[^)]*)\).*/\1/')
runtime=$(xcrun simctl list runtimes available | grep -F "iOS $os (" | head -1 |
    sed -E 's/.* - (com\.apple\.CoreSimulator\.SimRuntime\.[^ ]*).*/\1/')
sim=$(xcrun simctl create "rsi-chat-$$" "$type_id" "$runtime")
xcrun simctl boot "$sim"
xcrun simctl bootstatus "$sim" -b > /dev/null
xcrun simctl keychain "$sim" add-root-cert "$work/ca.pem"
xcrun simctl install "$sim" "$app"
bundle=$(/usr/libexec/PlistBuddy -c "Print CFBundleIdentifier" "$app/Info.plist")
shot() { xcrun simctl io "$sim" screenshot "$out/$1.png" > /dev/null 2>&1; }
launch() {
    xcrun simctl terminate "$sim" "$bundle" > /dev/null 2>&1 || true
    xcrun simctl launch "$sim" "$bundle" -gatewayHost "localhost:$port" -initialTab projects \
        -outboxOpenChat 1 "$@" > /dev/null
}

control '{"reset":true,"projects":1,"conversation":40}'
launch
sleep 12
shot 1-dark-opens-at-latest
# New manager output arrives while the reader is at the bottom: it follows.
control '{"append":3}'
sleep 14
shot 2-dark-streamed-follows
launch -rsi.remote.appearance light
sleep 12
shot 3-light
control '{"conversation":3000}'
launch
sleep 14
shot 4-dark-3000-items
echo "chat-simulator: screenshots in $out"
