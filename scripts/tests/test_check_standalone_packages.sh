#!/usr/bin/env bash
# suite-gate: tap
# The full-library QA recipe checks every workspace package standalone (#2444).
# Port of the Python suite of the same name (#366).
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/check-standalone-packages.sh"

# A fake cargo: `metadata` prints the JSON in $CASE_DIR/metadata.json, every other
# call is logged and exits 101 for the packages named in $FAILING.
fake_cargo() { # <metadata json> <failing packages, space separated>
    printf '%s' "$1" >"$CASE_DIR/metadata.json"
    cat >"$CASE_DIR/cargo" <<SH
#!/bin/sh
if [ "\$1" = metadata ]; then cat '$CASE_DIR/metadata.json'; exit 0; fi
echo "\$*" >> '$CASE_DIR/log'
for failing in $2; do [ "\$3" = "\$failing" ] && exit 101; done
exit 0
SH
    chmod +x "$CASE_DIR/cargo"
}

packages_json() { # <names...>: realistic cargo-metadata package objects
    local out="" name
    for name in "$@"; do
        out="$out${out:+,}{\"name\":\"$name\",\"version\":\"0.1.0\",\"id\":\"$name 0.1.0\",\"dependencies\":[{\"name\":\"serde\",\"source\":\"registry\",\"req\":\"1\"}],\"targets\":[{\"kind\":[\"lib\"],\"crate_types\":[\"lib\"],\"name\":\"$name\",\"src_path\":\"/x\"}]}"
    done
    printf '{"packages":[%s],"workspace_members":[]}' "$out"
}

run_script() { # (cargo is the case's fake)
    CHECK_STANDALONE_CARGO="$CASE_DIR/cargo" run_capture "$SCRIPT"
    CALLS=""
    [[ -f "$CASE_DIR/log" ]] && CALLS="$(cat "$CASE_DIR/log")"
    return 0
}

case_every_package_is_checked_alone_with_all_targets() {
    fake_cargo "$(packages_json beta alpha)" ""
    run_script
    assert_code 0 "exit code"
    assert_eq $'check -p alpha --all-targets\ncheck -p beta --all-targets' "$CALLS" "cargo calls"
}

case_a_failure_is_named_and_does_not_stop_the_sweep() {
    fake_cargo "$(packages_json alpha beta gamma)" "alpha"
    run_script
    assert_code 1 "exit code"
    assert_contains "$RUN_ERR" "standalone_check_failed:alpha"
    assert_eq 3 "$(printf '%s\n' "$CALLS" | wc -l | tr -d ' ')" "number of cargo calls"
}

case_an_empty_workspace_fails_closed() {
    fake_cargo '{"packages":[],"workspace_members":[]}' ""
    run_script
    assert_code 2 "exit code"
}

case_unparseable_metadata_fails_closed() {
    fake_cargo 'not json at all' ""
    run_script
    assert_code 2 "exit code"
    assert_contains "$RUN_ERR" "cannot parse the workspace metadata"
}

case_a_failing_metadata_call_fails_closed() {
    printf '#!/bin/sh\nexit 1\n' >"$CASE_DIR/cargo"
    chmod +x "$CASE_DIR/cargo"
    run_script
    assert_code 2 "exit code"
}

case_the_library_qa_recipe_runs_the_standalone_step() {
    local recipe
    recipe="$(awk '/^rolling-library-qa:/{on=1;next} on&&/^$/{exit} on{print}' "$REPO_ROOT/Makefile")"
    assert_contains "$recipe" "scripts/check-standalone-packages.sh" "recipe runs the step"
    assert_contains "$recipe" 'exit $$status' "recipe exits with the status"
    local before="${recipe%%nextest run*}" after="${recipe%%check-standalone-packages*}"
    # nextest run comes before the standalone step.
    [[ ${#before} -lt ${#after} ]] || tap_fail "nextest run must precede check-standalone-packages"
}

case_the_library_qa_plan_executes_the_small_standalone_lib_tests() {
    # #4002: rsi-common held the #4001 red that a cargo check passed.
    command -v make >/dev/null || { tap_fail "make is not installed (runner_unavailable)"; return 0; }
    local plan
    plan="$(cd "$REPO_ROOT" && env -u RSI_SCOPED_TEST_FILTERS make -n rolling-library-qa 2>&1)"
    local pkg
    for pkg in rsi-common rsid-core rsi-cloud; do
        assert_contains "$plan" "-p $pkg " "QA plan runs $pkg tests"
    done
    assert_contains "$plan" "cargo nextest run --profile rsid-qa" "plan runs nextest"
    assert_contains "$plan" "--lib --bins --tests" "plan includes the lib targets"
}

run_case "every package is checked alone with all targets" case_every_package_is_checked_alone_with_all_targets
run_case "a failure is named and does not stop the sweep" case_a_failure_is_named_and_does_not_stop_the_sweep
run_case "an empty workspace fails closed" case_an_empty_workspace_fails_closed
run_case "unparseable metadata fails closed" case_unparseable_metadata_fails_closed
run_case "a failing metadata call fails closed" case_a_failing_metadata_call_fails_closed
run_case "the library QA recipe runs the standalone step" case_the_library_qa_recipe_runs_the_standalone_step
run_case "the library QA plan executes the small standalone lib tests" case_the_library_qa_plan_executes_the_small_standalone_lib_tests
tap_done
