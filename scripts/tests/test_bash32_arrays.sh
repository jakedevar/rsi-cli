#!/usr/bin/env bash
# suite-gate: tap
# bash 3.2 empty-array safety (mac-mini #50): rsi-stash under old and new bash.
# The static guard over the macOS-run scripts is `scripts/repo-guard bash32-arrays`
# (crates/rsi-repo-guards, #360), tested by `cargo test -p rsi-repo-guards`.
# Port of the Python suite of the same name (#366).
#
# Case map (legacy test_bash32_arrays.py: StashUnderBash run once per bash class, Bash32 and Bash5):
#   test_empty_temp_dirs_paths_do_not_trip_set_u, test_save_restore_drop_round_trip,
#   test_two_saves_in_one_second_get_distinct_refs  -> the same three cases per available bash.
# Restored file contents are compared byte for byte (assert_file_is).
source "$(dirname "$0")/lib/tap.sh"

STASH="$SCRIPTS_DIR/rsi-stash"

# bash_version <path>: "major.minor" of that bash, or nothing.
bash_version() {
    "$1" -c 'echo "${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}"' 2>/dev/null
}

# find_bash <min|max> <major.minor>: the first candidate bash below / at-or-above a version.
find_bash() {
    local want="$1" bound="$2" candidate version major minor bound_major bound_minor
    bound_major="${bound%%.*}"
    bound_minor="${bound##*.}"
    for candidate in /bin/bash /usr/bin/bash /opt/homebrew/bin/bash /usr/local/bin/bash "$(command -v bash)"; do
        [[ -x "$candidate" ]] || continue
        version="$(bash_version "$candidate")" || continue
        [[ "$version" == *.* ]] || continue
        major="${version%%.*}"
        minor="${version##*.}"
        if [[ "$want" == below ]]; then
            if ((major < bound_major || (major == bound_major && minor < bound_minor))); then
                echo "$candidate"
                return 0
            fi
        elif ((major > bound_major || (major == bound_major && minor >= bound_minor))); then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

setup() { # <bash>
    BASH_UNDER_TEST="$1"
    REPO="$CASE_DIR/repo"
    fixture_repo "$REPO"
    printf 'base\n' >"$REPO/shared.txt"
    git -C "$REPO" add .
    git -C "$REPO" commit -q -m base
}

stash() { # <args...>: rsi-stash under the bash being tested, in the repository
    cd "$REPO" || exit 1
    run_capture "$BASH_UNDER_TEST" "$STASH" "$@"
    cd "$CASE_DIR" || exit 1
}

ok() { # <args...>: must succeed, silently
    stash "$@"
    assert_not_contains "$RUN_ERR" "unbound variable"
    assert_code 0 "rsi-stash $* (stderr: $RUN_ERR)"
    assert_eq "" "$RUN_ERR" "stderr of rsi-stash $*"
}

# No temp dir is created by list, drop, usage or a refused save: the cleanup trap
# expands an empty temp_dirs array.
case_empty_temp_dirs_paths_do_not_trip_set_u() {
    setup "$1"
    ok list
    assert_eq "" "$RUN_OUT" "list output"
    stash --help
    assert_not_contains "$RUN_ERR" "unbound variable"
    assert_code 2 "usage exit code"
    assert_contains "$RUN_ERR" "Usage: rsi-stash"
    stash save
    assert_not_contains "$RUN_ERR" "unbound variable"
    assert_code 1 "clean save exit code"
    assert_contains "$RUN_ERR" "no uncommitted work"
}

case_save_restore_drop_round_trip() {
    setup "$1"
    local sha
    printf 'saved\n' >"$REPO/shared.txt"
    printf 'untracked\n' >"$REPO/new.txt"
    ok save "round trip"
    sha="$RUN_OUT"
    printf '%s\n' "$sha" | grep -Eq '^[0-9a-f]{40,64}$' || tap_fail "not a sha: [$sha]"
    assert_file_is "$REPO/shared.txt" base$'\n' "shared.txt after save"
    assert_no_file "$REPO/new.txt"
    ok list
    assert_eq "$sha" "$(printf '%s' "$RUN_OUT" | cut -f1)" "listed sha"
    ok restore "$sha"
    assert_eq "$sha" "$RUN_OUT" "restore output"
    assert_file_is "$REPO/shared.txt" saved$'\n' "shared.txt after restore"
    assert_file_is "$REPO/new.txt" untracked$'\n' "new.txt after restore"
    ok drop "$sha"
    ok list
    assert_eq "" "$RUN_OUT" "list after drop"
}

case_two_saves_in_one_second_get_distinct_refs() {
    setup "$1"
    local first second
    printf 'a\n' >"$REPO/a.txt"
    ok save first
    first="$RUN_OUT"
    printf 'b\n' >"$REPO/b.txt"
    ok save second
    second="$RUN_OUT"
    assert_ne "$first" "$second" "the two refs"
    ok list
    assert_eq 2 "$(printf '%s\n' "$RUN_OUT" | wc -l | tr -d ' ')" "listed entries"
}

# run_under <label> <bash>: the three cases under that bash (each case gets the path).
run_under() {
    local label="$1"
    BASH_PATH="$2"
    run_case "[$label] empty temp_dirs paths do not trip set -u" case_under_empty
    run_case "[$label] save, restore, drop round trip" case_under_round_trip
    run_case "[$label] two saves in one second get distinct refs" case_under_two_saves
}

# A missing bash variant skips each of the three cases by name (the legacy class skipped each
# inherited test), so the report says exactly what was not run.
skip_under() { # <label> <reason>
    skip_case "[$1] empty temp_dirs paths do not trip set -u" "$2"
    skip_case "[$1] save, restore, drop round trip" "$2"
    skip_case "[$1] two saves in one second get distinct refs" "$2"
}
case_under_empty() { case_empty_temp_dirs_paths_do_not_trip_set_u "$BASH_PATH"; }
case_under_round_trip() { case_save_restore_drop_round_trip "$BASH_PATH"; }
case_under_two_saves() { case_two_saves_in_one_second_get_distinct_refs "$BASH_PATH"; }

BASH32="$(find_bash below 4.4)" || BASH32=""
BASH5="$(find_bash atleast 5.0)" || BASH5=""
if [[ -n "$BASH32" ]]; then
    run_under "bash < 4.4" "$BASH32"
else
    skip_under "bash < 4.4" "no bash < 4.4 on this host (macOS /bin/bash 3.2 is the target)"
fi
if [[ -n "$BASH5" ]]; then
    run_under "bash >= 5" "$BASH5"
else
    skip_under "bash >= 5" "no bash >= 5 on this host"
fi
tap_done
