#!/usr/bin/env bash
# suite-gate: tap
# The desktop-e2e-graph job recipe is declared, wired to make, and refuses
# arguments (#3279). Port of the Python suite of the same name (#366).
#
# Case map (legacy test_desktop_e2e_graph_job.py -> here):
#   test_recipe_is_declared_and_maps_to_a_make_target        -> the recipe is declared and maps to a make target
#   test_script_runs_the_seeded_build_and_the_receipt_judge  -> the script runs the seeded build and the receipt judge
#   test_raw_report_is_echoed_into_the_durable_log_before_the_judge -> the raw report is echoed ... before the judge
#   test_arguments_are_refused                               -> arguments are refused
# The legacy test parsed the TOML (tomllib); toml_get reads only ACTIVE keys of the named table
# (a commented key does not count). Negative coverage: ported_suites_tests.rs mutates the
# subject (comments out the recipe keys, deletes the judge invocation, reorders the echo) in a
# scratch copy and requires this suite red.
source "$(dirname "$0")/lib/tap.sh"

WRAPPER="$SCRIPTS_DIR/desktop-e2e-graph-job.sh"

case_the_recipe_is_declared_and_maps_to_a_make_target() {
    assert_eq make "$(toml_get "$REPO_ROOT/.rsi/jobs.toml" recipes.desktop-e2e-graph runner)" "recipe runner"
    assert_eq desktop-e2e-graph-job "$(toml_get "$REPO_ROOT/.rsi/jobs.toml" recipes.desktop-e2e-graph target)" "recipe target"
    assert_contains "$(cat "$REPO_ROOT/Makefile")" $'desktop-e2e-graph-job:\n\t./scripts/desktop-e2e-graph-job.sh'
    assert_executable "$WRAPPER"
}

case_the_script_runs_the_seeded_build_and_the_receipt_judge() {
    local text needle
    text="$(cat "$WRAPPER")"
    for needle in \
        "cargo build -p rsid --features e2e-seed --bin rsid --bin rsi-e2e-seed" \
        "--isolated-write" \
        "--only graph-run" \
        "graph-run-receipt.mjs" \
        "RSI_E2E_RSID"; do
        assert_contains "$text" "$needle"
    done
}

# The default OUT is under the job's TMPDIR, removed after the job: the raw report
# must reach the log (#3292).
case_the_raw_report_is_echoed_into_the_durable_log_before_the_judge() {
    local text
    text="$(cat "$WRAPPER")"
    assert_contains "$text" 'cat "$OUT/e2e-report.json"'
    assert_contains "$text" "--- e2e-report.json (retained in this log) ---"
    assert_contains "$text" "--- end e2e-report.json ---"
    # The judge INVOCATION must exist (the bare filename elsewhere is not enough) and come last.
    assert_before "$text" 'cat "$OUT/e2e-report.json"' "node scripts/graph-run-receipt.mjs" "the raw report is echoed before the judge"
}

case_arguments_are_refused() {
    run_capture bash "$WRAPPER" --x
    assert_code 2 "exit code"
    assert_contains "$RUN_ERR" "desktop_e2e_graph_job_args_refused"
}

run_case "the recipe is declared and maps to a make target" case_the_recipe_is_declared_and_maps_to_a_make_target
run_case "the script runs the seeded build and the receipt judge" case_the_script_runs_the_seeded_build_and_the_receipt_judge
run_case "the raw report is echoed into the durable log before the judge" case_the_raw_report_is_echoed_into_the_durable_log_before_the_judge
run_case "arguments are refused" case_arguments_are_refused
tap_done
