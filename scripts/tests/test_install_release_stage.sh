#!/usr/bin/env bash
# suite-gate: tap
# #136, #262 (hub #2799-#2802), #3423, #465: `install-release.sh --stage-dir DIR` stages the
# built set into DIR and touches nothing live (no ~/.rsi/install|bin|lib, no ~/.local/bin
# links, no supervisor, no restart) and prints a sha256 manifest; DIR is one fresh tree, never
# written through a symlink already in it; a staging build refuses while a live link resolves
# into the cargo release directory; the staged names are exactly the top-level files
# AgentRequestDeploy reads; no GNU-only readlink; the quiet-point restart hands the daemon a
# staged directory under ~/.rsi/staging. Port of the retired Python suite
# scripts/tests/test_install_release_stage.py (#366, no Python #358).
#
# Case map (legacy test -> case):
#   test_stage_dir_writes_the_set_and_touches_nothing_live
#       -> "stage-dir writes the set and touches nothing live"
#   test_stage_dir_prints_a_sha256_manifest_of_every_staged_file
#       -> "stage-dir prints a sha256 manifest of every staged file"
#   test_stage_dir_refuses_a_live_path               -> "stage-dir refuses a live path"
#   test_stage_dir_cannot_combine_with_tui_only      -> "stage-dir cannot combine with tui-only"
#   test_stage_dir_needs_a_directory                 -> "stage-dir needs a directory"
#   test_help_says_no_restart_is_not_staging         -> "help says no-restart is not staging"
#   test_no_restart_still_overwrites_the_live_install
#       -> "no-restart still overwrites the live install"
#   test_stage_dir_never_writes_through_a_symlink_inside_dir
#       -> "stage-dir never writes through a symlink inside DIR"
#   test_stage_dir_fills_an_empty_dir_as_one_fresh_tree
#       -> "stage-dir fills an empty dir as one fresh tree"
#   test_stage_build_refuses_a_live_link_into_the_build_output
#       -> "a staging build refuses a live link into the build output"
#   test_stage_build_runs_when_no_live_link_resolves_into_the_output
#       -> "a staging build runs when no live link resolves into the output"
#   test_staged_names_are_the_top_level_files_the_deploy_reads
#       -> "the staged names are the top-level files the deploy reads"
#   test_stage_dir_works_with_a_readlink_lacking_gnu_flags
#       -> "stage-dir works with a readlink lacking GNU flags"
#   test_quiet_restart_stages_into_the_daemon_allowed_root
#       -> "the quiet restart stages into the daemon's allowed root"
#   test_quiet_restart_replaces_a_stale_staging_dir_of_the_same_commit
#       -> "the quiet restart replaces a stale staging dir of the same commit"
#   test_now_restart_does_not_stage_for_the_daemon
#       -> "NOW=1 does not stage for the daemon"
# Case added with the port (#465, #454: the MANIFEST.sha256 writer is shell now):
#   "the manifest is exactly sha256 of each file": every line is a 64-hex digest equal to the
#       file's digest (the digest tool is pinned against the known sha256 of "hello\n"), and the
#       manifest does not list itself. The refusals above (live path, planted symlink, live link
#       into the build output) are the negative proofs: nothing is written or built after them.
#
# The fake daemon of the legacy suite (a Python socket and a Python rsi-rpc) is a Perl socket
# listener plus a POSIX-shell rsi-rpc that applies the same checks as deploy.rs stage_all.
source "$(dirname "$0")/lib/tap.sh"
source "$TAP_LIB_DIR/install-isolation.sh"

REPO="$REPO_ROOT"
AGENT_DEPLOY_RS="$REPO/crates/rsi-common/src/agent_deploy.rs"
DEPLOY_RS="$REPO/crates/rsid/src/deploy.rs"
# Members a cargo build produces; the rest of DEPLOY_BINARIES are repo scripts.
DEPLOY_SCRIPTS="rsid-supervisor.sh cargo-slot target-pool rsi-spill"
RELEASE_BINS="rsi rsid rsi-rpc rsi-agent-mcp rsi-build-rustc rsi-contract-validate rsi-known-failure rsi-turn-shim rsi-socket-hold"
QUIET_SHA="0123456789abcdef0123456789abcdef01234567"

if ! RSI_REPO_GUARD_BIN="$("$SCRIPTS_DIR/repo-guard" --bin)"; then
    echo "Bail out! scripts/repo-guard could not build rsi-repo-guard" >&2
    exit 1
fi
export RSI_REPO_GUARD_BIN

mode_of() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }
mtime_of() { stat -c %Y "$1" 2>/dev/null || stat -f %m "$1"; }
sha_of() { shasum -a 256 "$1" | awk '{print $1}'; }

# consumer_deploy_binaries: DEPLOY_BINARIES as rsi-common declares it, one name per line.
consumer_deploy_binaries() {
    awk '/pub const DEPLOY_BINARIES: \[&str; [0-9]+\] = \[/ { on = 1; next } on && /\];/ { exit } on { if (match($0, /"[^"]+"/)) print substr($0, RSTART + 1, RLENGTH - 2) }' "$AGENT_DEPLOY_RS"
}

# snapshot <path...>: every path under the roots with kind, mode, link target, content hash
# and mtime, one line each; a missing root is recorded as missing.
snapshot() {
    local root path
    for root in "$@"; do
        if [[ ! -e "$root" && ! -L "$root" ]]; then
            printf '%s\tmissing\n' "$root"
            continue
        fi
        find "$root" | LC_ALL=C sort | while IFS= read -r path; do
            if [[ -L "$path" ]]; then
                printf '%s\tlink\t%s\n' "$path" "$(readlink "$path")"
            elif [[ -f "$path" ]]; then
                printf '%s\tfile\t%s\t%s\t%s\n' "$path" "$(sha_of "$path")" "$(mtime_of "$path")" "$(mode_of "$path")"
            else
                printf '%s\tdir\t%s\n' "$path" "$(mode_of "$path")"
            fi
        done
    done
}

fixture() {
    BASE="$CASE_DIR/base"
    HOME_DIR="$BASE/home"
    RSI_HOME="$HOME_DIR/.rsi"
    BIN_DIR="$HOME_DIR/.local/bin"
    ROOT="$BASE/repo"
    TARGET="$BASE/target"
    FAKES="$BASE/fakes"
    CALLED="$BASE/called"
    CARGO_BUILDS="$BASE/cargo-builds"
    STAGE="$BASE/stage"
    mkdir -p "$BASE" "$ROOT" "$FAKES" "$HOME_DIR" "$CALLED" "$CARGO_BUILDS"
    # `cargo build` is recorded in CARGO_BUILDS (not in CALLED, which must stay empty).
    write_exec "$FAKES/cargo" "#!/bin/sh
[ \"\$1\" = build ] && touch \"$CARGO_BUILDS/build\"
printf '{\"target_directory\":\"%s\"}\\n' \"\$CARGO_TARGET_DIR\"
"
    write_exec "$FAKES/rustup" $'#!/bin/sh\nexit 99\n'
    local name
    install_isolation_fixture
    cp -R "$SCRIPTS_DIR" "$ROOT/scripts" || tap_fail "cannot copy scripts/"
    rm -rf "$ROOT/scripts/tests"
    printf '[workspace]\n[package]\nname = "stub"\nversion = "0.0.0"\nedition = "2021"\n' >"$ROOT/Cargo.toml"
    write_exec "$ROOT/src/lib.rs" ""
    for name in $RELEASE_BINS; do
        write_exec "$TARGET/release/$name" "#!/bin/sh
echo \"fake-$name \$*\"
"
    done
    printf 'worktree=%s\nstate=built\nhead=unknown\n' "$(cd -- "$ROOT" && pwd -P)" >"$TARGET/release/.rsi-release-source"
    # A pre-existing live install the staging run must leave alone.
    write_exec "$RSI_HOME/install/rsid" $'#!/bin/sh\necho live-rsid\n'
    write_exec "$RSI_HOME/install/rsid-supervisor.sh" $'#!/bin/sh\necho live-supervisor\n'
    write_exec "$RSI_HOME/bin/cargo-slot" $'#!/bin/sh\necho live-wrapper\n'
    write_exec "$RSI_HOME/bin/target-pool" $'#!/bin/sh\necho live-pool\n'
    write_exec "$RSI_HOME/lib/cargo-slot" $'#!/bin/sh\necho live-slot\n'
    mkdir -p "$BIN_DIR"
    ln -s "$RSI_HOME/install/rsid" "$BIN_DIR/rsid"
}

live_state() { snapshot "$RSI_HOME" "$BIN_DIR" "$ROOT/target" "$HOME_DIR/Library"; }

# run_script [args...]: the installer under the fixture; EXTRA_ENV words are added to the env.
run_script() {
    local -a INSTALL_ARGS=("$@")
    # shellcheck disable=SC2086
    run_capture install_isolated ${EXTRA_ENV:-}
    # Successful and refused staging paths must both return before any probe,
    # service-manager operation or signal. Check every existing staging case.
    local arg
    for arg in "$@"; do
        case "$arg" in --stage-dir|--stage-dir=*) assert_nothing_called ;; esac
    done
}

run_ok() {
    run_script "$@"
    assert_code 0 "install-release.sh $*"
}

assert_nothing_called() {
    assert_eq "" "$(ls -A "$CALLED")" "a restart or supervisor start was attempted"
}

case_stage_dir_writes_the_set_and_touches_nothing_live() {
    fixture
    local before name staged
    before="$(live_state)"
    run_ok --link-only --stage-dir "$STAGE"
    assert_eq "$before" "$(live_state)" "live state"
    assert_nothing_called
    for name in $RELEASE_BINS; do
        staged="$STAGE/$name"
        [[ -f "$staged" && ! -L "$staged" ]] || tap_fail "$name is not a regular staged file"
        [[ -x "$staged" ]] || tap_fail "$name is not executable"
    done
    for name in rsid-supervisor.sh cargo-slot target-pool rsi-spill; do
        assert_same_bytes "$STAGE/$name" "$ROOT/scripts/$name" "staged $name"
    done
    [[ -x "$STAGE/rsi-spill" ]] || tap_fail "rsi-spill is not executable"
    assert_not_contains "$RUN_OUT" "Linked:" "a staging run links nothing"
}

case_stage_dir_prints_a_sha256_manifest_of_every_staged_file() {
    fixture
    run_ok --link-only "--stage-dir=$STAGE"
    local staged rel written
    staged="$(cd "$STAGE" && find . -type f ! -name MANIFEST.sha256 | sed 's|^\./||' | LC_ALL=C sort)"
    assert_contains "$staged" rsid "staged names"
    assert_contains "$staged" rsid-supervisor.sh "staged names"
    while IFS= read -r rel; do
        assert_contains "$(printf '%s\n' "$RUN_OUT")" "$(sha_of "$STAGE/$rel")  $rel" "manifest line for $rel on stdout"
    done <<<"$staged"
    written="$(awk '{ sub(/^[0-9a-f]+  /, ""); print }' "$STAGE/MANIFEST.sha256")"
    assert_eq "$staged" "$written" "MANIFEST.sha256 names, in path order"
}

case_the_manifest_is_exactly_sha256_of_each_file() {
    fixture
    run_ok --link-only --stage-dir "$STAGE"
    # A known body: the digest of "hello\n" is fixed, so the writer is checked against a
    # constant and not only against the same tool.
    printf 'hello\n' >"$CASE_DIR/known"
    assert_eq "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03" "$(sha_of "$CASE_DIR/known")" "the reference digest"
    local line file digest
    while IFS= read -r line; do
        digest="${line%%  *}"
        file="${line#*  }"
        [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || tap_fail "not a sha256 digest: [$line]"
        assert_eq "$digest" "$(sha_of "$STAGE/$file")" "digest of $file"
    done <"$STAGE/MANIFEST.sha256"
    assert_not_contains "$(cat "$STAGE/MANIFEST.sha256")" "MANIFEST.sha256" "the manifest lists itself"
}

case_stage_dir_refuses_a_live_path() {
    fixture
    local before live
    before="$(live_state)"
    for live in "$RSI_HOME/install" "$RSI_HOME/bin" "$RSI_HOME/lib" "$BIN_DIR" "$RSI_HOME" "$RSI_HOME/install/nested"; do
        run_script --link-only --stage-dir "$live"
        assert_ne 0 "$RUN_CODE" "$live was accepted as a staging directory"
        assert_contains "$RUN_ERR" "overlaps the live path" "refusal for $live"
    done
    assert_eq "$before" "$(live_state)" "live state"
}

case_stage_dir_cannot_combine_with_tui_only() {
    fixture
    run_script --tui-only --stage-dir "$STAGE"
    assert_ne 0 "$RUN_CODE" "the combination was accepted"
    assert_contains "$RUN_ERR" "--tui-only" "the refusal names the flag"
    assert_no_file "$STAGE" "nothing is staged"
}

case_stage_dir_needs_a_directory() {
    fixture
    run_script --stage-dir
    assert_ne 0 "$RUN_CODE" "a bare --stage-dir was accepted"
    assert_contains "$RUN_ERR" "--stage-dir needs a directory" "the refusal"
}

case_help_says_no_restart_is_not_staging() {
    fixture
    run_ok --help
    assert_contains "$RUN_OUT" "--stage-dir DIR" "help lists --stage-dir"
    local no_restart
    no_restart="$(printf '%s\n' "$RUN_OUT" | awk '/^  --no-restart / { on = 1 } /^  --link-only / { on = 0 } on')"
    assert_contains "$no_restart" "not staging" "the --no-restart text"
    assert_contains "$no_restart" "overwritten" "the --no-restart text"
}

case_no_restart_still_overwrites_the_live_install() {
    fixture
    # The documented meaning of --no-restart: install, do not restart.
    run_ok --link-only --no-restart
    assert_contains "$(cat "$RSI_HOME/install/rsid")" fake-rsid "the new rsid"
    assert_contains "$(cat "$HOME_DIR/Library/LaunchAgents/dev.rsi.rsid.plist")" "$RSI_HOME/install/rsi-socket-hold" "the plist names this fake HOME's install"
    assert_nothing_called
}

# --- #262 / hub #2800: never write through a path already inside DIR -----

case_stage_dir_never_writes_through_a_symlink_inside_dir() {
    fixture
    mkdir -p "$STAGE"
    ln -s "$RSI_HOME/bin" "$STAGE/tools"
    ln -s "$RSI_HOME/lib/cargo-slot" "$STAGE/MANIFEST.sha256"
    local before planted
    before="$(live_state)"
    planted="$(snapshot "$STAGE")"
    run_script --link-only --stage-dir "$STAGE"
    assert_ne 0 "$RUN_CODE" "a non-empty staging dir was accepted"
    assert_contains "$RUN_ERR" "not an empty directory" "the refusal"
    assert_eq "$before" "$(live_state)" "live state"
    assert_eq "$planted" "$(snapshot "$STAGE")" "the planted tree"
}

case_stage_dir_fills_an_empty_dir_as_one_fresh_tree() {
    fixture
    mkdir -p "$STAGE"
    run_ok --link-only --stage-dir "$STAGE"
    [[ -f "$STAGE/rsid" ]] || tap_fail "rsid was not staged"
    [[ -f "$STAGE/MANIFEST.sha256" ]] || tap_fail "no manifest"
    # The private tree was renamed into place: the parent holds DIR alone.
    assert_eq "" "$(ls -A "$BASE" | grep '^\.' || true)" "hidden leftovers beside DIR"
    assert_eq 755 "$(mode_of "$STAGE")" "DIR mode"
}

# --- #262 / hub #2801: a staging build never changes a live link's file --

case_a_staging_build_refuses_a_live_link_into_the_build_output() {
    fixture
    ln -s "$TARGET/release/rsi" "$BIN_DIR/rsi"
    local before release_before
    before="$(live_state)"
    release_before="$(snapshot "$TARGET/release")"
    run_script --stage-dir "$STAGE"
    assert_ne 0 "$RUN_CODE" "the staging build ran over a live link"
    assert_contains "$RUN_ERR" "resolve into the build output" "the refusal"
    assert_contains "$RUN_ERR" "$BIN_DIR/rsi" "the refusal names the link"
    assert_eq "" "$(ls -A "$CARGO_BUILDS")" "cargo ran"
    assert_eq "$release_before" "$(snapshot "$TARGET/release")" "the build output"
    [[ -x "$TARGET/release/rsi" ]] || tap_fail "the linked binary is not executable any more"
    assert_eq "$before" "$(live_state)" "live state"
    assert_no_file "$STAGE" "nothing is staged"
}

case_a_staging_build_runs_when_no_live_link_resolves_into_the_output() {
    fixture
    local before
    before="$(live_state)"
    run_ok --stage-dir "$STAGE"
    assert_eq "build" "$(ls -A "$CARGO_BUILDS")" "the cargo build"
    [[ -f "$STAGE/rsid" ]] || tap_fail "rsid was not staged"
    assert_eq "$before" "$(live_state)" "live state"
    assert_nothing_called
}

# --- #262 / hub #2799: producer (stage) matches consumer (deploy) --------

case_the_staged_names_are_the_top_level_files_the_deploy_reads() {
    fixture
    local expected name staged
    expected="$(consumer_deploy_binaries)"
    assert_contains "$expected" cargo-slot "DEPLOY_BINARIES"
    assert_contains "$expected" target-pool "DEPLOY_BINARIES"
    assert_contains "$expected" rsi-spill "DEPLOY_BINARIES"
    # deploy.rs stage_all reads each member as `dir.join(name)`, top level.
    local deploy
    deploy="$(cat "$DEPLOY_RS")"
    assert_contains "$deploy" "for name in DEPLOY_BINARIES" "deploy.rs"
    assert_contains "$deploy" "let source = dir.join(name);" "deploy.rs"
    for name in $expected; do
        case " $DEPLOY_SCRIPTS " in
            *" $name "*) ;;
            *) write_exec "$TARGET/release/$name" "#!/bin/sh
echo fake-$name
" ;;
        esac
    done
    run_ok --link-only --stage-dir "$STAGE"
    assert_eq "$(printf '%s\nMANIFEST.sha256\n' "$expected" | LC_ALL=C sort)" "$(ls -A "$STAGE" | LC_ALL=C sort)" "the staged names"
    for name in $expected; do
        staged="$STAGE/$name"
        # The consumer requires a regular file (symlink_metadata is_file).
        [[ -f "$staged" && ! -L "$staged" ]] || tap_fail "$name is not a regular staged file"
        [[ -x "$staged" ]] || tap_fail "$name is not executable"
    done
    # cargo-slot is the full implementation, never the ~/.rsi/bin wrapper.
    assert_same_bytes "$STAGE/cargo-slot" "$ROOT/scripts/cargo-slot" "staged cargo-slot"
    assert_same_bytes "$STAGE/target-pool" "$ROOT/scripts/target-pool" "staged target-pool"
    assert_same_bytes "$STAGE/rsi-spill" "$ROOT/scripts/rsi-spill" "staged rsi-spill"
    [[ -x "$STAGE/rsi-spill" ]] || tap_fail "rsi-spill is not executable"
}

# --- #262 / hub #2802: no GNU-only readlink ------------------------------

case_stage_dir_works_with_a_readlink_lacking_gnu_flags() {
    fixture
    local real
    real="$(command -v readlink || echo /usr/bin/readlink)"
    write_exec "$FAKES/readlink" "#!/bin/sh
for a in \"\$@\"; do case \"\$a\" in -m|-e|-f|--canonicalize*) echo \"readlink: illegal option -- \$a\" >&2; exit 1;; esac; done
exec \"$real\" \"\$@\"
"
    # Overlap detection still resolves a symlinked path to the live one.
    ln -s "$RSI_HOME" "$BASE/alias-of-rsi-home"
    local before
    before="$(live_state)"
    run_script --link-only --stage-dir "$BASE/alias-of-rsi-home/bin/new"
    assert_ne 0 "$RUN_CODE" "a path through a symlink to the live dir was accepted"
    assert_contains "$RUN_ERR" "overlaps the live path" "the refusal"
    assert_eq "$before" "$(live_state)" "live state"
    run_ok --link-only --stage-dir "$STAGE"
    assert_not_contains "$RUN_ERR" "illegal option" "a GNU-only readlink flag was used"
    [[ -f "$STAGE/rsid" ]] || tap_fail "rsid was not staged"
    assert_eq "$before" "$(live_state)" "live state"
}

# --- #3423: the quiet-point restart hands the daemon a staged directory ---

# fake_daemon: sets SOCKET_PATH (a live Unix socket) and writes an rsi-rpc that answers like
# the daemon: it accepts `binaries_dir` only under ~/.rsi/staging (its allowed root) and checks
# what deploy.rs stage_all checks: MANIFEST present, rsid embeds a sha, every DEPLOY_BINARIES
# member at the top level.
fake_daemon() {
    command -v perl >/dev/null 2>&1 || tap_fail "perl is needed for the fake daemon socket"
    SOCK_DIR="$(mktemp -d /tmp/rqs.XXXXXX)" || tap_fail "no socket directory"
    SOCKET_PATH="$SOCK_DIR/d.sock"
    perl -MIO::Socket::UNIX -e '$s = IO::Socket::UNIX->new(Local => shift, Listen => 8) or die "bind: $!"; sleep 1 while 1' "$SOCKET_PATH" >/dev/null 2>&1 &
    LISTENER_PID=$!
    # shellcheck disable=SC2064
    trap 'kill "$LISTENER_PID" 2>/dev/null; wait "$LISTENER_PID" 2>/dev/null; rm -rf "$SOCK_DIR"' EXIT
    local tries=0
    until [[ -S "$SOCKET_PATH" ]]; do
        tries=$((tries + 1))
        [[ "$tries" -le 100 ]] || tap_fail "the fake daemon socket never appeared"
        sleep 0.05
    done
    RPC_LOG="$BASE/rpc-params.json"
    local name needs=""
    for name in $(consumer_deploy_binaries); do
        needs="$needs $name"
        case " $DEPLOY_SCRIPTS " in
            *" $name "*) ;;
            *)
                # rsi-rpc itself is the fake daemon front below.
                [[ "$name" == rsi-rpc ]] || write_exec "$TARGET/release/$name" "#!/bin/sh
[ \"\$1\" = --build-info ] && echo \"$QUIET_SHA 1\"
echo \"fake-$name \$*\"
"
                ;;
        esac
    done
    write_exec "$TARGET/release/rsi-rpc" "#!/bin/sh
params=
while [ \$# -gt 0 ]; do [ \"\$1\" = --params ] && params=\$2; shift; done
printf '%s' \"\$params\" > '$RPC_LOG'
fail() { printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"message\":\"Invalid parameter: %s\"}}\\n' \"\$1\"; exit 0; }
dir=\$(printf '%s' \"\$params\" | sed -n 's/.*\"binaries_dir\": *\"\\([^\"]*\\)\".*/\\1/p')
root=\$(cd \"\$HOME/.rsi/staging\" 2>/dev/null && pwd -P)
d=\$(cd \"\$dir\" 2>/dev/null && pwd -P)
case \"\$d\" in \"\$root\"/*) [ -n \"\$root\" ] || fail deploy_directory_not_allowed ;; *) fail deploy_directory_not_allowed ;; esac
for n in MANIFEST.sha256$needs; do
  [ -f \"\$d/\$n\" ] || fail \"deploy_binary_missing \$n\"
done
sha=\$(\"\$d/rsid\" --build-info | awk 'NR == 1 {print \$1}')
[ \"\$sha\" = '$QUIET_SHA' ] || fail deploy_sha_mismatch
printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"sha\":\"%s\",\"release_by\":\"soon\"}}\\n' \"\$sha\"
"
}

quiet_install() {
    EXTRA_ENV="RSI_DAEMON_SOCKET_PATH=$SOCKET_PATH RSI_BUILD_SHA=$QUIET_SHA NOW=0" run_script
}

case_the_quiet_restart_stages_into_the_daemons_allowed_root() {
    fixture
    fake_daemon
    # The checkout's target directory is outside every root the daemon allows; the quiet
    # restart must go through staging instead.
    quiet_install
    assert_code 0 "the quiet install"
    local staged="$RSI_HOME/staging/bin-${QUIET_SHA:0:9}" sent name
    assert_contains "$RUN_OUT" "Restart pending" "the quiet restart was not requested"
    sent="$(sed -n 's/.*"binaries_dir": *"\([^"]*\)".*/\1/p' "$RPC_LOG")"
    assert_eq "$(cd "$staged" && pwd -P)" "$(cd "$sent" && pwd -P)" "the directory sent to the daemon"
    assert_ne "$(cd "$TARGET/release" && pwd -P)" "$(cd "$sent" && pwd -P)" "the build output was sent"
    # Same checks as an agent deploy: MANIFEST, every DEPLOY_BINARIES member at the top level
    # as a regular executable, rsid embeds the commit.
    [[ -f "$staged/MANIFEST.sha256" ]] || tap_fail "no manifest in the staged directory"
    for name in $(consumer_deploy_binaries); do
        [[ -f "$staged/$name" && ! -L "$staged/$name" ]] || tap_fail "$name is not a regular staged file"
        [[ -x "$staged/$name" ]] || tap_fail "$name is not executable"
    done
    assert_eq "$QUIET_SHA" "$("$staged/rsid" --build-info | awk 'NR == 1 {print $1}')" "the commit rsid embeds"
}

case_the_quiet_restart_replaces_a_stale_staging_dir_of_the_same_commit() {
    fixture
    fake_daemon
    local stale="$RSI_HOME/staging/bin-${QUIET_SHA:0:9}"
    write_exec "$stale/stale-leftover" $'#!/bin/sh\n'
    quiet_install
    assert_code 0 "the quiet install"
    assert_no_file "$stale/stale-leftover" "the stale file survived"
    [[ -f "$stale/MANIFEST.sha256" ]] || tap_fail "no manifest in the replaced directory"
}

case_now_does_not_stage_for_the_daemon() {
    fixture
    fake_daemon
    # NOW=1 behaves as before: no quiet request, nothing under staging.
    EXTRA_ENV="RSI_DAEMON_SOCKET_PATH=$SOCKET_PATH" run_script --link-only --now
    assert_code 1 "the fake launchctl refuses bootstrap"
    assert_contains "$(cat "$CALLED/launchctl")" "bootstrap gui/424242 $HOME_DIR/Library/LaunchAgents/dev.rsi.rsid.plist" "only the fixture domain is loaded"
    assert_no_file "$RSI_HOME/staging" "staging happened under --now: $RUN_OUT $RUN_ERR"
    assert_no_file "$RPC_LOG" "the daemon was asked under --now"
}

case_missing_lifecycle_stub_refuses_before_install() {
    fixture
    must rm "$FAKES/launchctl"
    run_script --link-only --now
    assert_code 97 "missing launchctl stub"
    assert_contains "$RUN_ERR" "missing fixture launchctl" "the isolation refusal"
    assert_nothing_called
    assert_no_file "$HOME_DIR/Library/LaunchAgents/dev.rsi.rsid.plist" "installer never entered"
}

case_signals_and_process_probes_are_fixture_commands() {
    fixture
    # Exercise the actual Bash startup boundary, including `command kill`.
    write_exec "$ROOT/scripts/install-release.sh" '#!/usr/bin/env bash
kill -INT 424243
command kill -TERM 424243
pgrep -x rsid
ps -p 424243
lsof -p 424243
exit 23
'
    run_script
    assert_code 23 "the boundary script's exit is preserved"
    assert_file_is "$CALLED/kill" $'-INT 424243\n-TERM 424243\n' "both signals were recorded by the fake"
    assert_file_is "$CALLED/pgrep" $'-x rsid\n'
    assert_file_is "$CALLED/ps" $'-p 424243\n'
    assert_file_is "$CALLED/lsof" $'-p 424243\n'
}

case_fake_launchctl_refuses_a_host_domain() {
    fixture
    write_exec "$ROOT/scripts/install-release.sh" '#!/usr/bin/env bash
launchctl bootstrap gui/501 "$HOME/Library/LaunchAgents/dev.rsi.rsid.plist"
'
    run_script
    assert_code 98 "a non-fixture domain is refused"
    assert_contains "$RUN_ERR" "outside fixture launchd domain" "the domain refusal"
}

case_linux_staging_and_restart_use_only_fixture_controls() {
    fixture
    local INSTALL_TEST_OS=Linux
    run_ok --link-only --stage-dir "$STAGE"
    assert_nothing_called
    run_ok --link-only --no-restart
    assert_nothing_called
    run_script --link-only --now
    assert_code 1 "the fake systemctl refuses provisioning"
    assert_file "$CALLED/systemctl" "only fixture systemd was reached"
    assert_no_file "$CALLED/launchctl" "Linux uses its own lifecycle branch"
}

run_case "stage-dir writes the set and touches nothing live" case_stage_dir_writes_the_set_and_touches_nothing_live
run_case "stage-dir prints a sha256 manifest of every staged file" case_stage_dir_prints_a_sha256_manifest_of_every_staged_file
run_case "the manifest is exactly sha256 of each file" case_the_manifest_is_exactly_sha256_of_each_file
run_case "stage-dir refuses a live path" case_stage_dir_refuses_a_live_path
run_case "stage-dir cannot combine with tui-only" case_stage_dir_cannot_combine_with_tui_only
run_case "stage-dir needs a directory" case_stage_dir_needs_a_directory
run_case "help says no-restart is not staging" case_help_says_no_restart_is_not_staging
run_case "no-restart still overwrites the live install" case_no_restart_still_overwrites_the_live_install
run_case "stage-dir never writes through a symlink inside DIR" case_stage_dir_never_writes_through_a_symlink_inside_dir
run_case "stage-dir fills an empty dir as one fresh tree" case_stage_dir_fills_an_empty_dir_as_one_fresh_tree
run_case "a staging build refuses a live link into the build output" case_a_staging_build_refuses_a_live_link_into_the_build_output
run_case "a staging build runs when no live link resolves into the output" case_a_staging_build_runs_when_no_live_link_resolves_into_the_output
run_case "the staged names are the top-level files the deploy reads" case_the_staged_names_are_the_top_level_files_the_deploy_reads
run_case "stage-dir works with a readlink lacking GNU flags" case_stage_dir_works_with_a_readlink_lacking_gnu_flags
run_case "the quiet restart stages into the daemon's allowed root" case_the_quiet_restart_stages_into_the_daemons_allowed_root
run_case "the quiet restart replaces a stale staging dir of the same commit" case_the_quiet_restart_replaces_a_stale_staging_dir_of_the_same_commit
run_case "NOW=1 does not stage for the daemon" case_now_does_not_stage_for_the_daemon
run_case "a missing lifecycle stub refuses before install" case_missing_lifecycle_stub_refuses_before_install
run_case "signals and process probes are fixture commands" case_signals_and_process_probes_are_fixture_commands
run_case "fake launchctl refuses a host domain" case_fake_launchctl_refuses_a_host_domain
run_case "Linux staging and restart use only fixture controls" case_linux_staging_and_restart_use_only_fixture_controls
tap_done
