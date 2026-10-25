#!/usr/bin/env bash
# suite-gate: tap
# #453: scripts/target-pool is a POSIX shim over the Rust pool tool (`rsi-repo-guard target-pool`,
# crates/rsi-repo-guards/src/target_pool*.rs). Every caller (cargo-slot's disk-floor reclaim, the
# daemon's seed/gc/refresh/summary, `make target-pool-build`) keeps the one path
# ~/.rsi/bin/target-pool, so the shim's only jobs are to find the binary and pass the command
# line through untouched. The pool itself is tested in Rust; the legacy Python suite's case map
# is the header of crates/rsi-repo-guards/src/target_pool_case_map.rs.
source "$(dirname "$0")/lib/tap.sh"

SHIM_SOURCE="$SCRIPTS_DIR/target-pool"

# install_shim <dir>: a copy of the shim at <dir>/scripts/target-pool (beside nothing else), so
# the lookups relative to the shim cannot reach this checkout's own target/ directory.
install_shim() {
    mkdir -p "$1/scripts"
    cp "$SHIM_SOURCE" "$1/scripts/target-pool"
    chmod 755 "$1/scripts/target-pool"
}

# fake_guard <path> <label> [exit code]: an executable that names itself and the line it got.
fake_guard() {
    write_exec "$1" "#!/bin/sh
printf '%s:' '$2'
for argument in \"\$@\"; do printf ' [%s]' \"\$argument\"; done
printf '\\n'
exit ${3:-0}
"
}

# shim <dir> <env assignments...> -- <arguments...>: run the installed shim with a clean
# environment: a scratch HOME, a PATH of the system tools only and the stated variables.
shim() {
    local dir="$1"; shift
    local assignments=()
    while [[ "$1" != -- ]]; do assignments+=("$1"); shift; done
    shift
    run_capture env -i HOME="$dir/home" PATH=/usr/bin:/bin ${assignments[@]+"${assignments[@]}"} \
        "$dir/scripts/target-pool" "$@"
}

case_the_shim_is_not_a_python_script() {
    local first
    first="$(head -n 1 "$SHIM_SOURCE")"
    assert_eq "#!/bin/sh" "$first" "the interpreter line"
    assert_not_contains "$(cat "$SHIM_SOURCE")" "python" "the shim"
    assert_executable "$SHIM_SOURCE"
    bash -n "$SHIM_SOURCE" || tap_fail "the shim does not parse"
}

case_the_command_line_and_the_exit_code_pass_through_untouched() {
    install_shim "$CASE_DIR"
    fake_guard "$CASE_DIR/guard" explicit 7
    shim "$CASE_DIR" RSI_REPO_GUARD_BIN="$CASE_DIR/guard" -- seed --sandbox "/a b/c" --build-missing ""
    assert_code 7 "the guard's exit code"
    assert_eq "explicit: [target-pool] [seed] [--sandbox] [/a b/c] [--build-missing] []" "$RUN_OUT" "the guard's argv"
    assert_eq "" "$RUN_ERR" "stderr"
}

case_the_binary_beside_the_shim_is_preferred_over_the_target_dir_and_the_install() {
    install_shim "$CASE_DIR"
    fake_guard "$CASE_DIR/scripts/rsi-repo-guard" sibling
    fake_guard "$CASE_DIR/target/release/rsi-repo-guard" target
    fake_guard "$CASE_DIR/home/.rsi/install/rsi-repo-guard" installed
    shim "$CASE_DIR" CARGO_TARGET_DIR="$CASE_DIR/target" -- status
    assert_code 0
    assert_eq "sibling: [target-pool] [status]" "$RUN_OUT" "which binary ran"
}

case_a_release_build_of_the_callers_target_beats_a_debug_one_and_the_install() {
    install_shim "$CASE_DIR"
    fake_guard "$CASE_DIR/target/debug/rsi-repo-guard" debug
    fake_guard "$CASE_DIR/target/release/rsi-repo-guard" release
    fake_guard "$CASE_DIR/home/.rsi/install/rsi-repo-guard" installed
    shim "$CASE_DIR" CARGO_TARGET_DIR="$CASE_DIR/target" -- summary
    assert_eq "release: [target-pool] [summary]" "$RUN_OUT" "release first"
    rm "$CASE_DIR/target/release/rsi-repo-guard"
    shim "$CASE_DIR" CARGO_TARGET_DIR="$CASE_DIR/target" -- summary
    assert_eq "debug: [target-pool] [summary]" "$RUN_OUT" "then debug"
    rm "$CASE_DIR/target/debug/rsi-repo-guard"
    shim "$CASE_DIR" CARGO_TARGET_DIR="$CASE_DIR/target" -- summary
    assert_eq "installed: [target-pool] [summary]" "$RUN_OUT" "then the installed binary"
}

case_a_checkout_default_target_is_found_beside_the_scripts_directory() {
    install_shim "$CASE_DIR"
    fake_guard "$CASE_DIR/target/debug/rsi-repo-guard" checkout
    shim "$CASE_DIR" -- key --repo .
    assert_code 0
    assert_eq "checkout: [target-pool] [key] [--repo] [.]" "$RUN_OUT" "the checkout's own build"
}

case_the_linked_install_and_path_are_the_last_places_looked() {
    install_shim "$CASE_DIR"
    fake_guard "$CASE_DIR/home/.local/bin/rsi-repo-guard" linked
    fake_guard "$CASE_DIR/path/rsi-repo-guard" path
    shim "$CASE_DIR" PATH="$CASE_DIR/path:/usr/bin:/bin" -- gc --json
    assert_eq "linked: [target-pool] [gc] [--json]" "$RUN_OUT" "~/.local/bin before PATH"
    rm "$CASE_DIR/home/.local/bin/rsi-repo-guard"
    shim "$CASE_DIR" PATH="$CASE_DIR/path:/usr/bin:/bin" -- gc --json
    assert_eq "path: [target-pool] [gc] [--json]" "$RUN_OUT" "PATH last"
}

case_a_candidate_that_is_not_executable_is_skipped() {
    install_shim "$CASE_DIR"
    write_file "$CASE_DIR/not-executable" "#!/bin/sh
echo wrong
"
    fake_guard "$CASE_DIR/home/.rsi/install/rsi-repo-guard" installed
    shim "$CASE_DIR" RSI_REPO_GUARD_BIN="$CASE_DIR/not-executable" -- status
    assert_eq "installed: [target-pool] [status]" "$RUN_OUT" "the executable one ran"
}

case_without_any_binary_it_exits_127_and_names_the_fix_and_runs_nothing() {
    install_shim "$CASE_DIR"
    shim "$CASE_DIR" -- reclaim-idle
    assert_code 127 "no guard binary"
    assert_eq "" "$RUN_OUT" "nothing on stdout"
    assert_contains "$RUN_ERR" "target-pool: rsi-repo-guard is not installed"
    assert_contains "$RUN_ERR" "make release-install"
    assert_contains "$RUN_ERR" "cargo build --release -p rsi-repo-guards --bin rsi-repo-guard"
}

case_the_installer_builds_and_publishes_the_binary_the_shim_looks_for() {
    local installer
    installer="$(cat "$SCRIPTS_DIR/install-release.sh")"
    assert_contains "$installer" "--bin rsi-repo-guard" "the release build"
    assert_contains "$installer" '"$RSI_HOME_DIR/bin/rsi-repo-guard"' "the published copy, beside ~/.rsi/bin/target-pool"
    assert_contains "$installer" '"$RSI_HOME_DIR/bin/target-pool"' "the shim copy"
}

run_case "the shim is a POSIX shell script, not Python" case_the_shim_is_not_a_python_script
run_case "the command line and the exit code pass through untouched" case_the_command_line_and_the_exit_code_pass_through_untouched
run_case "the binary beside the shim is preferred over the target dir and the install" case_the_binary_beside_the_shim_is_preferred_over_the_target_dir_and_the_install
run_case "a release build of the caller's target beats a debug one and the install" case_a_release_build_of_the_callers_target_beats_a_debug_one_and_the_install
run_case "a checkout's default target is found beside the scripts directory" case_a_checkout_default_target_is_found_beside_the_scripts_directory
run_case "the linked install and PATH are the last places looked" case_the_linked_install_and_path_are_the_last_places_looked
run_case "a candidate that is not executable is skipped" case_a_candidate_that_is_not_executable_is_skipped
run_case "without any binary it exits 127, names the fix and runs nothing" case_without_any_binary_it_exits_127_and_names_the_fix_and_runs_nothing
run_case "the installer builds and publishes the binary the shim looks for" case_the_installer_builds_and_publishes_the_binary_the_shim_looks_for
tap_done
