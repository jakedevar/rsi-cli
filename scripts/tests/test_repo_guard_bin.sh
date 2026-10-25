#!/usr/bin/env bash
# suite-gate: tap
# #3872: `scripts/repo-guard` reuses an already built rsi-repo-guard only when it passes a
# capability probe, not on file times alone.
#
# A binary seeded from a target pool, or built from another commit, can be newer than every
# source file and still lack guards this tree has (six rsid fixture tests failed with `unknown
# guard 'released-migrations'`; a pre-port build made cargo-slot fail every governor call). The
# probe asks the binary (`--list`): it must list the guard being run and as many guards as the
# tree's GUARDS table declares. These cases run a copy of the script in a scratch checkout with a
# fake `cargo`, so nothing is compiled. #676 also covers fail-closed no-build
# resolution; real content-stamp and staged-check coverage lives in
# crates/rsi-repo-guards/tests/repo_guard_no_build.rs.
source "$(dirname "$0")/lib/tap.sh"

LIST_BOTH=$'alpha\tcrates/rsi-repo-guards/src/alpha.rs\nbeta\tcrates/rsi-repo-guards/src/beta.rs'

# fake_guard <path> <list text>: a stand-in binary that answers --list and otherwise says it ran.
fake_guard() {
    write_exec "$1" "#!/bin/sh
if [ \"\$1\" = --list ]; then
  printf '%s\\n' '${2//\'/}'
  exit \"\${FAKE_GUARD_LIST_EXIT:-0}\"
fi
if [ \"\$1\" = cargo-slot ] && [ \"\$2\" = verify-build ]; then
  exit \"\${FAKE_GUARD_STAMP_EXIT:-0}\"
fi
echo \"ran \$*\"
exit \"\${FAKE_GUARD_EXIT:-0}\"
"
}

# checkout: a scratch checkout whose GUARDS table declares alpha and beta, a copy of the script
# and a fake cargo that records its calls and installs the built.complete or built.stale binary.
checkout() {
    ROOT="$CASE_DIR/root"
    mkdir -p "$ROOT/scripts" "$ROOT/crates/rsi-repo-guards/src" "$ROOT/bin"
    cp "$SCRIPTS_DIR/repo-guard" "$ROOT/scripts/repo-guard"
    write_file "$ROOT/Cargo.toml" "[workspace]"
    write_file "$ROOT/Cargo.lock" "# lock"
    write_file "$ROOT/crates/rsi-repo-guards/src/lib.rs" 'pub const GUARDS: &[(&str, GuardMain, &str)] = &[
    (
        "alpha",
        alpha::main,
        "crates/rsi-repo-guards/src/alpha.rs",
    ),
    (
        "beta",
        beta::main,
        "crates/rsi-repo-guards/src/beta.rs",
    ),
];
'
    # Sources are old, the binary (made after) is newer: the file-time check alone would reuse it.
    find "$ROOT/crates" "$ROOT/Cargo.toml" "$ROOT/Cargo.lock" -exec touch -t 200109090146.40 {} +
    BIN="$ROOT/target/release/rsi-repo-guard"
    mkdir -p "$ROOT/target/release" "$ROOT/bin"
    CALLS="$CASE_DIR/cargo.calls"
    : >"$CALLS"
    fake_guard "$CASE_DIR/built.complete" "$LIST_BOTH"
    fake_guard "$CASE_DIR/built.stale" "alpha	crates/rsi-repo-guards/src/alpha.rs"
    printf complete >"$CASE_DIR/build.kind"
    write_exec "$ROOT/bin/cargo" "#!/bin/sh
echo \"cargo \$*\" >>\"$CALLS\"
cp \"$CASE_DIR/built.\$(cat \"$CASE_DIR/build.kind\")\" \"$BIN\"
chmod +x \"$BIN\"
printf \"{\\\"reason\\\":\\\"compiler-artifact\\\",\\\"executable\\\":\\\"%s\\\"}\\n\" \"$BIN\"
"
    GUARD_ENV="CARGO_TARGET_DIR=$ROOT/target PATH=$ROOT/bin:$PATH"
}

# repo_guard <args...>: the script in the scratch checkout; sets RUN_CODE/RUN_OUT/RUN_ERR.
repo_guard() {
    # shellcheck disable=SC2086
    run_capture env $GUARD_ENV RSI_REPO_GUARD_NO_BUILD="${NO_BUILD:-0}" sh "$ROOT/scripts/repo-guard" "$@"
}

calls() { grep -c . "$CALLS" || true; }

case_a_complete_fresh_binary_is_reused_without_starting_cargo() {
    checkout
    fake_guard "$BIN" "$LIST_BOTH"
    repo_guard --bin
    assert_code 0 "--bin with a complete binary"
    assert_eq "$BIN" "$RUN_OUT" "the binary path"
    assert_eq "0" "$(calls)" "cargo calls"
    repo_guard beta one two
    assert_code 0 "a guard the binary lists"
    assert_eq "ran beta one two" "$RUN_OUT" "the guard ran"
    assert_eq "0" "$(calls)" "cargo calls after running a guard"
}

case_a_newer_binary_with_fewer_guards_than_the_tree_is_rebuilt() {
    checkout
    # Newer than every source (the old mtime check passes it) but built before `beta` existed.
    fake_guard "$BIN" "alpha	crates/rsi-repo-guards/src/alpha.rs"
    repo_guard --bin
    assert_code 0 "--bin with a stale binary"
    assert_eq "$BIN" "$RUN_OUT" "the binary path"
    assert_contains "$RUN_ERR" "is stale (it lists 1 guards, this tree declares 2); rebuilding" "the reason is named"
    assert_eq "1" "$(calls)" "cargo calls"
    repo_guard beta
    assert_code 0 "the guard the stale binary lacked"
    assert_eq "ran beta" "$RUN_OUT" "the rebuilt binary ran it"
    assert_eq "1" "$(calls)" "the rebuilt binary is reused"
}

case_a_binary_lacking_the_requested_guard_is_rebuilt_but_other_guards_reuse_it() {
    checkout
    # Same guard count as the tree, but `beta` is not among them (a binary from another commit).
    fake_guard "$BIN" $'alpha\tcrates/rsi-repo-guards/src/alpha.rs\ngamma\tcrates/rsi-repo-guards/src/gamma.rs'
    repo_guard alpha x
    assert_code 0 "a guard the binary lists"
    assert_eq "ran alpha x" "$RUN_OUT" "alpha ran from the existing binary"
    assert_eq "0" "$(calls)" "cargo calls for alpha"
    repo_guard beta y
    assert_code 0 "the guard the binary lacks"
    assert_contains "$RUN_ERR" "does not list the guard 'beta'" "the reason"
    assert_eq "ran beta y" "$RUN_OUT" "beta ran from the rebuilt binary"
    assert_eq "1" "$(calls)" "cargo calls for beta"
}

case_a_build_that_still_fails_the_probe_is_refused_fail_closed() {
    checkout
    fake_guard "$BIN" "alpha	crates/rsi-repo-guards/src/alpha.rs"
    printf stale >"$CASE_DIR/build.kind"
    repo_guard --bin
    assert_code 2 "a rebuilt binary that is still incomplete"
    assert_eq "" "$RUN_OUT" "no path is printed"
    assert_contains "$RUN_ERR" "fails its capability probe (it lists 1 guards, this tree declares 2)" "the refusal"
}

case_no_build_runs_matching_prebuilt_and_resolution_flags_without_cargo() {
    checkout
    NO_BUILD=1
    fake_guard "$BIN" "$LIST_BOTH"
    repo_guard --bin beta
    assert_code 0 "matching prebuilt resolution"
    assert_eq "$BIN" "$RUN_OUT" "resolved path"
    repo_guard --existing-bin
    assert_code 0 "admission bootstrap stays non-building"
    assert_eq "$BIN" "$RUN_OUT" "bootstrap path"
    repo_guard --list
    assert_code 0 "guard listing"
    assert_eq "$LIST_BOTH" "$RUN_OUT" "listing from the prebuilt"
    repo_guard beta --staged
    assert_code 0 "actual guard invocation"
    assert_eq "ran beta --staged" "$RUN_OUT" "arguments reach the guard"
    assert_eq "0" "$(calls)" "no-build cargo calls"
}

no_build_refuses() {
    repo_guard "$@"
    assert_code 2 "no-build resolution refuses"
    assert_eq "" "$RUN_OUT" "no success path is printed"
    assert_contains "$RUN_ERR" "No-build mode requires an authorized, platform-compatible prebuild" "remediation"
    assert_eq "0" "$(calls)" "no Cargo fallback"
}

case_no_build_cold_target_refuses_for_every_entry_point() {
    checkout
    NO_BUILD=1
    no_build_refuses beta --staged
    no_build_refuses --bin beta
    no_build_refuses --list
    no_build_refuses --existing-bin
}

case_no_build_stale_incompatible_and_failed_probe_refuse() {
    checkout
    NO_BUILD=1
    fake_guard "$BIN" "$LIST_BOTH"
    export FAKE_GUARD_STAMP_EXIT=1
    no_build_refuses beta
    unset FAKE_GUARD_STAMP_EXIT
    write_exec "$BIN" "#!/bin/sh
exit 126
"
    no_build_refuses beta
    fake_guard "$BIN" "$LIST_BOTH"
    export FAKE_GUARD_LIST_EXIT=1
    no_build_refuses beta
    assert_contains "$RUN_ERR" "--list failed" "capability probe failed"
}

case_no_build_checks_requested_command_and_guard_count() {
    checkout
    NO_BUILD=1
    fake_guard "$BIN" $'alpha\tcrates/rsi-repo-guards/src/alpha.rs\ngamma\tcrates/rsi-repo-guards/src/gamma.rs'
    no_build_refuses --bin beta
    assert_contains "$RUN_ERR" "does not list the guard 'beta'" "requested command checked for --bin"
    no_build_refuses beta
    fake_guard "$BIN" "alpha	crates/rsi-repo-guards/src/alpha.rs"
    no_build_refuses alpha
    assert_contains "$RUN_ERR" "it lists 1 guards, this tree declares 2" "guard count checked"
}

case_no_build_propagates_guard_failure_without_cargo() {
    checkout
    NO_BUILD=1
    fake_guard "$BIN" "$LIST_BOTH"
    export FAKE_GUARD_EXIT=7
    repo_guard beta --staged
    assert_code 7 "the actual guard verdict is preserved"
    assert_eq "ran beta --staged" "$RUN_OUT" "guard ran with requested arguments"
    assert_eq "0" "$(calls)" "guard failure never builds"
}

case_no_build_searches_debug_and_native_exe_candidates() {
    checkout
    NO_BUILD=1
    must mkdir -p "$ROOT/target/debug"
    fake_guard "$ROOT/target/debug/rsi-repo-guard.exe" "$LIST_BOTH"
    repo_guard --bin beta
    assert_code 0 "debug .exe candidate"
    assert_eq "$ROOT/target/debug/rsi-repo-guard.exe" "$RUN_OUT" "native candidate path"
    assert_eq "0" "$(calls)" "no Cargo fallback"
}

case_invalid_no_build_setting_refuses_instead_of_building() {
    checkout
    NO_BUILD=yes
    no_build_refuses beta
    assert_contains "$RUN_ERR" "RSI_REPO_GUARD_NO_BUILD must be 0 or 1" "invalid setting"
}

case_default_cold_target_still_builds_and_runs_the_guard() {
    checkout
    repo_guard beta --staged
    assert_code 0 "ordinary cold fallback"
    assert_eq "ran beta --staged" "$RUN_OUT" "built guard runs"
    assert_eq "1" "$(calls)" "default mode builds once"
}

run_case "a complete fresh binary is reused without starting cargo" case_a_complete_fresh_binary_is_reused_without_starting_cargo
run_case "a newer binary with fewer guards than the tree is rebuilt" case_a_newer_binary_with_fewer_guards_than_the_tree_is_rebuilt
run_case "a binary lacking the requested guard is rebuilt but other guards reuse it" case_a_binary_lacking_the_requested_guard_is_rebuilt_but_other_guards_reuse_it
run_case "a build that still fails the probe is refused, fail closed" case_a_build_that_still_fails_the_probe_is_refused_fail_closed
run_case "no-build runs matching prebuilt and resolution flags without cargo" case_no_build_runs_matching_prebuilt_and_resolution_flags_without_cargo
run_case "no-build cold target refuses for every entry point" case_no_build_cold_target_refuses_for_every_entry_point
run_case "no-build stale incompatible and failed probe refuse" case_no_build_stale_incompatible_and_failed_probe_refuse
run_case "no-build checks requested command and guard count" case_no_build_checks_requested_command_and_guard_count
run_case "no-build propagates guard failure without cargo" case_no_build_propagates_guard_failure_without_cargo
run_case "no-build searches debug and native exe candidates" case_no_build_searches_debug_and_native_exe_candidates
run_case "invalid no-build setting refuses instead of building" case_invalid_no_build_setting_refuses_instead_of_building
run_case "default cold target still builds and runs the guard" case_default_cold_target_still_builds_and_runs_the_guard
tap_done
