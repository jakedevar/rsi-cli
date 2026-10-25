#!/usr/bin/env bash
# suite-gate: tap
# #1137, hub #2958, #1164, #1741, #465: `install-release.sh --link-only` links rsi-spill next
# to rsi-rpc and the link runs as printed; the links resolve to stable copies under ~/.rsi
# (never into the installing checkout); cargo-slot and target-pool are installed atomically.
# Port of the retired Python suite scripts/tests/test_install_release_links.py (#366, no
# Python #358) for the same installer, run against a copy of scripts/ with a stub cargo.
#
# Case map (legacy test -> case):
#   test_link_only_links_rsi_spill_next_to_rsi_rpc
#       -> "link-only links rsi-spill next to rsi-rpc, to a stable copy"
#   test_rsi_spill_link_never_resolves_into_the_checkout
#       -> "the rsi-spill link never resolves into the checkout"
#   test_linked_rsi_spill_survives_removal_of_the_checkout_copy
#       -> "the linked rsi-spill survives removal of the checkout"
#   test_rsi_spill_is_atomically_refreshed_and_repoints_an_old_checkout_link
#       -> "rsi-spill is refreshed atomically and an old checkout link is repointed"
#   test_link_only_installs_rsid_under_rsi_install_and_links_there
#       -> "link-only installs the binaries under ~/.rsi/install and links there"
#   test_cargo_slot_is_installed_and_atomically_refreshed
#       -> "cargo-slot and its wrapper are installed and refreshed atomically"
#   test_target_pool_is_installed_for_the_daemon_seed
#       -> "target-pool is installed for the daemon seed"
#   test_linked_rsi_spill_runs_from_another_cwd_via_path_rsi_rpc
#       -> "the linked rsi-spill runs from another cwd through PATH rsi-rpc"
#   test_linked_rsi_spill_falls_back_to_the_installed_rsi_rpc
#       -> "the linked rsi-spill falls back to the installed rsi-rpc"
# Negative proofs added with the port (#465): "a link-only run refuses a release dir this
# checkout did not build" and "a release dir without a build record is refused" (the #938
# marker: nothing is linked or installed after the refusal).
#
# The installer's helpers (release-target-guard.sh keep/prune, repo-guard-bin.sh) find the
# rsi-repo-guard binary through RSI_REPO_GUARD_BIN, built once for the suite.
source "$(dirname "$0")/lib/tap.sh"
source "$TAP_LIB_DIR/install-isolation.sh"

RELEASE_BINS="rsi rsid rsi-rpc rsi-agent-mcp rsi-build-rustc rsi-contract-validate rsi-known-failure rsi-turn-shim rsi-socket-hold"

if ! RSI_REPO_GUARD_BIN="$("$SCRIPTS_DIR/repo-guard" --bin)"; then
    echo "Bail out! scripts/repo-guard could not build rsi-repo-guard" >&2
    exit 1
fi
export RSI_REPO_GUARD_BIN

mode_of() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }

# resolve <path>: the physical path a (possibly symlinked) file resolves to.
resolve() {
    local p="$1" t
    while [[ -L "$p" ]]; do
        t="$(readlink "$p")"
        case "$t" in
            /*) p="$t" ;;
            *) p="$(dirname -- "$p")/$t" ;;
        esac
    done
    printf '%s/%s\n' "$(cd -- "$(dirname -- "$p")" && pwd -P)" "$(basename -- "$p")"
}

# fixture: a copy of scripts/ (without tests) as a checkout, a stub cargo, fake release binaries.
fixture() {
    BASE="$CASE_DIR/base"
    HOME_DIR="$BASE/home"
    BIN_DIR="$HOME_DIR/.local/bin"
    ROOT="$BASE/repo"
    TARGET="$BASE/target"
    FAKES="$BASE/fakes"
    mkdir -p "$BASE" "$ROOT" "$FAKES" "$HOME_DIR"
    write_exec "$FAKES/cargo" $'#!/bin/sh\nprintf \'{"target_directory":"%s"}\\n\' "$CARGO_TARGET_DIR"\n'
    write_exec "$FAKES/rustup" $'#!/bin/sh\nexit 99\n'
    install_isolation_fixture
    cp -R "$SCRIPTS_DIR" "$ROOT/scripts" || tap_fail "cannot copy scripts/"
    rm -rf "$ROOT/scripts/tests"
    printf '[workspace]\n[package]\nname = "stub"\nversion = "0.0.0"\nedition = "2021"\n' >"$ROOT/Cargo.toml"
    write_exec "$ROOT/src/lib.rs" ""
    local name
    for name in $RELEASE_BINS; do
        write_exec "$TARGET/release/$name" "#!/bin/sh
echo \"fake-$name \$*\"
"
    done
    # #938: --link-only links only what this checkout built.
    printf 'worktree=%s\nstate=built\nhead=unknown\n' "$(cd -- "$ROOT" && pwd -P)" >"$TARGET/release/.rsi-release-source"
}

# install [flags...]: the installer, `--link-only --no-restart`; sets RUN_CODE/RUN_OUT/RUN_ERR.
install() {
    local -a INSTALL_ARGS=(--link-only --no-restart "$@")
    run_capture install_isolated
    assert_dir_empty "$CALLED" "--no-restart made a lifecycle call"
}

install_ok() {
    install "$@"
    assert_code 0 "install-release.sh --link-only"
}

# run_linked <cwd> <PATH>: the linked rsi-spill under a bare environment.
run_linked() {
    cd "$1" || tap_fail "cannot enter $1"
    run_capture env -i HOME="$HOME_DIR" PATH="$2" rsi-spill show s/1 --range 1:3
    cd "$CASE_DIR" || tap_fail "cannot return to $CASE_DIR"
}

# leftovers <dir> <prefix>: temp files an interrupted atomic copy would leave behind.
leftovers() {
    ls -A "$1" | grep -F "$2" || true
}

case_link_only_links_rsi_spill_next_to_rsi_rpc() {
    fixture
    install_ok
    local link="$BIN_DIR/rsi-spill" stable="$HOME_DIR/.rsi/bin/rsi-spill"
    [[ -L "$BIN_DIR/rsi-rpc" ]] || tap_fail "rsi-rpc is not linked"
    [[ -L "$link" ]] || tap_fail "rsi-spill is not linked"
    assert_eq "$(resolve "$stable")" "$(resolve "$link")" "the link resolves to the stable copy"
    assert_contains "$RUN_OUT" "$link ->" "the link is printed"
    assert_same_bytes "$stable" "$ROOT/scripts/rsi-spill" "stable copy bytes"
    [[ -f "$stable" && ! -L "$stable" ]] || tap_fail "the stable copy is not a regular file"
    assert_eq 755 "$(mode_of "$stable")" "stable copy mode"
    assert_eq "" "$(leftovers "$(dirname -- "$stable")" .rsi-spill.)" "temp copies left behind"
}

case_the_rsi_spill_link_never_resolves_into_the_checkout() {
    fixture
    install_ok
    local resolved checkout
    resolved="$(resolve "$BIN_DIR/rsi-spill")"
    checkout="$(cd -- "$ROOT" && pwd -P)"
    assert_not_contains "$resolved/" "$checkout/" "the link resolves into the checkout"
}

case_the_linked_rsi_spill_survives_removal_of_the_checkout() {
    fixture
    install_ok
    rm -rf "$ROOT"
    [[ -e "$BIN_DIR/rsi-spill" ]] || tap_fail "the link dangles after the checkout is removed"
    mkdir -p "$BASE/elsewhere"
    run_linked "$BASE/elsewhere" "$BIN_DIR:/usr/bin:/bin"
    assert_code 0 "rsi-spill after the checkout is gone"
    assert_eq "fake-rsi-rpc spill show s/1 --range 1:3" "$RUN_OUT" "what the linked rsi-spill ran"
}

case_rsi_spill_is_refreshed_atomically_and_an_old_checkout_link_is_repointed() {
    fixture
    local stable="$HOME_DIR/.rsi/bin/rsi-spill" original
    # An older install linked the checkout script directly.
    mkdir -p "$BIN_DIR"
    ln -s "$ROOT/scripts/rsi-spill" "$BIN_DIR/rsi-spill"
    install_ok
    assert_eq "$(resolve "$stable")" "$(resolve "$BIN_DIR/rsi-spill")" "the old link is repointed"
    read_exact original "$stable"
    exec 7<"$stable"
    printf '%s\n# updated release\n' "$original" >"$ROOT/scripts/rsi-spill"
    install_ok
    local seen
    seen="$(cat <&7 && printf x)"
    seen="${seen%x}"
    exec 7<&-
    assert_eq "$original" "$seen" "an open reader keeps the old copy"
    assert_same_bytes "$stable" "$ROOT/scripts/rsi-spill" "refreshed copy"
    assert_eq "" "$(leftovers "$(dirname -- "$stable")" .rsi-spill.)" "temp copies left behind"
}

case_link_only_installs_the_binaries_under_rsi_install_and_links_there() {
    fixture
    install_ok
    local installed="$HOME_DIR/.rsi/install" name
    for name in $RELEASE_BINS; do
        [[ -f "$installed/$name" && ! -L "$installed/$name" ]] || tap_fail "$name is not a regular copy"
        [[ -x "$installed/$name" ]] || tap_fail "$name is not executable"
    done
    for name in rsi-known-failure rsi-socket-hold rsi-turn-shim rsid; do
        assert_eq "$(resolve "$installed/$name")" "$(resolve "$BIN_DIR/$name")" "$name link"
    done
    assert_ne "$(resolve "$TARGET/release/rsid")" "$(resolve "$BIN_DIR/rsid")" "rsid must not link into the build output"
    # Reinstalling replaces the file atomically with the new build.
    write_exec "$TARGET/release/rsid" $'#!/bin/sh\necho "fake-rsid-v2"\n'
    install_ok
    assert_contains "$(cat "$installed/rsid")" fake-rsid-v2 "the new build"
}

case_cargo_slot_and_its_wrapper_are_installed_and_refreshed_atomically() {
    fixture
    local source="$ROOT/scripts/cargo-slot" wrapper="$ROOT/scripts/cargo-slot-wrapper"
    local installed="$HOME_DIR/.rsi/bin/cargo-slot" implementation="$HOME_DIR/.rsi/lib/cargo-slot" original
    install_ok
    assert_same_bytes "$installed" "$wrapper" "the wrapper"
    assert_same_bytes "$implementation" "$source" "the implementation"
    assert_eq 755 "$(mode_of "$installed")" "wrapper mode"
    assert_eq 755 "$(mode_of "$implementation")" "implementation mode"
    # An open reader keeps the old implementation while install refreshes it.
    read_exact original "$implementation"
    exec 7<"$implementation"
    printf '%s\n# updated release\n' "$original" >"$source"
    chmod 600 "$source"
    install_ok
    local seen
    seen="$(cat <&7 && printf x)"
    seen="${seen%x}"
    exec 7<&-
    assert_eq "$original" "$seen" "an open reader keeps the old implementation"
    assert_same_bytes "$implementation" "$source" "refreshed implementation"
    assert_same_bytes "$installed" "$wrapper" "the wrapper after the refresh"
    assert_eq 755 "$(mode_of "$implementation")" "implementation mode after the refresh"
    assert_eq "" "$(leftovers "$(dirname -- "$installed")" .cargo-slot.)" "wrapper temp copies"
    assert_eq "" "$(leftovers "$(dirname -- "$implementation")" .cargo-slot.)" "implementation temp copies"
}

case_target_pool_is_installed_for_the_daemon_seed() {
    fixture
    local source="$ROOT/scripts/target-pool" installed="$HOME_DIR/.rsi/bin/target-pool"
    install_ok
    assert_same_bytes "$installed" "$source" "target-pool"
    assert_eq 755 "$(mode_of "$installed")" "target-pool mode"
    assert_eq "" "$(leftovers "$(dirname -- "$installed")" .target-pool.)" "temp copies"
}

case_the_linked_rsi_spill_runs_from_another_cwd_through_path_rsi_rpc() {
    fixture
    install_ok
    mkdir -p "$BASE/elsewhere"
    run_linked "$BASE/elsewhere" "$BIN_DIR:/usr/bin:/bin"
    assert_code 0 "rsi-spill from another cwd"
    assert_eq "fake-rsi-rpc spill show s/1 --range 1:3" "$RUN_OUT" "what the linked rsi-spill ran"
}

case_the_linked_rsi_spill_falls_back_to_the_installed_rsi_rpc() {
    fixture
    install_ok
    # No rsi-rpc on PATH: the stable copy has no checkout beside it, so it finds the
    # installed ~/.rsi/install/rsi-rpc instead.
    rm -f "$BIN_DIR/rsi-rpc"
    write_exec "$HOME_DIR/.rsi/install/rsi-rpc" $'#!/bin/sh\necho "debug-rsi-rpc $*"\n'
    mkdir -p "$BASE/elsewhere"
    run_linked "$BASE/elsewhere" "$BIN_DIR:/usr/bin:/bin"
    assert_code 0 "rsi-spill without rsi-rpc on PATH"
    assert_eq "debug-rsi-rpc spill show s/1 --range 1:3" "$RUN_OUT" "what the linked rsi-spill ran"
}

case_a_link_only_run_refuses_a_release_dir_this_checkout_did_not_build() {
    fixture
    printf 'worktree=%s\nstate=built\nhead=unknown\n' "$BASE/some-other-checkout" >"$TARGET/release/.rsi-release-source"
    install
    assert_ne 0 "$RUN_CODE" "a foreign build was linked"
    assert_contains "$RUN_ERR" "built from $BASE/some-other-checkout" "the refusal names the other worktree"
    assert_no_file "$BIN_DIR/rsi-spill" "nothing is linked after a refusal"
    assert_no_file "$HOME_DIR/.rsi/install/rsid" "nothing is installed after a refusal"
}

case_a_release_dir_without_a_build_record_is_refused() {
    fixture
    rm -f "$TARGET/release/.rsi-release-source"
    install
    assert_ne 0 "$RUN_CODE" "an unrecorded build was linked"
    assert_contains "$RUN_ERR" "No release build from" "the refusal says no build is recorded"
    assert_no_file "$BIN_DIR/rsid" "nothing is linked after a refusal"
}

run_case "link-only links rsi-spill next to rsi-rpc, to a stable copy" case_link_only_links_rsi_spill_next_to_rsi_rpc
run_case "the rsi-spill link never resolves into the checkout" case_the_rsi_spill_link_never_resolves_into_the_checkout
run_case "the linked rsi-spill survives removal of the checkout" case_the_linked_rsi_spill_survives_removal_of_the_checkout
run_case "rsi-spill is refreshed atomically and an old checkout link is repointed" case_rsi_spill_is_refreshed_atomically_and_an_old_checkout_link_is_repointed
run_case "link-only installs the binaries under ~/.rsi/install and links there" case_link_only_installs_the_binaries_under_rsi_install_and_links_there
run_case "cargo-slot and its wrapper are installed and refreshed atomically" case_cargo_slot_and_its_wrapper_are_installed_and_refreshed_atomically
run_case "target-pool is installed for the daemon seed" case_target_pool_is_installed_for_the_daemon_seed
run_case "the linked rsi-spill runs from another cwd through PATH rsi-rpc" case_the_linked_rsi_spill_runs_from_another_cwd_through_path_rsi_rpc
run_case "the linked rsi-spill falls back to the installed rsi-rpc" case_the_linked_rsi_spill_falls_back_to_the_installed_rsi_rpc
run_case "a link-only run refuses a release dir this checkout did not build" case_a_link_only_run_refuses_a_release_dir_this_checkout_did_not_build
run_case "a release dir without a build record is refused" case_a_release_dir_without_a_build_record_is_refused
tap_done
