#!/usr/bin/env bash
# suite-gate: tap
# Regression checks for the authorized systemd test fallback (#1717).
# Port of the Python suite of the same name (#366).
#
# Case map (legacy test_systemd_test_fallback.py -> here):
#   test_success_records_zero_and_both_output_streams -> success records zero and both output streams
#   test_command_failure_survives_successful_tee      -> a command failure survives a successful tee
#   test_log_failure_cannot_record_green              -> a log failure cannot record green
#   test_real_unit_preserves_bash_variables_and_failure -> a real unit preserves bash variables and
#       failure (Linux with a systemd user manager only: SKIP elsewhere, hub #3621 Linux checklist)
# The exit record and the log are compared byte for byte (assert_file_is): `$(cat file)` strips
# trailing newlines and would hide a missing final newline (#474). The real-unit case stops
# its transient unit from an EXIT trap AFTER the completion record is observed, as the legacy
# test did in cleanup (stopping the asynchronous unit first raced the record, #473).
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/systemd-test-fallback"

# execute <command...>: the fallback's --execute mode against $LOG / $EXIT_FILE.
execute() {
    run_capture bash "$SCRIPT" --execute "$LOG" "$EXIT_FILE" -- "$@"
}

setup_paths() {
    LOG="$CASE_DIR/test.log"
    EXIT_FILE="$CASE_DIR/test.exit"
}

case_success_records_zero_and_both_output_streams() {
    setup_paths
    execute bash -c 'echo output; echo error >&2'
    assert_code 0 "exit code"
    assert_file_is "$EXIT_FILE" $'0\n' "exit record"
    # Both streams, in order, nothing else.
    assert_file_is "$LOG" $'output\nerror\n' "log"
}

case_command_failure_survives_successful_tee() {
    setup_paths
    execute bash -c 'echo failed; exit 7'
    assert_code 7 "exit code"
    assert_file_is "$EXIT_FILE" $'7\n' "exit record"
    assert_file_is "$LOG" $'failed\n' "log"
}

case_log_failure_cannot_record_green() {
    setup_paths
    LOG="$CASE_DIR/missing/test.log"
    execute true
    assert_code 1 "exit code"
    assert_file_is "$EXIT_FILE" $'1\n' "exit record"
}

case_real_unit_preserves_bash_variables_and_failure() {
    setup_paths
    local unit="rsi-test-fallback-$(date +%s)$$" command waited=0
    # The unit is asynchronous (systemd-run without --wait): stop it only on the way out.
    trap 'systemctl --user stop "$unit" >/dev/null 2>&1 || true' EXIT
    # Neither systemd nor the outer shell may expand this Bash expression.
    # shellcheck disable=SC2016
    command='value=preserved; printf "%s\n" "$value" "$CARGO_TARGET_DIR"; (exit 7) | cat; result=${PIPESTATUS[0]}; exit "$result"'
    CARGO_TARGET_DIR="$CASE_DIR/target" run_capture bash "$SCRIPT" "$unit" "$LOG" "$EXIT_FILE" -- bash -c "$command"
    assert_code 0 "launch exit code"
    while [[ ! -e "$EXIT_FILE" && $waited -lt 200 ]]; do
        sleep 0.05
        waited=$((waited + 1))
    done
    assert_file "$EXIT_FILE" "the unit never recorded its completion within 10 s"
    assert_file_is "$EXIT_FILE" $'7\n' "exit record"
    assert_file_is "$LOG" $'preserved\n'"$CASE_DIR/target"$'\n' "log"
}

run_case "success records zero and both output streams" case_success_records_zero_and_both_output_streams
run_case "a command failure survives a successful tee" case_command_failure_survives_successful_tee
run_case "a log failure cannot record green" case_log_failure_cannot_record_green
if [[ "$(uname -s)" == Linux ]] && command -v systemd-run >/dev/null 2>&1 \
    && systemctl --user show-environment >/dev/null 2>&1; then
    run_case "a real unit preserves bash variables and failure" case_real_unit_preserves_bash_variables_and_failure
else
    skip_linux_only "a real unit preserves bash variables and failure" "Linux with systemd-run and a running systemd user manager"
fi
tap_done
