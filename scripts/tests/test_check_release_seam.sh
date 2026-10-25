#!/usr/bin/env bash
# suite-gate: tap
# scripts/check-release-seam.sh refuses a release build that enables a test seam
# (#1021 S4). Port of the Python suite of the same name (#366).
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/check-release-seam.sh"

# run_check <tree text> [cargo exit code]: the script against a fake `cargo tree`.
run_check() {
    local tree="$1" code="${2:-0}"
    printf '#!/bin/sh\ncat <<'"'"'TREE'"'"'\n%s\nTREE\nexit %s\n' "$tree" "$code" >"$CASE_DIR/cargo"
    chmod +x "$CASE_DIR/cargo"
    CHECK_RELEASE_SEAM_CARGO="$CASE_DIR/cargo" run_capture bash "$SCRIPT"
}

case_a_seam_free_graph_passes() {
    run_check $'rsid v0.1.0 (/x/crates/rsid)\nrsid-store v0.1.0 (/x/crates/rsid-store) \nrsid-core v0.1.0 (/x) '
    assert_code 0 "exit code"
}

case_a_seam_on_a_normal_edge_is_refused_and_named() {
    local features
    for features in test-seam foo,test-seam test-seam,bar; do
        run_check "rsid-store v0.1.0 (/x/crates/rsid-store) $features"$'\nrsid v0.1.0 (/x)'
        assert_code 1 "exit code for $features"
        assert_contains "$RUN_ERR" "rsid-store" "$features"
        assert_contains "$RUN_ERR" "is enabled in the release dependency graph" "$features"
    done
}

case_the_e2e_seed_feature_on_a_normal_edge_is_refused_and_named() {
    local features
    for features in e2e-seed foo,e2e-seed e2e-seed,bar; do
        run_check "rsid v0.1.0 (/x/crates/rsid) $features"
        assert_code 1 "exit code for $features"
        assert_contains "$RUN_ERR" "e2e-seed" "$features"
    done
}

case_a_similarly_named_feature_is_not_the_seam() {
    run_check "rsid-store v0.1.0 (/x) test-seamless,not-test-seam"
    assert_code 0 "exit code"
}

case_an_unreadable_graph_fails_closed() {
    run_check "" 101
    assert_code 2 "exit code"
}

# Runs the real `cargo tree` of this workspace (as the Python suite did).
case_the_real_workspace_release_graph_has_no_seam() {
    cd "$REPO_ROOT" || exit 1
    run_capture bash "$SCRIPT"
    assert_code 0 "exit code (stderr: $RUN_ERR)"
}

case_every_release_build_path_runs_the_check() {
    local path
    for path in scripts/install-release.sh scripts/build-aws-artifacts.sh Makefile; do
        assert_contains "$(cat "$REPO_ROOT/$path")" "check-release-seam.sh" "$path"
    done
}

run_case "a seam-free graph passes" case_a_seam_free_graph_passes
run_case "a seam on a normal edge is refused and named" case_a_seam_on_a_normal_edge_is_refused_and_named
run_case "the e2e-seed feature on a normal edge is refused and named" case_the_e2e_seed_feature_on_a_normal_edge_is_refused_and_named
run_case "a similarly named feature is not the seam" case_a_similarly_named_feature_is_not_the_seam
run_case "an unreadable graph fails closed" case_an_unreadable_graph_fails_closed
run_case "the real workspace release graph has no seam" case_the_real_workspace_release_graph_has_no_seam
run_case "every release build path runs the check" case_every_release_build_path_runs_the_check
tap_done
