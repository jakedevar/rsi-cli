#!/usr/bin/env bash
# Build RSI Remote for the operator's iPhone and install it (hub #2963).
#
# usage: scripts/ios/dev-install.sh [--dry-run] [--network-first] [--device ID]
#                                   [--no-launch] [--show-ids]
#
#   --dry-run        print the exact generate, build, install and launch commands
#                    and exit 0 without running them
#   --network-first  accept a paired phone that is reachable over the network
#                    (not only by cable); devicectl picks the transport
#   --device ID      install to this device instead of the first paired iPhone
#   --no-launch      install only
#   --show-ids       print the team and device ids instead of masking them
#
# Exit codes: 0 installed; 2 usage or missing tool; 3 no paired iPhone is
# reachable (or it is locked); 4 no signing team; any other non-zero is the failing
# xcodegen/xcodebuild/devicectl step's code.
#
# Signing is the operator's free Apple Personal Team; nothing beyond it.
# The team id comes from, in order: $RSI_IOS_TEAM_ID, ~/.config/rsi-ios/env
# (shell `KEY=value` lines), or Xcode's own account (the signed-in Personal
# Team). The device id comes from --device, $RSI_IOS_DEVICE_ID or
# `xcrun devicectl list devices`. None of them is ever written to the repo.
# The bundle id defaults to dev.rsi.remote.<6 hex of the team id's sha256>
# (free teams need a unique id); set RSI_IOS_BUNDLE_ID to override.
# Runs on macOS /bin/bash 3.2.
set -euo pipefail

dry_run=0
network_first=0
launch=1
show_ids=0
device="${RSI_IOS_DEVICE_ID:-}"
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=1 ;;
        --network-first) network_first=1 ;;
        --no-launch) launch=0 ;;
        --show-ids) show_ids=1 ;;
        --device)
            [ $# -ge 2 ] || { echo "dev-install: --device needs an id" >&2; exit 2; }
            device="$2"
            shift
            ;;
        -h | --help) sed -n '2,27p' "$0"; exit 0 ;;
        *) echo "dev-install: unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done

top=$(cd "$(dirname "$0")/../.." && pwd)
project_dir="$top/remote/ios"
derived="$project_dir/build/DerivedData"
app="$derived/Build/Products/Debug-iphoneos/RSIRemote.app"
logdir="${RSI_IOS_LOG_DIR:-$HOME/.rsi/logs}"

if [ -f "$HOME/.config/rsi-ios/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.config/rsi-ios/env"
fi

for tool in xcodegen xcodebuild xcrun python3; do
    command -v "$tool" > /dev/null 2>&1 || { echo "dev-install: $tool not found" >&2; exit 2; }
done

mask() {
    if [ "$show_ids" -eq 1 ]; then printf '%s' "$1"; else printf '<%s>' "$2"; fi
}

# The signed-in Personal Team from Xcode's preferences (never printed).
xcode_team() {
    plutil -extract IDEProvisioningTeamByIdentifier json -o - \
        "$HOME/Library/Preferences/com.apple.dt.Xcode.plist" 2> /dev/null \
        | python3 -I -c '
import json, sys
try:
    accounts = json.load(sys.stdin)
except ValueError:
    sys.exit(0)
teams = [t for ts in accounts.values() for t in ts if t.get("isFreeProvisioningTeam")]
if teams:
    print(teams[0]["teamID"])
' || true
}

team="${RSI_IOS_TEAM_ID:-}"
[ -n "$team" ] || team=$(xcode_team)
if [ -z "$team" ]; then
    echo "dev-install: no signing team. Sign in to Xcode (Settings > Accounts) with the" >&2
    echo "  operator's Apple ID, open remote/ios/RSIRemote.xcodeproj once so Xcode lists" >&2
    echo "  its Personal Team, or set RSI_IOS_TEAM_ID." >&2
    exit 4
fi

bundle="${RSI_IOS_BUNDLE_ID:-}"
if [ -z "$bundle" ]; then
    suffix=$(printf '%s' "$team" | shasum -a 256 | cut -c1-6)
    bundle="dev.rsi.remote.$suffix"
fi

# The first paired physical iPhone (cable, or network with --network-first).
find_device() {
    local json
    json=$(mktemp "${TMPDIR:-/tmp}/rsi-devices.XXXXXX")
    if ! xcrun devicectl list devices --json-output "$json" > /dev/null 2>&1; then
        rm -f "$json"
        return 0
    fi
    NETWORK_FIRST="$network_first" python3 -I - "$json" << 'EOF' || true
import json, os, sys
with open(sys.argv[1]) as handle:
    devices = json.load(handle).get("result", {}).get("devices", [])
network_first = os.environ.get("NETWORK_FIRST") == "1"
for d in devices:
    hw = d.get("hardwareProperties", {})
    conn = d.get("connectionProperties", {})
    if hw.get("platform") != "iOS" or hw.get("reality") != "physical":
        continue
    if conn.get("pairingState") != "paired":
        continue
    transport = conn.get("transportType")
    tunnel = conn.get("tunnelState")
    reachable = transport == "wired" or tunnel == "connected"
    if network_first:
        reachable = reachable or transport == "localNetwork"
    if reachable:
        print(d["identifier"])
        break
EOF
    rm -f "$json"
}

if [ -z "$device" ]; then
    device=$(find_device)
fi

generate=(xcodegen generate --quiet --spec "$project_dir/project.yml")
build=(xcodebuild build -project "$project_dir/RSIRemote.xcodeproj" -scheme RSIRemote
    -configuration Debug -destination "id=${device:-DEVICE}" -derivedDataPath "$derived"
    -allowProvisioningUpdates -allowProvisioningDeviceRegistration
    "DEVELOPMENT_TEAM=$team" "RSI_IOS_BUNDLE_ID=$bundle")
install=(xcrun devicectl device install app --device "${device:-DEVICE}" "$app")
run=(xcrun devicectl device process launch --device "${device:-DEVICE}" "$bundle")

# Print a command with the team and device ids masked (unless --show-ids).
show() {
    local word out=""
    for word in "$@"; do
        word=${word//$team/$(mask "$team" team)}
        if [ -n "$device" ]; then word=${word//$device/$(mask "$device" device)}; fi
        out="$out ${word}"
    done
    printf '%s\n' "${out# }"
}

# Mask the team id, the devicectl id and any UDID/UUID in tool output.
scrub() {
    if [ "$show_ids" -eq 1 ]; then cat "$1"; return; fi
    sed -E -e "s/$team/<team>/g" \
        -e 's/[0-9A-Fa-f]{8}-[0-9A-Fa-f]{16}/<device>/g' \
        -e 's/[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}/<device>/g' "$1"
}

if [ "$dry_run" -eq 1 ]; then
    show "${generate[@]}"
    show "${build[@]}"
    show "${install[@]}"
    if [ "$launch" -eq 1 ]; then show "${run[@]}"; fi
    exit 0
fi

if [ -z "$device" ]; then
    echo "dev-install: no paired iPhone is reachable (cable it, or pair it once and use --network-first)" >&2
    exit 3
fi

mkdir -p "$logdir"
build_log="$logdir/ios-build.log"
echo "dev-install: generating the Xcode project"
"${generate[@]}"
echo "dev-install: building for $(mask "$device" device) (log: $build_log)"
code=0
"${build[@]}" > "$build_log" 2>&1 || code=$?
if [ "$code" -ne 0 ]; then
    if grep -q "needs to be unlocked" "$build_log"; then
        echo "dev-install: the iPhone is locked; unlock it (and keep it unlocked) and rerun" >&2
        exit 3
    fi
    scrub "$build_log" | grep -E "error" | sort -u | head -20 >&2 || true
    echo "dev-install: build failed (exit $code); see $build_log" >&2
    exit "$code"
fi
echo "dev-install: installing $bundle"
"${install[@]}"
if [ "$launch" -eq 1 ]; then
    echo "dev-install: launching"
    "${run[@]}" || echo "dev-install: installed, but launch failed (unlock the phone and trust the developer in Settings > General > VPN & Device Management)" >&2
fi
echo "dev-install: ok"
