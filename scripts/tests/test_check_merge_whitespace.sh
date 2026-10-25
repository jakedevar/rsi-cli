#!/usr/bin/env bash
# suite-gate: tap
# scripts/check-merge-whitespace names a conflict in plain words and checks
# whitespace on a clean merge (#96). Port of the Python suite of the same name (#366).
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/check-merge-whitespace"

# A repository on `rolling` with one base commit that origin/rolling also points at.
setup_repo() {
    REPO="$CASE_DIR/repo"
    fixture_repo "$REPO"
    git -C "$REPO" branch -q -m rolling
    commit_file shared.txt $'one\ntwo\n' base
    git -C "$REPO" update-ref refs/remotes/origin/rolling HEAD
}

commit_file() { # <name> <text> <message>
    printf '%s' "$2" >"$REPO/$1"
    git -C "$REPO" add "$1"
    git -C "$REPO" commit -q -m "$3"
}

check() { # [args...]
    cd "$REPO" || return 1
    run_capture "$SCRIPT" "$@"
    cd "$CASE_DIR" || return 1
}

advance_rolling() { # <text>
    git -C "$REPO" switch -q -c upstream origin/rolling
    commit_file shared.txt "$1" "rolling advances"
    git -C "$REPO" update-ref refs/remotes/origin/rolling HEAD
    git -C "$REPO" switch -q source
}

case_a_clean_merge_without_whitespace_errors_passes() {
    setup_repo
    git -C "$REPO" switch -q -c source
    commit_file new.txt $'fine\n' "source change"
    check
    assert_code 0 "exit code"
    assert_contains "$RUN_OUT" "merges cleanly onto origin/rolling; no whitespace errors"
}

case_a_trailing_space_in_the_merged_source_fails() {
    setup_repo
    git -C "$REPO" switch -q -c source
    commit_file new.txt $'trailing \n' "source change"
    check
    assert_code 1 "exit code"
    assert_contains "$RUN_OUT" "new.txt:1: trailing whitespace."
}

case_a_conflict_names_the_paths_instead_of_a_git_argument_error() {
    setup_repo
    git -C "$REPO" switch -q -c source
    commit_file shared.txt $'one\nsource\n' "source edits shared"
    advance_rolling $'one\nrolling\n'
    check
    assert_code 2 "exit code"
    assert_contains "$RUN_ERR" "HEAD conflicts with origin/rolling; merge origin/rolling (never rebase)"
    assert_contains "$RUN_ERR" "  shared.txt"
}

case_an_unknown_base_is_named() {
    setup_repo
    check origin/missing
    assert_code 3 "exit code"
    assert_contains "$RUN_ERR" "cannot resolve origin/missing (fetch it first)"
}

run_case "a clean merge without whitespace errors passes" case_a_clean_merge_without_whitespace_errors_passes
run_case "a trailing space in the merged source fails" case_a_trailing_space_in_the_merged_source_fails
run_case "a conflict names the paths instead of a git argument error" case_a_conflict_names_the_paths_instead_of_a_git_argument_error
run_case "an unknown base is named" case_an_unknown_base_is_named
tap_done
