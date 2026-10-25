#!/usr/bin/env bash
# suite-gate: tap
# scripts/ios/test.sh: #11 watchdog for a hung xcodebuild, mac-mini #22 per-run simulator.
# Port of the Python suite of the same name (#366).
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/ios/test.sh"

write_stub() { # <bin dir> <name> (body on stdin)
    cat >"$1/$2"
    chmod +x "$1/$2"
}

OK_XCODEBUILD='#!/bin/bash
echo "Test Suite '"'"'All tests'"'"' passed"
printf "\t Executed 12 tests, with 0 failures (0 unexpected) in 0.5 (0.6) seconds\n"
'

# Prints the summary, then hangs like the observed simulator stall. The marker file
# lets the test confirm the process (and its sleep child) are gone.
HUNG_XCODEBUILD='#!/bin/bash
printf "\t Executed 39 tests, with 0 failures (0 unexpected) in 0.5 (0.6) seconds\n"
echo $$ > "$STUB_PID_FILE"
sleep 1000
'

setup_timeout_case() { # <xcodebuild stub text>
    BIN="$CASE_DIR"
    PID_FILE="$BIN/xcodebuild.pid"
    printf '#!/bin/bash\nexit 0\n' | write_stub "$BIN" xcodegen
    printf '#!/bin/bash\nexit 0\n' | write_stub "$BIN" xcrun
    printf '%s' "$1" | write_stub "$BIN" xcodebuild
}

run_timeout_script() { # [args...] (EXTRA_ENV: NAME=value ...)
    ELAPSED_START=$SECONDS
    # shellcheck disable=SC2086
    run_capture env PATH="$BIN:/usr/bin:/bin" STUB_PID_FILE="$PID_FILE" ${EXTRA_ENV:-} /bin/bash "$SCRIPT" "$@"
    ELAPSED=$((SECONDS - ELAPSED_START))
}

pid_is_gone() { # <pid>
    ! kill -0 "$1" 2>/dev/null
}

case_success_prints_an_ok_receipt() {
    setup_timeout_case "$OK_XCODEBUILD"
    run_timeout_script
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_contains "$RUN_OUT" "ios-test: ok tests=12 failures=0 "
    assert_not_contains "$RUN_OUT" "reason=timeout"
}

case_a_hung_xcodebuild_times_out_with_a_receipt() {
    setup_timeout_case "$HUNG_XCODEBUILD"
    run_timeout_script --timeout 2
    assert_code 124 "exit code (stderr: $RUN_ERR)"
    assert_contains "$RUN_OUT" "ios-test: failed tests=39 failures=0 "
    case "$RUN_OUT" in *reason=timeout) ;; *) tap_fail "the receipt does not end with reason=timeout: $RUN_OUT" ;; esac
    [[ "$ELAPSED" -lt 20 ]] || tap_fail "the watchdog took ${ELAPSED}s"
    pid_is_gone "$(cat "$PID_FILE")" || tap_fail "the hung xcodebuild is still running"
}

case_the_env_timeout_applies() {
    setup_timeout_case "$HUNG_XCODEBUILD"
    EXTRA_ENV="RSI_IOS_TEST_TIMEOUT=2" run_timeout_script
    assert_code 124 "exit code (stderr: $RUN_ERR)"
    assert_contains "$RUN_OUT" "reason=timeout"
}

case_a_bad_timeout_is_rejected() {
    setup_timeout_case "$OK_XCODEBUILD"
    run_timeout_script --timeout abc
    assert_code 2 "exit code"
}

# --- mac-mini #22: every run owns a simulator clone and always deletes it ---

# Stateful xcrun stub: answers the simctl queries test.sh makes, hands out a new udid
# per `create` (none when STUB_NO_CREATE is set) and logs every call.
CLONE_XCRUN='#!/bin/bash
echo "$*" >> "$STUB_XCRUN_LOG"
[ "$1" = simctl ] || exit 0
case "$2" in
    list)
        case "$3" in
            devicetypes) echo "iPhone 17 Pro (com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro)"
                echo "iPhone 17 Pro Max (com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro-Max)" ;;
            runtimes) echo "iOS 26.5 (26.5 - 23F77) - com.apple.CoreSimulator.SimRuntime.iOS-26-5" ;;
            devices) echo "    iPhone 17 Pro (995FC5B4-40A7-4D35-BD8A-27832523127D) (Booted)" ;;
        esac ;;
    create)
        [ -z "$STUB_NO_CREATE" ] || exit 1
        n=$(cat "$STUB_COUNTER" 2>/dev/null || echo 0)
        n=$((n + 1))
        echo "$n" > "$STUB_COUNTER"
        printf "AAAAAAAA-0000-0000-0000-%012d\n" "$n" ;;
esac
exit 0
'

# Logs its -destination, fails once with the preflight Busy error when asked.
RECORDING_XCODEBUILD='#!/bin/bash
for a in "$@"; do echo "$a" >> "$STUB_DEST_LOG"; done
n=$(cat "$STUB_XB_COUNTER" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$STUB_XB_COUNTER"
if [ "$STUB_XB_MODE" = preflight_once ] && [ "$n" = 1 ]; then
    echo "Application failed preflight checks: SBMainWorkspace Busy"
    exit 65
fi
if [ "$STUB_XB_MODE" = fail ]; then
    echo "error: build broke"
    exit 65
fi
if [ "$STUB_XB_MODE" = hang ]; then
    sleep 1000
fi
printf "\t Executed 7 tests, with 0 failures (0 unexpected) in 0.5 (0.6) seconds\n"
'

setup_clone_case() {
    DIR="$CASE_DIR"
    BIN="$DIR/bin"
    mkdir "$BIN"
    XCRUN_LOG="$DIR/xcrun.log"
    DEST_LOG="$DIR/dest.log"
    : >"$XCRUN_LOG"
    : >"$DEST_LOG"
    printf '#!/bin/bash\nexit 0\n' | write_stub "$BIN" xcodegen
    printf '%s' "$CLONE_XCRUN" | write_stub "$BIN" xcrun
    printf '%s' "$RECORDING_XCODEBUILD" | write_stub "$BIN" xcodebuild
}

# run_clone_script <mode> [no-create] [script args...]
run_clone_script() {
    local mode="$1" no_create=""
    shift
    if [[ "${1:-}" == no-create ]]; then
        no_create="1"
        shift
    fi
    run_capture env PATH="$BIN:/usr/bin:/bin" TMPDIR="$DIR" STUB_XCRUN_LOG="$XCRUN_LOG" \
        STUB_DEST_LOG="$DEST_LOG" STUB_COUNTER="$DIR/create.count" STUB_XB_COUNTER="$DIR/xb.count" \
        STUB_XB_MODE="$mode" STUB_NO_CREATE="$no_create" /bin/bash "$SCRIPT" "$@"
}

xcrun_calls() { # <verb>: the logged `simctl <verb> ...` lines
    grep "^simctl $1 " "$XCRUN_LOG" || true
}

destinations() {
    grep -E '^(id=|platform=)' "$DEST_LOG" || true
}

assert_all_clones_deleted() {
    local created deleted n
    created="$(xcrun_calls create | grep -c . || true)"
    deleted="$(xcrun_calls delete | awk '{print $3}')"
    assert_eq "$created" "$(printf '%s\n' "$deleted" | grep -c . || true)" "deleted clones vs created ($(cat "$XCRUN_LOG"))"
    n=1
    while [[ $n -le $created ]]; do
        assert_contains "$deleted" "$(printf 'AAAAAAAA-0000-0000-0000-%012d' "$n")" "clone $n deleted"
        n=$((n + 1))
    done
    # never the pre-existing booted simulator
    assert_not_contains "$deleted" "995FC5B4-40A7-4D35-BD8A-27832523127D"
}

case_a_run_uses_its_own_clone_and_deletes_it() {
    setup_clone_case
    run_clone_script ok
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_contains "$RUN_OUT" "ios-test: ok tests=7 failures=0 os=26.5 device=iPhone 17 Pro "
    local create
    create="$(xcrun_calls create | sed -n 1p)"
    set -- $create
    case "$3" in rsi-ios-test-*) ;; *) tap_fail "the clone is not named rsi-ios-test-*: $create" ;; esac
    assert_eq "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro" "$4" "device type"
    assert_eq "com.apple.CoreSimulator.SimRuntime.iOS-26-5" "$5" "runtime"
    assert_eq "id=AAAAAAAA-0000-0000-0000-000000000001" "$(destinations)" "destinations"
    assert_all_clones_deleted
}

case_the_clone_is_deleted_after_a_build_failure() {
    setup_clone_case
    run_clone_script fail
    assert_code 65 "exit code (stderr: $RUN_ERR)"
    assert_contains "$RUN_OUT" "ios-test: failed "
    assert_not_contains "$RUN_OUT" "reason="
    assert_eq 1 "$(xcrun_calls create | grep -c .)" "created clones"
    assert_all_clones_deleted
}

case_the_clone_is_deleted_after_a_timeout() {
    setup_clone_case
    run_clone_script hang --timeout 2
    assert_code 124 "exit code (stderr: $RUN_ERR)"
    case "$RUN_OUT" in *reason=timeout) ;; *) tap_fail "the receipt does not end with reason=timeout: $RUN_OUT" ;; esac
    assert_all_clones_deleted
}

case_a_preflight_busy_retries_once_on_a_fresh_clone() {
    setup_clone_case
    run_clone_script preflight_once
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_contains "$RUN_OUT" "ios-test: ok tests=7 "
    case "$RUN_OUT" in *reason=sim-preflight) ;; *) tap_fail "the receipt does not end with reason=sim-preflight: $RUN_OUT" ;; esac
    assert_eq $'id=AAAAAAAA-0000-0000-0000-000000000001\nid=AAAAAAAA-0000-0000-0000-000000000002' "$(destinations)" "destinations"
    assert_all_clones_deleted
}

case_it_falls_back_to_the_shared_simulator_when_create_fails() {
    setup_clone_case
    run_clone_script ok no-create
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_contains "$RUN_ERR" "using the shared one"
    assert_eq "platform=iOS Simulator,OS=26.5,name=iPhone 17 Pro" "$(destinations)" "destinations"
    assert_eq "" "$(xcrun_calls delete)" "deleted simulators"
}

if [[ -x /bin/bash ]]; then
    run_case "success prints an ok receipt" case_success_prints_an_ok_receipt
    run_case "a hung xcodebuild times out with a receipt" case_a_hung_xcodebuild_times_out_with_a_receipt
    run_case "the env timeout applies" case_the_env_timeout_applies
    run_case "a bad timeout is rejected" case_a_bad_timeout_is_rejected
    run_case "a run uses its own clone and deletes it" case_a_run_uses_its_own_clone_and_deletes_it
    run_case "the clone is deleted after a build failure" case_the_clone_is_deleted_after_a_build_failure
    run_case "the clone is deleted after a timeout" case_the_clone_is_deleted_after_a_timeout
    run_case "a preflight busy retries once on a fresh clone" case_a_preflight_busy_retries_once_on_a_fresh_clone
    run_case "it falls back to the shared simulator when create fails" case_it_falls_back_to_the_shared_simulator_when_create_fails
else
    skip_case "scripts/ios/test.sh" "needs /bin/bash"
fi
tap_done
