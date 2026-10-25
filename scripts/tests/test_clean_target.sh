#!/usr/bin/env bash
# suite-gate: tap
# scripts/clean-target frees a sandbox's build output but keeps the harness tmp (#288).
# Port of the Python suite of the same name (#366).
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/clean-target"

run_script() { # [args...] (in the case directory)
    run_capture bash "$SCRIPT" "$@"
}

case_keeps_only_the_harness_tmp() {
    local target="$CASE_DIR/target"
    mkdir -p "$target/debug/deps" "$target/.rsi-tmp/claude-1000"
    printf x >"$target/debug/deps/a.rlib"
    printf live >"$target/.rsi-tmp/claude-1000/task.output"
    printf sig >"$target/CACHEDIR.TAG"
    printf h >"$target/.hidden"
    run_script "$CASE_DIR"
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_dir_only "$target" .rsi-tmp "what remains"
    assert_eq live "$(cat "$target/.rsi-tmp/claude-1000/task.output")" "the live harness file"
}

case_a_missing_target_is_a_noop() {
    run_script "$CASE_DIR"
    assert_code 0 "exit code (stderr: $RUN_ERR)"
    assert_no_file "$CASE_DIR/target" "target"
}

case_a_symlinked_target_is_refused() {
    mkdir "$CASE_DIR/real"
    printf k >"$CASE_DIR/real/keep"
    ln -s "$CASE_DIR/real" "$CASE_DIR/target"
    run_script "$CASE_DIR"
    assert_code 1 "exit code"
    assert_file "$CASE_DIR/real/keep" "the symlink's destination"
}

case_two_arguments_are_a_usage_error() {
    run_script a b
    assert_code 2 "exit code"
}

run_case "keeps only the harness tmp" case_keeps_only_the_harness_tmp
run_case "a missing target is a no-op" case_a_missing_target_is_a_noop
run_case "a symlinked target is refused" case_a_symlinked_target_is_refused
run_case "two arguments are a usage error" case_two_arguments_are_a_usage_error
tap_done
