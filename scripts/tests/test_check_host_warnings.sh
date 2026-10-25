#!/usr/bin/env bash
# suite-gate: tap
# suite-gate: scans scripts/check-host-warnings.sh
# Host warning self-test (#348), selected when the host check changes.
source "$(dirname "$0")/lib/tap.sh"

case_the_host_warning_self_test_passes() {
    run_capture "$SCRIPTS_DIR/check-host-warnings.selftest.sh"
    assert_code 0 "self-test exit code"
    assert_contains "$RUN_OUT" "all passed" "self-test result"
}

run_case "the host warning self-test passes" case_the_host_warning_self_test_passes
tap_done
