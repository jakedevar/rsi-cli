#!/usr/bin/env bash
# suite-gate: tap
# #63, #465: `install-release.sh` asserts the agent git hooks on a host that never installed
# them: a repository with tools/git-hooks and no core.hooksPath gets the versioned hooks, an
# existing core.hooksPath is kept. Port of the retired Python suite
# scripts/tests/test_install_release_git_hooks.py (#366, no Python #358).
#
# Case map (legacy test -> case):
#   test_a_repository_without_hooks_gets_the_versioned_hooks
#       -> "a repository without hooks gets the versioned hooks"
#   test_an_existing_hooks_path_is_kept
#       -> "an existing hooks path is kept"
# Negative proofs added with the port (#465): "a repository without tools/git-hooks is left
# alone" and "a directory that is not a repository is left alone" (the function returns 0 and
# configures nothing).
#
# The function under test is cut out of install-release.sh (`ensure_git_hooks() { ... }`), as
# the legacy suite did, and run in a fresh bash against a fixture repository.
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/install-release.sh"

# ensure_git_hooks_run <root>: run the installer's function; sets RUN_CODE/RUN_OUT/RUN_ERR.
ensure_git_hooks_run() {
    local body
    body="$(awk '/^ensure_git_hooks\(\) \{$/ { on = 1 } on { print } on && /^}$/ { exit }' "$SCRIPT")" \
        || tap_fail "cannot read ensure_git_hooks from install-release.sh"
    [[ -n "$body" ]] || tap_fail "ensure_git_hooks is not defined in install-release.sh"
    run_capture bash -c "$body"$'\n''ensure_git_hooks "$1"' ensure "$1"
}

hooks_path() { git -C "$1" config --get core.hooksPath || true; }

case_a_repository_without_hooks_gets_the_versioned_hooks() {
    fixture_repo "$CASE_DIR/repo"
    mkdir -p "$CASE_DIR/repo/tools/git-hooks"
    ensure_git_hooks_run "$CASE_DIR/repo"
    assert_code 0 "ensure_git_hooks"
    assert_eq "tools/git-hooks" "$(hooks_path "$CASE_DIR/repo")" "core.hooksPath"
    assert_contains "$RUN_OUT" "Installed git hooks: core.hooksPath -> tools/git-hooks" "the install line"
}

case_an_existing_hooks_path_is_kept() {
    fixture_repo "$CASE_DIR/repo"
    mkdir -p "$CASE_DIR/repo/tools/git-hooks"
    git -C "$CASE_DIR/repo" config core.hooksPath /opt/operator-hooks || tap_fail "cannot set core.hooksPath"
    ensure_git_hooks_run "$CASE_DIR/repo"
    assert_code 0 "ensure_git_hooks"
    assert_eq "/opt/operator-hooks" "$(hooks_path "$CASE_DIR/repo")" "core.hooksPath"
    assert_not_contains "$RUN_OUT" "Installed git hooks" "no install line for a kept path"
}

case_a_repository_without_tools_git_hooks_is_left_alone() {
    fixture_repo "$CASE_DIR/repo"
    ensure_git_hooks_run "$CASE_DIR/repo"
    assert_code 0 "ensure_git_hooks"
    assert_eq "" "$(hooks_path "$CASE_DIR/repo")" "core.hooksPath"
    assert_eq "" "$RUN_OUT" "output"
}

case_a_directory_that_is_not_a_repository_is_left_alone() {
    mkdir -p "$CASE_DIR/plain/tools/git-hooks"
    ensure_git_hooks_run "$CASE_DIR/plain"
    assert_code 0 "ensure_git_hooks"
    assert_eq "" "$RUN_OUT" "output"
    assert_no_file "$CASE_DIR/plain/.git" "no repository is created"
}

run_case "a repository without hooks gets the versioned hooks" case_a_repository_without_hooks_gets_the_versioned_hooks
run_case "an existing hooks path is kept" case_an_existing_hooks_path_is_kept
run_case "a repository without tools/git-hooks is left alone" case_a_repository_without_tools_git_hooks_is_left_alone
run_case "a directory that is not a repository is left alone" case_a_directory_that_is_not_a_repository_is_left_alone
tap_done
