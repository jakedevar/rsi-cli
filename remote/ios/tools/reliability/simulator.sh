#!/usr/bin/env bash
# Run the real app in a throwaway iOS simulator against the fake gateway
# through an rsid outage and take screenshots (#350):
#   1 the dashboard, 2 rsid down under it (a deploy restart), 3 a cold start
#   during the outage, 4 rsid back.
#
# usage: remote/ios/tools/reliability/simulator.sh <RSIRemote.app> <out-dir>
#   Build the app first, e.g.
#   xcodebuild build -project remote/ios/RSIRemote.xcodeproj -scheme RSIRemote \
#     -destination 'generic/platform=iOS Simulator' \
#     -derivedDataPath remote/ios/build/DerivedData CODE_SIGNING_ALLOWED=NO
#
# The simulator, its trusted test CA and the gateway exist only for this run;
# nothing is installed on a device and no other simulator is touched.
set -euo pipefail

app=${1:?usage: simulator.sh <RSIRemote.app> <out-dir>}
out=${2:?usage: simulator.sh <RSIRemote.app> <out-dir>}
here=$(cd "$(dirname "$0")" && pwd)
port="${RSI_FAKE_PORT:-8461}"
os="${RSI_SIM_OS:-26.5}"
device="${RSI_SIM_DEVICE:-iPhone 17 Pro}"
work=$(mktemp -d "${TMPDIR:-/tmp}/rsi-reliability-sim.XXXXXX")
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

# A throwaway CA, trusted by this simulator only; the gateway leaf is signed by it.
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=rsi reliability test CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign" \
    -keyout "$work/ca.key" -out "$work/ca.pem" > /dev/null 2>&1
"$here/fake-gateway.sh" "$work" "$port" "$work/ca.pem" "$work/ca.key"
gateway_pid=$(cat "$work/gateway.pid")
control() { curl -sk -m 5 -X POST -d "$1" "https://localhost:$port/__control" > /dev/null; }

type_id=$(xcrun simctl list devicetypes | grep -F "$device (com.apple" | head -1 |
    sed -E 's/.*\((com\.apple[^)]*)\).*/\1/')
runtime=$(xcrun simctl list runtimes available | grep -F "iOS $os (" | head -1 |
    sed -E 's/.* - (com\.apple\.CoreSimulator\.SimRuntime\.[^ ]*).*/\1/')
sim=$(xcrun simctl create "rsi-reliability-$$" "$type_id" "$runtime")
# Boot it with the simulator's crashing Family service disabled first (#509).
. "$here/../../../../scripts/ios/sim-quiet-family.sh"
sim_quiet_family "$sim"
xcrun simctl bootstatus "$sim" -b > /dev/null
xcrun simctl keychain "$sim" add-root-cert "$work/ca.pem"
xcrun simctl install "$sim" "$app"
bundle=$(/usr/libexec/PlistBuddy -c "Print CFBundleIdentifier" "$app/Info.plist")
shot() { xcrun simctl io "$sim" screenshot "$out/$1.png" > /dev/null 2>&1; }

control '{"reset":true,"projects":3,"waiting":2}'
# The host comes from the launch arguments (the UserDefaults argument domain).
xcrun simctl launch "$sim" "$bundle" -gatewayHost "localhost:$port" > /dev/null
sleep 6
shot 1-dashboard
# rsid goes down for two minutes; the dashboard's 30 s refresh meets it.
control '{"down_for_s":120}'
sleep 36
shot 2-rsid-down-under-data
xcrun simctl terminate "$sim" "$bundle" || true
xcrun simctl launch "$sim" "$bundle" -gatewayHost "localhost:$port" > /dev/null
sleep 8
shot 3-cold-start-during-outage
control '{"down_for_s":0}'
sleep 14
shot 4-rsid-back
echo "simulator: screenshots in $out"
