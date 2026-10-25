#!/usr/bin/env bash
# suite-gate: tap
# scripts/run-rsid-test-shards.sh: the artifact locks, the cleanup contract and the planned commands,
# against deterministic Cargo and guard stand-ins. Port of the Python suite of the same name (#366).
#
# Case map (legacy scripts/tests/test_run_rsid_test_shards.py ArtifactLockTests -> here):
#   test_peer_binary_survives_cleanup_during_build_and_execution (list and run phases)
#                                                  -> a peer binary survives cleanup during build and execution   [flock]
#   test_new_build_waits_until_exclusive_cleanup_finishes -> a new build waits until exclusive cleanup finishes  [flock]
#   test_alone_run_cleans_environment_selected_target -> an alone run cleans the environment-selected target      [flock]
#   test_shards_run_with_only_build_and_lander_rsi_variables -> shards run with only build and lander RSI variables [flock]
#   test_keep_artifacts_retains_binary              -> keep artifacts retains the binary                          [flock]
#   test_lander_environment_keeps_the_binary_it_prebuilt -> the lander environment keeps the binary it prebuilt   [flock]
#   test_keep_run_protects_its_binary_from_an_active_peer -> a keep run protects its binary from an active peer   [flock]
#   test_fast_mode_cleans_between_shards_and_at_exit -> fast mode cleans between shards and at exit               [flock]
#   test_multi_shard_run_restores_shared_lock_after_skipped_clean -> a multi-shard run restores the shared lock   [flock]
#   test_failed_tests_clean_and_preserve_exit_status -> failed tests clean and preserve the exit status           [flock]
#   test_dry_run_does_not_invoke_cargo_or_create_lock -> a dry run does not invoke cargo or create the lock       (every host)
#   test_a_shard_selects_every_package_that_declares_it -> a shard selects every package that declares it         (every host)
#   test_the_remote_read_leaf_library_runs_in_fast_mode_and_in_the_full_workspace_lane
#                                                  -> the remote-read leaf is planned in fast mode (every host) and a
#                                                     real fast run executes it                                   [flock]
#   test_the_runtime_listing_ignores_the_default_filter_and_a_run_does_only_on_the_slow_tier -> same name         (every host)
#   test_a_filterset_atom_of_every_shape_is_forwarded_as_one_quoted_argument -> same name (six shapes)            (every host)
#   test_a_malformed_or_control_character_filterset_is_refused -> same name (nine inputs)                         (every host)
# Divergence from the legacy fixture, on purpose: the subject now reads the shard inventory from the native
# `scripts/repo-guard rsid-test-shards` (#362, #448), no longer a Python checker, so the stand-in is a shell
# `scripts/repo-guard` (the lists and `--target-directory` the runner asks for) and the Cargo stand-in is shell.
# The `[flock]` cases need util-linux flock: on a host without it they are listed as Linux-only SKIPs (hub
# #3621 checklist); the dry-run cases run everywhere, so the suite is never all-skipped.
# Negative coverage: rsi-repo-guards `ported_suites_tests` neuters the runner and mutates its filterset and
# tier handling in a scratch copy and requires this suite red.
source "$(dirname "$0")/lib/tap.sh"

RUNNER="$SCRIPTS_DIR/run-rsid-test-shards.sh"
SHARD_COUNT=16

# The merge-queue lander runs this suite with its own RSI_LANDER_* settings; cases opt in explicitly.
while IFS= read -r variable; do
    unset "$variable"
done < <(compgen -e | grep '^RSI_LANDER_' || true)

# The Cargo stand-in: records what it is asked, holds on control files, never builds anything.
FAKE_CARGO='#!/usr/bin/env bash
set -u
target="${CARGO_TARGET_DIR:-$FAKE_TARGET_DIRECTORY}"
control="$FAKE_CONTROL_DIRECTORY"
name="$FAKE_RUN_NAME"
wait_for() {
    local tries=0
    while [[ ! -e "$1" ]]; do
        tries=$((tries + 1))
        [[ "$tries" -le 1500 ]] || { echo "timed out waiting for $1" >&2; exit 99; }
        sleep 0.01
    done
}
case "$1" in
    metadata)
        : >"$control/metadata-$name"
        printf "{\"target_directory\": \"%s\"}\n" "$target"
        ;;
    nextest)
        case "$2" in
            list)
                mkdir -p "$target/debug/deps"
                printf "#!/bin/sh\nexit 0\n" >"$target/debug/deps/rsid-$name"
                chmod 755 "$target/debug/deps/rsid-$name"
                : >"$control/listed-$name"
                if [[ -n "${FAKE_HOLD_LIST:-}" ]]; then wait_for "$control/release-$name"; fi
                printf "{\"binary\": \"%s\"}\n" "$target/debug/deps/rsid-$name"
                ;;
            run)
                # Cargo rebuilds a harness after an earlier shard clean in fast/full mode.
                mkdir -p "$target/debug/deps"
                if [[ ! -e "$target/debug/deps/rsid-$name" ]]; then
                    printf "#!/bin/sh\nexit 0\n" >"$target/debug/deps/rsid-$name"
                    chmod 755 "$target/debug/deps/rsid-$name"
                fi
                : >"$control/running-$name"
                env | cut -d= -f1 | grep "^RSI_" | sort >"$control/env-$name"
                if [[ -n "${FAKE_HOLD_RUN:-}" ]]; then wait_for "$control/release-$name"; fi
                # Exec the feature-hashed harness after the peer has attempted cleanup.
                "$target/debug/deps/rsid-$name"
                status=$?
                if [[ "$status" -ne 0 ]]; then exit "$status"; fi
                exit "${FAKE_TEST_STATUS:-0}"
                ;;
            *) echo "unexpected Cargo args: $*" >&2; exit 1 ;;
        esac
        ;;
    clean)
        : >"$control/cleaning-$name"
        echo clean >>"$control/cleans-$name"
        if [[ -n "${FAKE_HOLD_CLEAN:-}" ]]; then wait_for "$control/release-clean-$name"; fi
        rm -f "$target"/debug/deps/rsid-*
        echo "Removed fake rsid artifacts"
        ;;
    test)
        case " $* " in *" --doc "*) ;; *) echo "unexpected Cargo args: $*" >&2; exit 1 ;; esac
        ;;
    *) echo "unexpected Cargo args: $*" >&2; exit 1 ;;
esac
'

# The shard guard stand-in: the inventory the runner asks `scripts/repo-guard rsid-test-shards` for.
FAKE_GUARD='#!/usr/bin/env bash
[[ "${1:-}" == rsid-test-shards ]] || { echo "unexpected guard: $*" >&2; exit 64; }
shift
list_packages=0 list_shards=0 target_directory=0
for argument in "$@"; do
    case "$argument" in
        --list-shard-packages) list_packages=1 ;;
        --list-shards) list_shards=1 ;;
        --target-directory) target_directory=1 ;;
    esac
done
i=1
if [[ "$target_directory" == 1 ]]; then
    sed -n "s/.*\"target_directory\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" | head -n 1
elif [[ "$list_packages" == 1 ]]; then
    while [[ "$i" -le 16 ]]; do printf "store-%02d rsid rsid-store\n" "$i"; i=$((i + 1)); done
elif [[ "$list_shards" == 1 ]]; then
    while [[ "$i" -le 16 ]]; do printf "store-%02d\n" "$i"; i=$((i + 1)); done
fi
exit 0
'

have_flock() { command -v flock >/dev/null 2>&1; }

setup() {
    TARGET="$CASE_DIR/shared target"
    CONTROL="$CASE_DIR/control"
    BIN="$CASE_DIR/bin"
    mkdir -p "$CONTROL" "$BIN"
    write_exec "$BIN/cargo" "$FAKE_CARGO"
    EXTRA_ENV=()
    trap 'stop_runners' EXIT
}

# Stop only the runners this case started.
stop_runners() {
    local pid_file pid
    for pid_file in "$CASE_DIR"/pid-*; do
        [[ -f "$pid_file" ]] || continue
        pid="$(cat "$pid_file")"
        kill "$pid" 2>/dev/null || true
    done
}

# launch <name> <mode> [runner args...]: a runner in its own scratch worktree sharing $TARGET.
# EXTRA_ENV holds extra NAME=value assignments. Its output goes to $CASE_DIR/out-<name>.
launch() {
    local name="$1" mode="$2" repo
    shift 2
    repo="$CASE_DIR/$name"
    mkdir -p "$repo/scripts" "$repo/crates/rsid/tests"
    cp "$RUNNER" "$repo/scripts/run-rsid-test-shards.sh"
    write_exec "$repo/scripts/repo-guard" "$FAKE_GUARD"
    : >"$repo/crates/rsid/tests/fixture.rs"
    local args=("$mode")
    [[ "$mode" == shard ]] && args+=(store-01)
    args+=(--jobs 4)
    env -u CARGO_TARGET_DIR PATH="$BIN:$PATH" FAKE_TARGET_DIRECTORY="$TARGET" \
        FAKE_CONTROL_DIRECTORY="$CONTROL" FAKE_RUN_NAME="$name" \
        ${EXTRA_ENV[@]+"${EXTRA_ENV[@]}"} \
        bash "$repo/scripts/run-rsid-test-shards.sh" "${args[@]}" "$@" \
        </dev/null >"$CASE_DIR/out-$name" 2>&1 &
    echo "$!" >"$CASE_DIR/pid-$name"
}

# finish <name> [expected status]: wait for it; its whole output is left in FINISH_OUT (bytes kept).
finish() {
    local name="$1" want="${2:-0}" pid rc=0
    pid="$(cat "$CASE_DIR/pid-$name")"
    wait "$pid" || rc=$?
    read_exact FINISH_OUT "$CASE_DIR/out-$name"
    [[ "$rc" == "$want" ]] || tap_fail "$name exited $rc, expected $want: $FINISH_OUT"
}

# wait_for <control file>: up to ten seconds.
wait_for() {
    local tries=0
    while [[ ! -e "$CONTROL/$1" ]]; do
        tries=$((tries + 1))
        [[ "$tries" -le 1000 ]] || tap_fail "timed out waiting for $1"
        sleep 0.01
    done
}

running() { kill -0 "$(cat "$CASE_DIR/pid-$1")" 2>/dev/null; }

no_rsid_artifacts() { # the deps directory exists and holds no rsid-* harness
    local found
    found="$(find_named "$TARGET/debug/deps" 'rsid-*')"
    [[ -z "$found" ]] || tap_fail "rsid artifacts remain: $found"
}

# --- flock cases (the artifact locks) ---

case_a_peer_binary_survives_cleanup_during_build_and_execution() {
    setup
    local phase marker binary
    for phase in list run; do
        marker=running
        [[ "$phase" == list ]] && marker=listed
        if [[ "$phase" == list ]]; then EXTRA_ENV=(FAKE_HOLD_LIST=1); else EXTRA_ENV=(FAKE_HOLD_RUN=1); fi
        launch "peer-$phase" shard
        wait_for "$marker-peer-$phase"
        EXTRA_ENV=()
        launch "first-$phase" shard
        finish "first-$phase"
        assert_contains "$FINISH_OUT" "skipping clean-store-01: another rsid shard runner" "$phase: the peer's build is protected"
        binary="$TARGET/debug/deps/rsid-peer-$phase"
        assert_file "$binary" "the peer's binary survived ($phase)"
        "$binary" || tap_fail "the peer's binary does not run ($phase)"
        : >"$CONTROL/release-peer-$phase"
        finish "peer-$phase"
        assert_contains "$FINISH_OUT" "running clean-store-01" "$phase: the last runner cleans"
        no_rsid_artifacts
    done
}

case_a_new_build_waits_until_exclusive_cleanup_finishes() {
    setup
    EXTRA_ENV=(FAKE_HOLD_CLEAN=1)
    launch first shard
    wait_for cleaning-first
    EXTRA_ENV=()
    launch second shard
    wait_for metadata-second
    # Its list must wait until the first runner releases exclusive.
    sleep 0.2
    running second || tap_fail "the second runner finished while the exclusive clean was held"
    assert_no_file "$CONTROL/listed-second" "the second runner listed during the exclusive clean"
    : >"$CONTROL/release-clean-first"
    finish first
    finish second
}

case_an_alone_run_cleans_the_environment_selected_target() {
    setup
    EXTRA_ENV=(CARGO_TARGET_DIR="$TARGET")
    # `env -u CARGO_TARGET_DIR` in launch strips the inherited one; the assignment re-adds it.
    launch alone shard
    finish alone
    assert_contains "$FINISH_OUT" "running clean-store-01"
    assert_file "$CONTROL/cleaning-alone" "the alone run cleaned"
    no_rsid_artifacts
}

case_shards_run_with_only_build_and_lander_rsi_variables() {
    setup
    # #1163: a worker shell inherits the daemon's RSI_* configuration and the merge-queue lander does
    # not; tests must see the same environment in both.
    EXTRA_ENV=(RSI_CONTEXT_ROTATION_ENABLED=true "RSI_SANDBOX_BASE=$CASE_DIR/daemon-sandboxes"
        RSI_SESSION_ID=00000000-0000-0000-0000-000000000000 RSI_BUILD_SLOTS=2 RSI_JOB_CARGO_SLOT=1 RSI_LANDER_PREBUILD=1)
    launch environment shard
    finish environment
    local seen expected
    seen="$(sort -u "$CONTROL/env-environment")" || tap_fail "no environment record"
    expected="$({ env | cut -d= -f1; printf '%s\n' RSI_CONTEXT_ROTATION_ENABLED RSI_SANDBOX_BASE RSI_SESSION_ID RSI_BUILD_SLOTS RSI_JOB_CARGO_SLOT RSI_LANDER_PREBUILD; } \
        | grep -E '^RSI_(BUILD|JOB|LANDER)_' | sort -u)"
    assert_ne "" "$expected" "the expected plumbing variables"
    assert_eq "$expected" "$seen" "the RSI_* variables the shard saw"
}

case_keep_artifacts_retains_the_binary() {
    setup
    launch keep shard --keep-rsid-artifacts
    finish keep
    assert_file "$TARGET/debug/deps/rsid-keep" "the kept binary"
}

case_the_lander_environment_keeps_the_binary_it_prebuilt() {
    setup
    # #1244: the lander's target is private and discarded after the gate; a clean there only forced
    # the next shard to rebuild what it prebuilt.
    EXTRA_ENV=(RSI_LANDER_KEEP_RSID_ARTIFACTS=1)
    launch lander shard
    finish lander
    assert_file "$TARGET/debug/deps/rsid-lander" "the lander's prebuilt binary"
    assert_no_file "$CONTROL/cleaning-lander" "a clean ran in the lander environment: $FINISH_OUT"
}

case_a_keep_run_protects_its_binary_from_an_active_peer() {
    setup
    EXTRA_ENV=(FAKE_HOLD_RUN=1)
    launch kept shard --keep-rsid-artifacts
    wait_for running-kept
    EXTRA_ENV=()
    launch peer shard
    finish peer
    assert_contains "$FINISH_OUT" "skipping clean-store-01"
    : >"$CONTROL/release-kept"
    finish kept
    assert_file "$TARGET/debug/deps/rsid-kept" "the kept run's binary"
}

case_fast_mode_cleans_between_shards_and_at_exit() {
    setup
    launch fast fast
    finish fast
    assert_contains "$FINISH_OUT" "running clean-store-01"
    assert_contains "$FINISH_OUT" "running clean-final"
    local expected="" i
    for ((i = 0; i < SHARD_COUNT + 1; i++)); do expected="${expected}clean"$'\n'; done
    assert_file_is "$CONTROL/cleans-fast" "$expected" "one clean per shard and a final one (17)"
    no_rsid_artifacts
}

case_a_multi_shard_run_restores_the_shared_lock_after_a_skipped_clean() {
    setup
    EXTRA_ENV=(FAKE_HOLD_RUN=1)
    launch held shard
    wait_for running-held
    EXTRA_ENV=()
    launch fast fast
    finish fast
    assert_contains "$FINISH_OUT" "skipping clean-store-01"
    assert_contains "$FINISH_OUT" "skipping clean-final"
    assert_file "$TARGET/debug/deps/rsid-held" "the held peer's binary"
    : >"$CONTROL/release-held"
    finish held
    no_rsid_artifacts
}

case_failed_tests_clean_and_preserve_the_exit_status() {
    setup
    EXTRA_ENV=(FAKE_TEST_STATUS=42)
    launch failed shard
    finish failed 42
    assert_file "$CONTROL/cleaning-failed" "the failed run still cleaned"
    no_rsid_artifacts
}

case_a_real_fast_run_executes_the_remote_read_leaf() {
    setup
    launch leaf-ran fast
    finish leaf-ran
    assert_contains "$FINISH_OUT" "running remote-read-leaf"
}

# --- dry-run cases (the planned commands; every host) ---

case_a_dry_run_does_not_invoke_cargo_or_create_the_lock() {
    setup
    launch dry shard --dry-run
    finish dry
    assert_contains "$FINISH_OUT" "static plan:"
    assert_dir_empty "$CONTROL" "the control directory (cargo was invoked)"
    assert_no_file "$TARGET" "the shared target (a lock was created)"
}

case_a_shard_selects_every_package_that_declares_it() {
    setup
    # #1021 S4: the store tests live in rsid-store, the rest in rsid; one shard run lists, runs and
    # cleans both.
    launch pair shard --dry-run
    finish pair
    assert_contains "$FINISH_OUT" "-p rsid -p rsid-store --lib --no-default-features --features test-shard-store-01"
    assert_contains "$FINISH_OUT" "cargo clean -p rsid -p rsid-store --profile test"
}

case_the_remote_read_leaf_is_planned_in_fast_mode_and_the_full_workspace_lane() {
    setup
    # #2410: rsid-remote-read declares no shard feature, so no shard lists its tests. `fast` runs it as
    # its own package; `full` covers it with the other workspace packages; a single shard does neither.
    launch leaf-fast fast --dry-run
    finish leaf-fast
    assert_contains "$FINISH_OUT" "-p rsid-remote-read --lib"
    launch leaf-full full --dry-run
    finish leaf-full
    assert_contains "$FINISH_OUT" "--workspace --exclude rsid --exclude rsid-store"
    assert_not_contains "$FINISH_OUT" "-p rsid-remote-read"
    launch leaf-shard shard --dry-run
    finish leaf-shard
    assert_not_contains "$FINISH_OUT" "rsid-remote-read"
}

# lines <output> <verb>: the planned `nextest <verb> ... --lib` commands.
planned_lines() {
    printf '%s\n' "$1" | grep -F "nextest $2" | grep -F -e '--lib' || true
}

# every_line_has <lines> <text> / no_line_has: a non-empty set where all / none carry the text.
assert_all_have() { # <label> <lines> <text>
    assert_ne "" "$2" "$1: there are planned commands"
    [[ -z "$(printf '%s\n' "$2" | grep -v -F -e "$3")" ]] || tap_fail "$1: a command lacks $3: $2"
}

assert_none_have() { # <label> <lines> <text>
    assert_ne "" "$2" "$1: there are planned commands"
    [[ -z "$(printf '%s\n' "$2" | grep -F -e "$3")" ]] || tap_fail "$1: a command carries $3: $2"
}

case_the_runtime_listing_ignores_the_default_filter_and_a_run_does_only_on_the_slow_tier() {
    setup
    # #2270: the nextest default-filter leaves the released-migration tests out of a run, but the
    # runtime identity check needs every shard test listed; only the slow tier (the full QA, a
    # migration-touching gate) runs them.
    launch tier-plain shard --dry-run
    finish tier-plain
    assert_all_have "plain list" "$(planned_lines "$FINISH_OUT" list)" --ignore-default-filter
    assert_none_have "plain run" "$(planned_lines "$FINISH_OUT" run)" --ignore-default-filter
    launch tier-flag shard --dry-run --slow-tier
    finish tier-flag
    assert_all_have "slow-tier flag list" "$(planned_lines "$FINISH_OUT" list)" --ignore-default-filter
    assert_all_have "slow-tier flag run" "$(planned_lines "$FINISH_OUT" run)" --ignore-default-filter
    EXTRA_ENV=(RSI_LANDER_SLOW_TIER=full)
    launch tier-env shard --dry-run
    finish tier-env
    assert_all_have "slow-tier env list" "$(planned_lines "$FINISH_OUT" list)" --ignore-default-filter
    assert_all_have "slow-tier env run" "$(planned_lines "$FINISH_OUT" run)" --ignore-default-filter
}

case_a_filterset_atom_of_every_shape_is_forwarded_as_one_quoted_argument() {
    setup
    # #1510: check-touched-shards emits exact and regex atoms, not only test(name); #1605: composite
    # filtersets, the landing gate's own shape.
    local index=0 atom quoted
    for atom in \
        'test(session::tests::event_wake)' \
        'test(=session::tests::event_wake)' \
        'test(/^session::tests::manager_actions::[A-Za-z0-9_]+$/)' \
        'test(/^session::big::(?:\w+::)*(?:t3|t9)$/)' \
        'test(a) | test(b)' \
        'test(=a::b::one)|test(=a::b::two) | test(/^sandbox::/)'; do
        index=$((index + 1))
        launch "atom$index" shard --dry-run --filterset "$atom"
        finish "atom$index"
        quoted="$(printf '%q' "$atom")"
        assert_contains "$FINISH_OUT" "--filterset $quoted "$'\n' "the filterset [$atom] as one quoted argument"
    done
}

case_a_malformed_or_control_character_filterset_is_refused() {
    setup
    local index=0 atom
    for atom in 'test()' 'test(/a/' 'all()' $'test(/a\tb/)' 'test(/é/)' 'test(a) | all()' 'test(a) |' 'test(a) || test(b)' 'test(a) | test(b) x'; do
        index=$((index + 1))
        launch "bad$index" shard --dry-run --filterset "$atom"
        finish "bad$index" 2
    done
}

# The dry-run cases need no flock; the artifact-lock cases do (util-linux).
run_case "a dry run does not invoke cargo or create the lock" case_a_dry_run_does_not_invoke_cargo_or_create_the_lock
run_case "a shard selects every package that declares it" case_a_shard_selects_every_package_that_declares_it
run_case "the remote-read leaf is planned in fast mode and the full workspace lane" case_the_remote_read_leaf_is_planned_in_fast_mode_and_the_full_workspace_lane
run_case "the runtime listing ignores the default filter and a run does only on the slow tier" case_the_runtime_listing_ignores_the_default_filter_and_a_run_does_only_on_the_slow_tier
run_case "a filterset atom of every shape is forwarded as one quoted argument" case_a_filterset_atom_of_every_shape_is_forwarded_as_one_quoted_argument
run_case "a malformed or control-character filterset is refused" case_a_malformed_or_control_character_filterset_is_refused
if have_flock; then
    run_case "a peer binary survives cleanup during build and execution" case_a_peer_binary_survives_cleanup_during_build_and_execution
    run_case "a new build waits until exclusive cleanup finishes" case_a_new_build_waits_until_exclusive_cleanup_finishes
    run_case "an alone run cleans the environment-selected target" case_an_alone_run_cleans_the_environment_selected_target
    run_case "shards run with only build and lander RSI variables" case_shards_run_with_only_build_and_lander_rsi_variables
    run_case "keep artifacts retains the binary" case_keep_artifacts_retains_the_binary
    run_case "the lander environment keeps the binary it prebuilt" case_the_lander_environment_keeps_the_binary_it_prebuilt
    run_case "a keep run protects its binary from an active peer" case_a_keep_run_protects_its_binary_from_an_active_peer
    run_case "fast mode cleans between shards and at exit" case_fast_mode_cleans_between_shards_and_at_exit
    run_case "a multi-shard run restores the shared lock after a skipped clean" case_a_multi_shard_run_restores_the_shared_lock_after_a_skipped_clean
    run_case "failed tests clean and preserve the exit status" case_failed_tests_clean_and_preserve_the_exit_status
    run_case "a real fast run executes the remote-read leaf" case_a_real_fast_run_executes_the_remote_read_leaf
else
    for flock_case in \
        "a peer binary survives cleanup during build and execution" \
        "a new build waits until exclusive cleanup finishes" \
        "an alone run cleans the environment-selected target" \
        "shards run with only build and lander RSI variables" \
        "keep artifacts retains the binary" \
        "the lander environment keeps the binary it prebuilt" \
        "a keep run protects its binary from an active peer" \
        "fast mode cleans between shards and at exit" \
        "a multi-shard run restores the shared lock after a skipped clean" \
        "failed tests clean and preserve the exit status" \
        "a real fast run executes the remote-read leaf"; do
        skip_linux_only "$flock_case" "util-linux flock (the runner's artifact lock)"
    done
fi
tap_done
