#!/usr/bin/env bash
# suite-gate: tap
# scripts/rsi-stash: an isolated, per-worktree stash (hard rule 9). Port of the Python
# suite of the same name (#366).
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/rsi-stash"
REAL_GIT="$(command -v git)"
STASH_ENV=""

# The fixture: a repository with one commit and a linked worktree per case.
setup() {
    ROOT="$CASE_DIR"
    REPOSITORY="$ROOT/repository"
    fixture_repo "$REPOSITORY"
    printf 'base\n' >"$REPOSITORY/shared.txt"
    git -C "$REPOSITORY" add .
    git -C "$REPOSITORY" commit -q -m base
}

linked_worktree() { # <name>: prints the path
    git -C "$REPOSITORY" worktree add -q -b "feature-$1" "$ROOT/$1" >/dev/null 2>&1
    echo "$ROOT/$1"
}

# G <worktree> <git args...>: output with trailing newlines stripped by the caller's $( ).
G() {
    local dir="$1"
    shift
    git -C "$dir" "$@"
}

# run_stash <worktree> <args...>: the script in that directory; $STASH_ENV adds environment.
run_stash() {
    local dir="$1"
    shift
    cd "$dir" || exit 1
    # shellcheck disable=SC2086
    run_capture env $STASH_ENV bash "$SCRIPT" "$@"
    cd "$ROOT" || exit 1
}

# stash <worktree> <file> <contents>: write the file and save; prints the new stash sha.
stash() {
    printf '%s' "$3" >"$1/$2"
    run_stash "$1" save "save-$2"
    assert_code 0 "save of $2 (stderr: $RUN_ERR)"
    echo "$RUN_OUT"
}

state() { # <worktree>: porcelain status, staged entries, shared.txt BYTES (sentinel keeps a final newline)
    local shared
    read_exact shared "$1/shared.txt"
    printf '%s\n--\n%s\n--\n%s<EOF>' \
        "$(G "$1" status --porcelain=v1 --untracked-files=all)" "$(G "$1" ls-files -s)" "$shared"
}

state_tail() { # <worktree>: everything but the status
    local shared
    read_exact shared "$1/shared.txt"
    printf '%s\n--\n%s<EOF>' "$(G "$1" ls-files -s)" "$shared"
}

# Raw .git/index bytes also carry stat-cache and racy-git timestamps that git may
# refresh at any time; the staged entries and their diff are the contract.
logical_index() { # <worktree>
    printf '%s\n--\n%s' "$(G "$1" ls-files -s)" "$(G "$1" diff --cached --name-status)"
}

stage_file() { # <worktree> <file> <contents>: sets INDEX_BASELINE
    printf '%s' "$3" >"$1/$2"
    G "$1" add "$2"
    INDEX_BASELINE="$(logical_index "$1")"
    assert_contains "$INDEX_BASELINE" "$2" "the staged entry"
    assert_contains "$INDEX_BASELINE" "A"$'\t'"$2" "the staged addition"
}

# git_wrapper <body>: a PATH-first `git` that runs <body> (then the real git).
git_wrapper() {
    mkdir -p "$ROOT/bin"
    printf '#!/usr/bin/env bash\n%s' "$1" >"$ROOT/bin/git"
    chmod +x "$ROOT/bin/git"
    STASH_ENV="PATH=$ROOT/bin:$PATH"
}

ignored_build_tree() { # <worktree>
    printf 'target/\n' >"$ROOT/excludes"
    G "$1" config core.excludesFile "$ROOT/excludes"
    mkdir "$1/target"
    dd if=/dev/zero of="$1/target/large.bin" bs=1048576 count=4 2>/dev/null
}

file_size() { wc -c <"$1" | tr -d ' '; }

recovery_path() { # from RUN_ERR
    local text="${RUN_ERR##*recovery snapshot: }"
    printf '%s' "${text%%$'\n'*}"
}

list_shas() { # <worktree>: the stash shas of the worktree, one per line
    run_stash "$1" list
    printf '%s\n' "$RUN_OUT" | cut -f1
}

device_of() { stat -c %d "$1" 2>/dev/null || stat -f %d "$1"; }

case_linked_worktrees_cannot_consume_each_others_entries() {
    setup
    local first second first_sha second_sha before other_before owner other foreign command side
    first="$(linked_worktree first)"
    second="$(linked_worktree second)"
    first_sha="$(stash "$first" first.txt $'first work\n')"
    second_sha="$(stash "$second" second.txt $'second work\n')"
    assert_eq "$first_sha" "$(list_shas "$first")" "first list"
    assert_eq "$second_sha" "$(list_shas "$second")" "second list"
    for side in first second; do
        if [[ $side == first ]]; then
            owner=$first other=$second foreign=$second_sha
        else
            owner=$second other=$first foreign=$first_sha
        fi
        before="$(state "$owner")"
        other_before="$(state "$other")"
        for command in restore drop; do
            run_stash "$owner" "$command" "$foreign"
            assert_ne 0 "$RUN_CODE" "$command of a foreign entry"
            assert_contains "$RUN_ERR" "not owned by this worktree"
            assert_eq "$before" "$(state "$owner")" "owner state after $command"
            assert_eq "$other_before" "$(state "$other")" "other state after $command"
        done
    done
    run_stash "$first" restore "$first_sha"
    assert_code 0 "restore first"
    run_stash "$second" restore "$second_sha"
    assert_code 0 "restore second"
    assert_file_is "$first/shared.txt" "base"$'\n' "first shared.txt"
    assert_file_is "$second/shared.txt" "base"$'\n' "second shared.txt"
    assert_file "$first/first.txt"
    assert_file "$second/second.txt"
    run_stash "$first" drop "$first_sha"
    assert_code 0 "drop first"
    run_stash "$first" list
    assert_eq "" "$RUN_OUT" "first list after drop"
    run_stash "$second" list
    assert_contains "$RUN_OUT" "$second_sha" "second list after the other drop"
}

case_save_captures_tracked_and_untracked_changes() {
    setup
    local worktree sha
    worktree="$(linked_worktree capture)"
    printf 'modified\n' >"$worktree/shared.txt"
    G "$worktree" add shared.txt
    sha="$(stash "$worktree" untracked.txt $'new work\n')"
    assert_file_is "$worktree/shared.txt" base$'\n' "shared.txt after save"
    assert_no_file "$worktree/untracked.txt"
    run_stash "$worktree" restore "$sha"
    assert_code 0 "restore"
    assert_file_is "$worktree/shared.txt" modified$'\n' "shared.txt after restore"
    assert_file_is "$worktree/untracked.txt" "new work"$'\n' "untracked.txt after restore"
}

case_stash_layout_restores_index_worktree_and_untracked_parent() {
    setup
    local worktree sha
    worktree="$(linked_worktree layout)"
    printf 'staged\n' >"$worktree/shared.txt"
    G "$worktree" add shared.txt
    printf 'unstaged\n' >"$worktree/shared.txt"
    printf 'untracked\n' >"$worktree/new.txt"
    run_stash "$worktree" save layout
    sha="$RUN_OUT"
    assert_eq 4 "$(G "$worktree" rev-list --parents -n 1 "$sha" | wc -w | tr -d ' ')" "commit parents"
    assert_eq staged "$(G "$worktree" show "$sha^2:shared.txt")" "index commit"
    assert_eq unstaged "$(G "$worktree" show "$sha:shared.txt")" "worktree commit"
    assert_eq untracked "$(G "$worktree" show "$sha^3:new.txt")" "untracked commit"
    run_stash "$worktree" restore "$sha"
    assert_code 0 "restore (stderr: $RUN_ERR)"
    assert_eq "$sha" "$RUN_OUT" "restore output"
    assert_eq staged "$(G "$worktree" show :shared.txt)" "restored index"
    assert_file_is "$worktree/shared.txt" unstaged$'\n' "restored worktree"
    assert_file_is "$worktree/new.txt" untracked$'\n' "restored untracked"
}

case_staged_deletion_with_untracked_replacement_cleans_safely() {
    setup
    local worktree sha
    worktree="$(linked_worktree replacement)"
    G "$worktree" rm -q shared.txt
    printf 'replacement\n' >"$worktree/shared.txt"
    run_stash "$worktree" save replacement
    assert_code 0 "save (stderr: $RUN_ERR)"
    sha="$RUN_OUT"
    assert_file_is "$worktree/shared.txt" base$'\n' "shared.txt"
    assert_eq "" "$(G "$worktree" status --porcelain)" "status"
    assert_eq replacement "$(G "$worktree" show "$sha^3:shared.txt")" "untracked replacement"
}

case_save_from_a_subdirectory_captures_root_changes() {
    setup
    local worktree sha
    worktree="$(linked_worktree subdirectory)"
    mkdir "$worktree/sub"
    printf 'changed\n' >"$worktree/shared.txt"
    printf 'new\n' >"$worktree/sub/new.txt"
    run_stash "$worktree/sub" save "from subdirectory"
    assert_code 0 "save (stderr: $RUN_ERR)"
    sha="$RUN_OUT"
    assert_file_is "$worktree/shared.txt" base$'\n' "shared.txt"
    assert_eq "" "$(G "$worktree" status --porcelain)" "status"
    assert_eq new "$(G "$worktree" show "$sha^3:sub/new.txt")" "untracked in subdirectory"
}

case_a_save_cleanup_failure_rolls_back_with_the_ref_preserved() {
    setup
    local worktree before recovery saved
    worktree="$(linked_worktree rollback)"
    printf 'changed\n' >"$worktree/shared.txt"
    printf 'new\n' >"$worktree/new.txt"
    before="$(state "$worktree")"
    git_wrapper "if [[ \"\$1\" == restore ]]; then \"$REAL_GIT\" \"\$@\"; printf \"later\\n\" > late.txt; exit 72; fi
exec \"$REAL_GIT\" \"\$@\"
"
    STASH_ENV="$STASH_ENV TMPDIR=$ROOT"
    run_stash "$worktree" save failure
    assert_ne 0 "$RUN_CODE" "exit code"
    assert_contains "$RUN_ERR" "working tree and index restored"
    # Everything but the status line block (the late file shows up untracked).
    assert_eq "$(printf '%s' "$before" | sed '1,/^--$/d')" "$(state_tail "$worktree")" "index and shared.txt"
    assert_file_is "$worktree/new.txt" new$'\n' "new.txt"
    assert_file_is "$worktree/late.txt" later$'\n' "late.txt"
    recovery="$(recovery_path)"
    assert_no_file "$recovery/tree/late.txt" "late file in the recovery snapshot"
    STASH_ENV=""
    saved="$(list_shas "$worktree" | sed -n 1p)"
    assert_eq changed "$(G "$worktree" show "$saved:shared.txt")" "saved shared.txt"
    assert_eq new "$(G "$worktree" show "$saved^3:new.txt")" "saved untracked"
}

case_a_new_file_after_capture_survives_an_aborted_save() {
    setup
    local worktree saved
    worktree="$(linked_worktree late)"
    printf 'changed\n' >"$worktree/shared.txt"
    git_wrapper "if [[ \"\$1\" == update-ref ]]; then \"$REAL_GIT\" \"\$@\" || exit \$?; printf \"later\\n\" > late.txt; exit 0; fi
exec \"$REAL_GIT\" \"\$@\"
"
    run_stash "$worktree" save late
    assert_ne 0 "$RUN_CODE" "exit code"
    assert_contains "$RUN_ERR" "changed during capture"
    assert_file_is "$worktree/shared.txt" changed$'\n' "shared.txt"
    assert_file_is "$worktree/late.txt" later$'\n' "late.txt"
    STASH_ENV=""
    saved="$(list_shas "$worktree" | sed -n 1p)"
    assert_eq changed "$(G "$worktree" show "$saved:shared.txt")" "saved shared.txt"
}

case_an_interrupted_cleanup_restores_the_original_and_keeps_the_capture() {
    setup
    local worktree before saved
    worktree="$(linked_worktree interrupted)"
    printf 'changed\n' >"$worktree/shared.txt"
    printf 'new\n' >"$worktree/new.txt"
    before="$(state "$worktree")"
    git_wrapper "if [[ \"\$1\" == restore ]]; then \"$REAL_GIT\" \"\$@\" || exit \$?; kill -TERM \"\$PPID\"; exit 0; fi
exec \"$REAL_GIT\" \"\$@\"
"
    run_stash "$worktree" save interrupted
    assert_ne 0 "$RUN_CODE" "exit code"
    assert_contains "$RUN_ERR" "working tree and index restored"
    assert_eq "$before" "$(state "$worktree")" "state"
    assert_file_is "$worktree/new.txt" new$'\n' "new.txt"
    STASH_ENV=""
    saved="$(list_shas "$worktree" | sed -n 1p)"
    assert_eq changed "$(G "$worktree" show "$saved:shared.txt")" "saved shared.txt"
}

case_a_restore_conflict_keeps_tree_and_index_intact() {
    setup
    local worktree sha before
    worktree="$(linked_worktree conflict)"
    printf 'saved\n' >"$worktree/shared.txt"
    run_stash "$worktree" save conflict
    sha="$RUN_OUT"
    printf 'current\n' >"$worktree/shared.txt"
    G "$worktree" add shared.txt
    before="$(state "$worktree")"
    run_stash "$worktree" restore "$sha"
    assert_ne 0 "$RUN_CODE" "exit code"
    assert_eq "$before" "$(state "$worktree")" "state"
    assert_eq current "$(G "$worktree" show :shared.txt)" "index"
    assert_file_is "$worktree/shared.txt" current$'\n' "shared.txt"
}

case_a_restore_apply_failure_rolls_back_the_original_state() {
    setup
    local worktree sha before
    worktree="$(linked_worktree restore-failure)"
    printf 'saved\n' >"$worktree/shared.txt"
    run_stash "$worktree" save "restore failure"
    sha="$RUN_OUT"
    before="$(state "$worktree")"
    git_wrapper "if [[ \"\$1\" == stash && \"\$2\" == apply ]]; then \"$REAL_GIT\" \"\$@\" || exit \$?; exit 73; fi
exec \"$REAL_GIT\" \"\$@\"
"
    STASH_ENV="$STASH_ENV TMPDIR=$ROOT"
    run_stash "$worktree" restore "$sha"
    assert_ne 0 "$RUN_CODE" "exit code"
    assert_contains "$RUN_ERR" "working tree and index restored"
    assert_eq "$before" "$(state "$worktree")" "state"
    assert_file_is "$worktree/shared.txt" base$'\n' "shared.txt"
}

case_a_save_failure_keeps_the_ignored_tree_out_of_the_snapshot() {
    setup
    local worktree before index_before recovery
    worktree="$(linked_worktree ignored-save)"
    ignored_build_tree "$worktree"
    printf 'changed\n' >"$worktree/shared.txt"
    stage_file "$worktree" staged.txt $'staged\n'
    index_before="$INDEX_BASELINE"
    before="$(state "$worktree")"
    git_wrapper "if [[ \"\$1\" == restore ]]; then \"$REAL_GIT\" \"\$@\"; exit 72; fi
exec \"$REAL_GIT\" \"\$@\"
"
    run_stash "$worktree" save ignored
    assert_ne 0 "$RUN_CODE" "exit code"
    assert_contains "$RUN_ERR" "working tree and index restored"
    assert_eq "$before" "$(state "$worktree")" "state"
    assert_eq "$index_before" "$(logical_index "$worktree")" "index"
    assert_eq 4194304 "$(file_size "$worktree/target/large.bin")" "the ignored build output"
    recovery="$(recovery_path)"
    assert_eq "" "$(find_named "$recovery" large.bin)" "ignored output in the recovery snapshot"
    assert_file_is "$recovery/tree/shared.txt" changed$'\n' "recovery copy of shared.txt"
}

case_restore_preflight_and_rollback_skip_the_ignored_tree() {
    setup
    local worktree sha before index_before recovery
    worktree="$(linked_worktree ignored-restore)"
    printf 'saved\n' >"$worktree/shared.txt"
    run_stash "$worktree" save "ignored restore"
    sha="$RUN_OUT"
    ignored_build_tree "$worktree"
    stage_file "$worktree" staged.txt $'staged\n'
    index_before="$INDEX_BASELINE"
    before="$(state "$worktree")"
    git_wrapper "if [[ \"\$1\" == -C && \"\$3\" == stash && \"\$4\" == apply ]]; then
  [[ ! -e \"\$2/target\" ]] || exit 91
  [[ \"\$(stat -c %d \"\$2\" 2>/dev/null || stat -f %d \"\$2\")\" == \"\$EXPECTED_DEVICE\" ]] || exit 92
  printf checked > \"\$PROBE_FILE\"
fi
if [[ \"\$1\" == stash && \"\$2\" == apply ]]; then \"$REAL_GIT\" \"\$@\" || exit \$?; exit 73; fi
exec \"$REAL_GIT\" \"\$@\"
"
    STASH_ENV="$STASH_ENV EXPECTED_DEVICE=$(device_of "$worktree") PROBE_FILE=$ROOT/preflight-probe"
    run_stash "$worktree" restore "$sha"
    assert_ne 0 "$RUN_CODE" "exit code"
    assert_file_is "$ROOT/preflight-probe" checked "the preflight probe"
    assert_eq "$before" "$(state "$worktree")" "state"
    assert_eq "$index_before" "$(logical_index "$worktree")" "index"
    assert_eq 4194304 "$(file_size "$worktree/target/large.bin")" "the ignored build output"
    recovery="$(recovery_path)"
    assert_eq "" "$(find_named "$recovery" large.bin)" "ignored output in the recovery snapshot"
}

case_a_detached_recovery_copy_preserves_the_original_index_and_namespace() {
    setup
    local original detached existing_sha index_before status_before list_before original_index detached_index path detached_sha
    original="$(linked_worktree recovery-source)"
    existing_sha="$(stash "$original" older.txt $'older\n')"
    printf 'staged\n' >"$original/shared.txt"
    G "$original" add shared.txt
    printf 'unstaged\n' >"$original/shared.txt"
    printf 'untracked\n' >"$original/new.txt"
    original_index="$(G "$original" rev-parse --git-path index)"
    [[ "$original_index" == /* ]] || original_index="$original/$original_index"
    index_before="$(logical_index "$original")"
    assert_contains "$index_before" "M"$'\t'"shared.txt"
    status_before="$(state "$original")"
    run_stash "$original" list
    list_before="$RUN_OUT"
    # Mirror the documented safe-copy procedure: detached worktree, selected
    # tracked/untracked paths, then the original index in the detached index.
    detached="$ROOT/recovery-copy"
    G "$REPOSITORY" worktree add -q --detach "$detached" HEAD >/dev/null 2>&1
    while IFS= read -r path; do
        [[ -n "$path" ]] || continue
        if [[ -f "$original/$path" || -L "$original/$path" ]]; then
            mkdir -p "$(dirname -- "$detached/$path")"
            cp -pP "$original/$path" "$detached/$path"
        elif [[ -f "$detached/$path" || -L "$detached/$path" ]]; then
            rm -f "${detached:?}/${path:?}"
        fi
    done < <({
        G "$original" ls-files --cached -z
        G "$original" ls-files --others --exclude-standard -z
        G "$original" diff --no-renames HEAD --name-only -z
    } | tr '\0' '\n' | sort -u)
    detached_index="$(G "$detached" rev-parse --git-path index)"
    [[ "$detached_index" == /* ]] || detached_index="$detached/$detached_index"
    cp -p "$original_index" "$detached_index"
    run_stash "$detached" save "recovery copy"
    assert_code 0 "save in the detached copy (stderr: $RUN_ERR)"
    detached_sha="$RUN_OUT"
    assert_eq "$index_before" "$(logical_index "$original")" "original index"
    assert_eq "$status_before" "$(state "$original")" "original state"
    run_stash "$original" list
    assert_eq "$list_before" "$RUN_OUT" "original list"
    assert_contains "$list_before" "$existing_sha"
    run_stash "$detached" list
    assert_contains "$RUN_OUT" "$detached_sha" "the detached list"
    assert_not_contains "$list_before" "$detached_sha" "the original list"
}

run_case "linked worktrees cannot consume each other's entries" case_linked_worktrees_cannot_consume_each_others_entries
run_case "save captures tracked and untracked changes" case_save_captures_tracked_and_untracked_changes
run_case "the stash layout restores the index, worktree and untracked parent" case_stash_layout_restores_index_worktree_and_untracked_parent
run_case "a staged deletion with an untracked replacement cleans safely" case_staged_deletion_with_untracked_replacement_cleans_safely
run_case "save from a subdirectory captures root changes" case_save_from_a_subdirectory_captures_root_changes
run_case "a save cleanup failure rolls back with the ref preserved" case_a_save_cleanup_failure_rolls_back_with_the_ref_preserved
run_case "a new file after capture survives an aborted save" case_a_new_file_after_capture_survives_an_aborted_save
run_case "an interrupted cleanup restores the original and keeps the capture" case_an_interrupted_cleanup_restores_the_original_and_keeps_the_capture
run_case "a restore conflict keeps the tree and index intact" case_a_restore_conflict_keeps_tree_and_index_intact
run_case "a restore apply failure rolls back the original state" case_a_restore_apply_failure_rolls_back_the_original_state
run_case "a save failure keeps the ignored tree out of the snapshot" case_a_save_failure_keeps_the_ignored_tree_out_of_the_snapshot
run_case "restore preflight and rollback skip the ignored tree" case_restore_preflight_and_rollback_skip_the_ignored_tree
run_case "a detached recovery copy preserves the original index and namespace" case_a_detached_recovery_copy_preserves_the_original_index_and_namespace
tap_done
