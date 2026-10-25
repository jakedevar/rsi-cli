#!/usr/bin/env bash
# suite-gate: tap
# The test seam is excluded from release builds by the crates' build scripts (#1021 S4).
#
# `test-seam` carries fake capabilities and route-validation bypasses, so every crate
# that declares it refuses to build with it in the release profile. The guard text is
# the one in each build.rs (between the SEAM-GUARD markers); this suite compiles that
# text and runs it under the environment Cargo gives a build script. The `e2e-seed`
# feature (local #122) compiles the isolated-state fixture writer; no release may carry it.
# Port of the Python suite of the same name (#366).
source "$(dirname "$0")/lib/tap.sh"

CRATES="rsid-core rsid-store rsid"
BIN_DIR="$TAP_ROOT/guards"

# guard_text <crate> <BEGIN marker> <END marker>: the text between the markers of build.rs.
guard_text() {
    awk -v b="// $2" -v e="// $3" '$0==b{on=1;next} $0==e{on=0} on{print}' "$REPO_ROOT/crates/$1/build.rs"
}

# The build script must call the guard before it does anything else.
main_calls_first() { # <crate> <call>
    awk '/fn main\(\)/{on=1;next} on&&/[^[:space:]]/{print;exit}' "$REPO_ROOT/crates/$1/build.rs" | grep -q "^[[:space:]]*$2"
}

compile_guard() { # <name> <crate> <begin> <end> <fn>
    local text
    text="$(guard_text "$2" "$3" "$4")"
    [[ -n "$text" ]] || return 1
    mkdir -p "$BIN_DIR"
    printf '%s\nfn main() { %s(); }\n' "$text" "$5" >"$BIN_DIR/$1.rs"
    (cd "$REPO_ROOT" && rustc --edition 2024 -o "$BIN_DIR/$1" "$BIN_DIR/$1.rs") >"$BIN_DIR/$1.log" 2>&1
}

# run_guard <binary> <seam var name or -> <profile> [allow var=value]: environment the build script gets.
run_guard() {
    local binary="$1" feature_var="$2" profile="$3" allow="${4:-}" name
    local -a clean=()
    for name in $(compgen -e); do
        case "$name" in CARGO_FEATURE* | RSID_ALLOW*) clean+=(-u "$name") ;; esac
    done
    local -a extra=(PROFILE="$profile")
    [[ "$feature_var" != "-" ]] && extra+=("$feature_var=1")
    [[ -n "$allow" ]] && extra+=("$allow")
    run_capture env ${clean[@]+"${clean[@]}"} "${extra[@]}" "$BIN_DIR/$binary"
}

case_every_seam_crate_has_the_guard_and_calls_it_first() {
    local crate
    for crate in $CRATES; do
        [[ -n "$(guard_text "$crate" SEAM-GUARD-BEGIN SEAM-GUARD-END)" ]] || tap_fail "$crate/build.rs has no seam guard"
        main_calls_first "$crate" "refuse_test_seam_in_release();" || tap_fail "$crate: main must call the guard first"
    done
}

case_the_guards_compile() {
    local crate
    for crate in $CRATES; do
        compile_guard "$crate" "$crate" SEAM-GUARD-BEGIN SEAM-GUARD-END refuse_test_seam_in_release \
            || tap_fail "$crate guard did not compile: $(cat "$BIN_DIR/$crate.log")"
    done
    compile_guard e2e-seed rsid E2E-SEED-GUARD-BEGIN E2E-SEED-GUARD-END refuse_e2e_seed_in_release \
        || tap_fail "e2e-seed guard did not compile: $(cat "$BIN_DIR/e2e-seed.log" 2>/dev/null)"
    grep -q "refuse_e2e_seed_in_release();" <(awk '/fn main\(\)/{on=1} on{print} /^}/{if(on)exit}' "$REPO_ROOT/crates/rsid/build.rs") \
        || tap_fail "rsid/build.rs main must call the e2e-seed guard"
}

case_a_release_build_with_the_seam_is_refused_in_every_seam_crate() {
    local crate
    for crate in $CRATES; do
        run_guard "$crate" CARGO_FEATURE_TEST_SEAM release
        assert_ne 0 "$RUN_CODE" "$crate exit code"
        assert_contains "$RUN_ERR" '`test-seam` feature is enabled in a release build' "$crate"
    done
}

case_debug_builds_and_seam_free_release_builds_pass() {
    local crate
    for crate in $CRATES; do
        run_guard "$crate" CARGO_FEATURE_TEST_SEAM debug
        assert_code 0 "$crate with the seam in a debug build"
        run_guard "$crate" - release
        assert_code 0 "$crate seam-free release build"
    done
}

case_a_release_mode_test_run_must_opt_in_explicitly() {
    local crate
    for crate in $CRATES; do
        run_guard "$crate" CARGO_FEATURE_TEST_SEAM release RSID_ALLOW_TEST_SEAM_RELEASE=0
        assert_ne 0 "$RUN_CODE" "$crate with allow=0"
        run_guard "$crate" CARGO_FEATURE_TEST_SEAM release RSID_ALLOW_TEST_SEAM_RELEASE=yes
        assert_ne 0 "$RUN_CODE" "$crate with allow=yes"
        run_guard "$crate" CARGO_FEATURE_TEST_SEAM release RSID_ALLOW_TEST_SEAM_RELEASE=1
        assert_code 0 "$crate with allow=1"
    done
}

case_a_release_build_with_the_seed_feature_is_refused() {
    run_guard e2e-seed CARGO_FEATURE_E2E_SEED release
    assert_ne 0 "$RUN_CODE" "exit code"
    assert_contains "$RUN_ERR" '`e2e-seed` feature is enabled in a release build'
}

case_debug_and_seed_free_release_builds_pass_for_the_seed_guard() {
    run_guard e2e-seed CARGO_FEATURE_E2E_SEED debug
    assert_code 0 "debug build with the seed feature"
    run_guard e2e-seed - release
    assert_code 0 "seed-free release build"
}

# Hub #3244: the seeder writes a database, so unlike test-seam it has no release override.
case_no_opt_in_value_allows_a_release_build_with_the_seed_feature() {
    local allow name
    for allow in 0 yes 1; do
        for name in RSID_ALLOW_E2E_SEED_RELEASE RSID_ALLOW_TEST_SEAM_RELEASE; do
            run_guard e2e-seed CARGO_FEATURE_E2E_SEED release "$name=$allow"
            assert_ne 0 "$RUN_CODE" "$name=$allow exit code"
            assert_contains "$RUN_ERR" '`e2e-seed` feature is enabled in a release build' "$name=$allow"
        done
    done
}

case_the_seed_guard_reads_no_release_override() {
    assert_not_contains "$(cat "$BIN_DIR/e2e-seed.rs")" "RSID_ALLOW"
}

case_the_seeder_is_a_feature_gated_binary_and_never_a_default_feature() {
    local manifest
    manifest="$(cat "$REPO_ROOT/crates/rsid/Cargo.toml")"
    assert_contains "$manifest" $'name = "rsi-e2e-seed"\npath = "src/bin/rsi-e2e-seed.rs"\nrequired-features = ["e2e-seed"]'
    assert_contains "$manifest" 'default = []'
    if grep -Eq '^default = \[[^]]*e2e-seed' "$REPO_ROOT/crates/rsid/Cargo.toml"; then
        tap_fail "e2e-seed is a default feature"
    fi
}

if command -v rustc >/dev/null 2>&1; then
    run_case "every seam crate has the guard and calls it first" case_every_seam_crate_has_the_guard_and_calls_it_first
    # Compiled once, in the first case, for every case after it (cases run in subshells,
    # so the binaries live in $BIN_DIR, not in a variable).
    run_case "the guards compile" case_the_guards_compile
    run_case "a release build with the seam is refused in every seam crate" case_a_release_build_with_the_seam_is_refused_in_every_seam_crate
    run_case "debug builds and seam-free release builds pass" case_debug_builds_and_seam_free_release_builds_pass
    run_case "a release-mode test run must opt in explicitly" case_a_release_mode_test_run_must_opt_in_explicitly
    run_case "a release build with the seed feature is refused" case_a_release_build_with_the_seed_feature_is_refused
    run_case "debug and seed-free release builds pass for the seed guard" case_debug_and_seed_free_release_builds_pass_for_the_seed_guard
    run_case "no opt-in value allows a release build with the seed feature" case_no_opt_in_value_allows_a_release_build_with_the_seed_feature
    run_case "the seed guard reads no release override" case_the_seed_guard_reads_no_release_override
    run_case "the seeder is a feature-gated binary and never a default feature" case_the_seeder_is_a_feature_gated_binary_and_never_a_default_feature
else
    skip_case "the release seam guards" "requires rustc"
fi
tap_done
