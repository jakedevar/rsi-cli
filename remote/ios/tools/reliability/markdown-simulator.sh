#!/usr/bin/env bash
# Run the real app in a throwaway iOS simulator on the offline Markdown fixture
# (#503) and screenshot each part of it, dark and light, and the zoom screen.
#
# usage: remote/ios/tools/reliability/markdown-simulator.sh <RSIRemote.app> <out-dir>
#   Build the app first, as simulator.sh says. The launch arguments are the ones the
#   phone uses: `-markdownDemo <0..7>` (0: the whole conversation, n: the question and
#   answer n), `-markdownDemoZoom 1` (open the zoom view of the inline image),
#   `-rsi.remote.appearance light`.
#
# No gateway, no network, no certificate: the demo screen makes no request. The
# simulator exists only for this run; nothing is installed on a device.
set -euo pipefail

app=${1:?usage: markdown-simulator.sh <RSIRemote.app> <out-dir>}
out=${2:?usage: markdown-simulator.sh <RSIRemote.app> <out-dir>}
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
sim=$(xcrun simctl create "rsi-markdown-$$" "$type_id" "$runtime")
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

names=(1-text 2-lists 3-table-quote 4-code 5-code-more 6-images 7-degraded)
for part in 1 2 3 4 5 6 7; do
    launch -markdownDemo "$part"
    shot "dark-${names[$((part - 1))]}"
done
for part in 3 4 6; do
    launch -markdownDemo "$part" -rsi.remote.appearance light
    shot "light-${names[$((part - 1))]}"
done
launch -markdownDemo 6 -markdownDemoZoom 1
shot "dark-zoom"
echo "markdown-simulator: screenshots in $out"
