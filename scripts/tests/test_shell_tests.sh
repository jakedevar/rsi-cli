#!/usr/bin/env bash
# suite-gate: tap
# suite-gate: scans scripts/tests
# Runs every exit-status shell test under `scripts/tests/*.sh` inside the residual
# lane (#297), so a change to one of them or to the script it covers runs it.
# Port of the Python wrapper of the same name (#366).
#
# The residual lane selects a suite whose text names a changed file's basename, so
# listing each shell test and its subject scripts below is what makes a change to
# either select this wrapper. A shell test that declares `# suite-gate: tap` is
# itself a suite and is not listed here. Adding any other `scripts/tests/*.sh`
# without listing it in `shell_test_subjects` fails the first case.
#
# Case map (legacy test_shell_tests.py -> here):
#   test_every_shell_test_is_listed                  -> every shell test is listed
#   test_a_skip_names_a_filed_issue, test_skipped_shell_tests_are_reported_as_skipped
#                                                    -> removed with the (empty) SKIPPED registry; a shell test
#                                                       that cannot run is a RED case naming #465, never a skip
#   test_every_covered_subject_script_exists         -> every covered subject script exists
#   test_the_gate_selects_this_wrapper_for_a_shell_test_or_its_subject
#   test_the_gate_runs_this_wrapper_under_the_stdlib_runner
#                                                    -> the REAL selector and dispatch are exercised by native
#                                                       tests, not by substrings here: rsi-repo-guards
#                                                       `ported_suites_tests::the_residual_selector_picks_this_wrapper_for_every_listed_shell_test_and_subject`
#                                                       builds a scratch repository from this checkout's listed
#                                                       files and asks `residual::selected` / `residual::lane`
#                                                       for each listed test and subject. The two cases below
#                                                       are the cheap in-suite tripwires for the same contract.
#   test_each_shell_test_exits_zero (x5)             -> one "<name> exits zero" case per listed shell test
#   (new, #465)                                      -> "the Python-subject refusal still bites": negative proof of runs_python on
#                                                       a python3 heredoc, an inline python3 -c, a bare python call and a clean
#                                                       script, plus every real shell test and subject is Python-free
#
# Covered shell tests and their subjects:
#   check-corpus-pins.test.sh   check-corpus-pins.sh
#   test_install_release_start.sh  install-release.sh rsid-start-satellite rsid-health.sh rsid-launchd.sh rsid-processes.sh repo-guard-bin.sh
#   test_rsid_health_wait.sh    rsid-health.sh repo-guard-bin.sh
#   test_rsid_processes.sh      rsid-processes.sh
#   test_rsid_quiet_restart.sh  rsid-quiet-restart.sh rsid-supervisor.sh repo-guard-bin.sh
source "$(dirname "$0")/lib/tap.sh"

TESTS_DIR="$SCRIPTS_DIR/tests"

SHELL_TESTS="check-corpus-pins.test.sh test_install_release_start.sh test_rsid_health_wait.sh test_rsid_processes.sh test_rsid_quiet_restart.sh"

shell_test_subjects() { # <shell test>
    case "$1" in
        check-corpus-pins.test.sh) echo "check-corpus-pins.sh" ;;
        test_install_release_start.sh) echo "install-release.sh rsid-start-satellite rsid-health.sh rsid-launchd.sh rsid-processes.sh repo-guard-bin.sh" ;;
        test_rsid_health_wait.sh) echo "rsid-health.sh repo-guard-bin.sh" ;;
        test_rsid_processes.sh) echo "rsid-processes.sh" ;;
        test_rsid_quiet_restart.sh) echo "rsid-quiet-restart.sh rsid-supervisor.sh repo-guard-bin.sh" ;;
        *) return 1 ;;
    esac
}

# Whether a file runs an interpreter this repository forbids (#358): such a shell
# test cannot run at all until its subject is ported, and says so (red), never skips.
runs_python() { # <file>
    grep -Eq '(^|[^[:alnum:]_./-])python3?([^[:alnum:]_-]|$)' "$1"
}

case_every_shell_test_is_listed() {
    local on_disk="" path name unlisted="" stale=""
    for path in "$TESTS_DIR"/*.sh; do
        name="$(basename "$path")"
        grep -qx '# suite-gate: tap' "$path" && continue
        on_disk="$on_disk $name"
        case " $SHELL_TESTS " in *" $name "*) ;; *) unlisted="$unlisted $name" ;; esac
    done
    for name in $SHELL_TESTS; do
        case " $on_disk " in *" $name "*) ;; *) stale="$stale $name" ;; esac
    done
    assert_eq "" "$unlisted" "scripts/tests/*.sh run by no lane: list it in shell_test_subjects"
    assert_eq "" "$stale" "a listed shell test no longer exists"
}

case_every_covered_subject_script_exists() {
    local name subject subjects
    for name in $SHELL_TESTS; do
        subjects="$(shell_test_subjects "$name")" || tap_fail "$name has no subject entry"
        [[ -n "$subjects" ]] || tap_fail "$name names no subject script"
        for subject in $subjects; do
            assert_file "$SCRIPTS_DIR/$subject" "$name: scripts/$subject"
        done
    done
}

case_this_wrapper_names_every_shell_test_and_subject_so_the_lane_selects_it() {
    local self name subject
    self="$(cat "$SUITE_FILE")"
    for name in $SHELL_TESTS; do
        assert_contains "$self" "$name" "the wrapper must name the shell test"
        for subject in $(shell_test_subjects "$name"); do
            assert_contains "$self" "$subject" "the wrapper must name the subject of $name"
        done
    done
}

case_the_wrapper_declares_the_scripts_tests_scan() {
    grep -qx '# suite-gate: scans scripts/tests' "$SUITE_FILE" || tap_fail "the directory scan declaration is missing"
}

# Negative proof (#465): the refusal that keeps a Python-backed subject red is intact.
# The subjects are Python-free now, so nothing else exercises it; if `runs_python` were
# weakened, a regression that put an interpreter back would run (or hang) instead of
# failing here.
case_the_python_subject_refusal_still_bites() {
    write_file heredoc.sh $'#!/usr/bin/env bash\npython3 - "$1" <<\'PY\'\nprint(1)\nPY\n'
    write_file inline.sh $'#!/usr/bin/env bash\nroot="$(cargo metadata | python3 -c \'import json\')"\n'
    write_file bare.sh $'#!/usr/bin/env bash\npython -c \'print(1)\'\n'
    write_file clean.sh $'#!/usr/bin/env bash\necho "a pythonic habit, mypython3x, ./python3-free"\n'
    runs_python heredoc.sh || tap_fail "a python3 heredoc is not recognised as a Python-backed subject"
    runs_python inline.sh || tap_fail "an inline python3 -c is not recognised as a Python-backed subject"
    runs_python bare.sh || tap_fail "a bare python call is not recognised as a Python-backed subject"
    runs_python clean.sh && tap_fail "a script that only mentions similar words was taken for Python"
    # Every real subject and shell test is Python-free today (this is what #465 delivered).
    local name subject
    for name in $SHELL_TESTS; do
        runs_python "$TESTS_DIR/$name" && tap_fail "$name runs Python"
        for subject in $(shell_test_subjects "$name"); do
            runs_python "$SCRIPTS_DIR/$subject" && tap_fail "scripts/$subject runs Python"
        done
    done
    return 0
}

# One case per shell test: it must exit zero (the case output shows why when it does not).
case_current_shell_test() { # the shell test named by SHELL_TEST_NAME
    local name="$SHELL_TEST_NAME" path subject
    path="$TESTS_DIR/$name"
    if runs_python "$path"; then
        tap_fail "$name runs Python, which is forbidden (#358); its subject must be ported before this test can run"
    fi
    for subject in $(shell_test_subjects "$name"); do
        if runs_python "$SCRIPTS_DIR/$subject"; then
            tap_fail "$name exercises scripts/$subject, which still runs Python (#358); port it before this test can run"
        fi
    done
    cd "$REPO_ROOT" || exit 1
    run_capture bash "$path"
    assert_code 0 "$name exit code"
}

run_case "every shell test is listed" case_every_shell_test_is_listed
run_case "every covered subject script exists" case_every_covered_subject_script_exists
run_case "this wrapper names every shell test and subject so the lane selects it" case_this_wrapper_names_every_shell_test_and_subject_so_the_lane_selects_it
run_case "the wrapper declares the scripts/tests scan" case_the_wrapper_declares_the_scripts_tests_scan
run_case "the Python-subject refusal still bites (negative proof, #465)" case_the_python_subject_refusal_still_bites
for shell_test in $SHELL_TESTS; do
    SHELL_TEST_NAME="$shell_test"
    run_case "$shell_test exits zero" case_current_shell_test
done
tap_done
