#!/usr/bin/env bash
# Run the real app in a throwaway iOS simulator on the offline provider usage
# fixture (#504) and screenshot each state, dark and light.
#
# usage: remote/ios/tools/reliability/usage-simulator.sh <RSIRemote.app> <out-dir>
#   Build the app first, as simulator.sh says. The launch arguments are the ones the
#   phone uses: `-providerUsageFixture full|nodata|sparse|empty|failing` and
#   `-rsi.remote.appearance light|dark`.
#
# No gateway, no network, no certificate: the fixture screen makes no request and
# touches no saved host. The simulator exists only for this run; nothing is
# installed on a device.
set -euo pipefail

app=${1:?usage: usage-simulator.sh <RSIRemote.app> <out-dir>}
out=${2:?usage: usage-simulator.sh <RSIRemote.app> <out-dir>}
os="${RSI_SIM_OS:-26.5}"
device="${RSI_SIM_DEVICE:-iPhone 17 Pro}"
sim=""
mkdir -p "$out"

cleanup() {
    if [ -n "$sim" ]; then
        xcrun simctl shutdown "$sim" > /dev/null 2>&1 || true
        xcrun simctl delete "$sim" > /dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

type_id=$(xcrun simctl list devicetypes | grep -F "$device (com.apple" | head -1 |
    sed -E 's/.*\((com\.apple[^)]*)\).*/\1/')
runtime=$(xcrun simctl list runtimes available | grep -F "iOS $os (" | head -1 |
    sed -E 's/.* - (com\.apple\.CoreSimulator\.SimRuntime\.[^ ]*).*/\1/')
sim=$(xcrun simctl create "rsi-usage-$$" "$type_id" "$runtime")
xcrun simctl boot "$sim"
xcrun simctl bootstatus "$sim" -b > /dev/null
xcrun simctl install "$sim" "$app"
bundle=$(/usr/libexec/PlistBuddy -c "Print CFBundleIdentifier" "$app/Info.plist")
shot() { xcrun simctl io "$sim" screenshot "$out/$1.png" > /dev/null 2>&1; }
launch() {
    xcrun simctl terminate "$sim" "$bundle" > /dev/null 2>&1 || true
    xcrun simctl launch "$sim" "$bundle" "$@" > /dev/null
    sleep 4
}

for variant in full nodata sparse empty failing; do
    launch -providerUsageFixture "$variant" -rsi.remote.appearance dark
    shot "dark-$variant"
done
for variant in full sparse; do
    launch -providerUsageFixture "$variant" -rsi.remote.appearance light
    shot "light-$variant"
done
echo "usage-simulator: screenshots in $out"
