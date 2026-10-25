#!/usr/bin/env bash
# Run the RSI Remote iOS unit tests on an iOS simulator (hub #2963).
#
# usage: scripts/ios/test.sh [--os 26.5] [--device "iPhone 17 Pro"] [--timeout SECS]
#
# Generates the Xcode project with XcodeGen, then runs `xcodebuild test`
# unsigned on the simulator. Prints one receipt line at the end:
#   ios-test: ok|failed tests=<executed> failures=<n> os=<os> device=<name> log=<path>
# xcodebuild runs under a watchdog (default 300 s; --timeout SECS or
# RSI_IOS_TEST_TIMEOUT). A hung run is killed (its whole process group, plus
# any simulator this run booted), the log is parsed anyway, the receipt gets a
# trailing `reason=timeout` and the exit code is 124 (hub #2963, #11).
# Each run creates its own simulator (same device type and runtime as
# --os/--device, named rsi-ios-test-<pid>) and deletes it on exit, also after a
# timeout or an error, so worktrees never share (and wedge) one simulator
# (mac-mini #22). If a run still reports "Application failed preflight checks"
# it retries once on a fresh simulator and the receipt ends `reason=sim-preflight`
# (`reason=timeout` wins when both happen). If a simulator cannot be created the
# run falls back to the shared one named by --os/--device, with a warning.
# Each clone is booted once up front with the simulator's own Family service
# (com.apple.familynotification, which starts Family.app) disabled in its own
# launchd, because Family.app aborts at launch on the 26.5 runtime and makes
# macOS show a "Family quit unexpectedly" dialog (#509). Only the clone changes:
# the host's com.apple.CrashReporter settings and all other crash reports stay.
# Runs on macOS /bin/bash 3.2 (no GNU timeout).
set -euo pipefail

os="26.5"
device="iPhone 17 Pro"
timeout_secs="${RSI_IOS_TEST_TIMEOUT:-300}"
while [ $# -gt 0 ]; do
    case "$1" in
        --os) os="$2"; shift ;;
        --device) device="$2"; shift ;;
        --timeout) timeout_secs="${2:-}"; shift ;;
        -h | --help) sed -n '2,26p' "$0"; exit 0 ;;
        *) echo "ios-test: unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done

case "$timeout_secs" in
    '' | *[!0-9]* | 0) echo "ios-test: timeout must be a positive integer of seconds, got '$timeout_secs'" >&2; exit 2 ;;
esac

top=$(cd "$(dirname "$0")/../.." && pwd)
project_dir="$top/remote/ios"
log=$(mktemp "${TMPDIR:-/tmp}/rsi-ios-test.XXXXXX")
flag="$log.timeout"
uuid_re='[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}'
# shellcheck source=scripts/ios/sim-quiet-family.sh
. "$top/scripts/ios/sim-quiet-family.sh"

booted_sims() {
    command -v xcrun > /dev/null 2>&1 || return 0
    xcrun simctl list devices booted 2> /dev/null | grep -Eo "$uuid_re" | sort -u || true
}

# The simulator this run created; only ever this one is shut down and deleted.
clone=""
clone_name="rsi-ios-test-$$"

drop_clone() {
    [ -n "$clone" ] || return 0
    xcrun simctl shutdown "$clone" > /dev/null 2>&1 || true
    xcrun simctl delete "$clone" > /dev/null 2>&1 || true
    clone=""
}

cleanup() {
    drop_clone
    rm -f "$flag"
}
trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT

# Create a fresh simulator of the --device type on the --os runtime; sets $clone.
make_clone() {
    clone=""
    command -v xcrun > /dev/null 2>&1 || return 1
    local type_id runtime_id made
    type_id=$(xcrun simctl list devicetypes 2> /dev/null |
        grep -F "$device (com.apple.CoreSimulator.SimDeviceType." | head -1 |
        sed -E 's/.*\((com\.apple\.CoreSimulator\.SimDeviceType\.[^)]*)\).*/\1/') || true
    runtime_id=$(xcrun simctl list runtimes available 2> /dev/null |
        grep -F "iOS $os (" | grep -v unavailable | head -1 |
        sed -E 's/.* - (com\.apple\.CoreSimulator\.SimRuntime\.[^ ]*).*/\1/') || true
    [ -n "$type_id" ] && [ -n "$runtime_id" ] || return 1
    made=$(xcrun simctl create "$clone_name" "$type_id" "$runtime_id" 2> /dev/null |
        grep -Eo "$uuid_re" | head -1) || true
    [ -n "$made" ] || return 1
    clone="$made"
    # Stop the simulator's own Family service from crashing and raising the macOS
    # "Family quit unexpectedly" dialog on every clone boot (#509).
    sim_quiet_family "$clone"
}

# Run xcodebuild under the watchdog; sets $code and $timed_out.
run_xcodebuild() {
    local dest xb_pid wd_pid
    if [ -n "$clone" ]; then
        dest="id=$clone"
    else
        dest="platform=iOS Simulator,OS=$os,name=$device"
    fi
    code=0
    timed_out=0
    rm -f "$flag"
    # Job control gives the background xcodebuild its own process group, so the
    # watchdog can kill it and every child it spawned.
    set -m
    xcodebuild test -project "$project_dir/RSIRemote.xcodeproj" -scheme RSIRemote \
        -destination "$dest" \
        -derivedDataPath "$project_dir/build/DerivedData" CODE_SIGNING_ALLOWED=NO \
        > "$log" 2>&1 &
    xb_pid=$!
    (
        waited=0
        while [ "$waited" -lt "$timeout_secs" ]; do
            kill -0 "$xb_pid" 2> /dev/null || exit 0
            sleep 1
            waited=$((waited + 1))
        done
        kill -0 "$xb_pid" 2> /dev/null || exit 0
        : > "$flag"
        kill -s TERM -- "-$xb_pid" 2> /dev/null || kill -s TERM "$xb_pid" 2> /dev/null || true
        sleep 3
        kill -s KILL -- "-$xb_pid" 2> /dev/null || kill -s KILL "$xb_pid" 2> /dev/null || true
    ) > /dev/null 2>&1 &
    wd_pid=$!
    wait "$xb_pid" 2> /dev/null || code=$?
    kill "$wd_pid" 2> /dev/null || true
    wait "$wd_pid" 2> /dev/null || true
    set +m
    if [ -e "$flag" ]; then
        rm -f "$flag"
        timed_out=1
        code=124
    fi
}

xcodegen generate --quiet --spec "$project_dir/project.yml"
sims_before=$(booted_sims)
if ! make_clone; then
    echo "ios-test: could not create a simulator for $device iOS $os; using the shared one" >&2
fi
code=0
timed_out=0
preflight=0
run_xcodebuild
if [ "$code" -ne 0 ] && [ "$timed_out" -eq 0 ] && [ -n "$clone" ] &&
    grep -Eq "failed preflight checks|SBMainWorkspace.*Busy" "$log"; then
    echo "ios-test: simulator preflight Busy; retrying once on a fresh simulator" >&2
    preflight=1
    mv -f "$log" "$log.preflight"
    drop_clone
    make_clone || true
    run_xcodebuild
fi
if [ "$timed_out" -eq 1 ]; then
    echo "ios-test: xcodebuild exceeded ${timeout_secs}s; killed" >&2
    # Shut down only simulators this run booted; others may belong to someone else.
    for sim in $(booted_sims); do
        case "$sims_before" in
            *"$sim"*) ;;
            *) xcrun simctl shutdown "$sim" > /dev/null 2>&1 || true ;;
        esac
    done
fi
drop_clone

grep -E "error:|failed|Test Case .* failed" "$log" | sort -u | head -40 >&2 || true
summary=$(grep -E "Executed [0-9]+ tests?, with [0-9]+ failures?" "$log" | tail -1 || true)
executed=$(printf '%s' "$summary" | sed -E 's/.*Executed ([0-9]+) tests?.*/\1/')
failures=$(printf '%s' "$summary" | sed -E 's/.*with ([0-9]+) failures?.*/\1/')
state=ok
[ "$code" -eq 0 ] || state=failed
reason=""
[ "$preflight" -eq 0 ] || reason=" reason=sim-preflight"
[ "$timed_out" -eq 0 ] || reason=" reason=timeout"
echo "ios-test: $state tests=${executed:-0} failures=${failures:-?} os=$os device=$device log=$log$reason"
exit "$code"
