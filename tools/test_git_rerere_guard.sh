#!/usr/bin/env bash
# suite-gate: tap
# Real-repository checks of the shared rerere hook guard. Port of the Python suite
# tools/test_git_rerere_guard.py (#366).
source "$(dirname "$0")/../scripts/tests/lib/tap.sh"

TOOLS="$REPO_ROOT/tools"

setup_repo() {
    ROOT="$CASE_DIR"
    REPO="$ROOT/repo"
    mkdir -p "$REPO"
    export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null CLAUDE_AGENT_ROLE=""
    git -C "$REPO" init -q
    git -C "$REPO" config user.name Test
    git -C "$REPO" config user.email test@example.invalid
    mkdir -p "$REPO/tools"
    cp -R "$TOOLS/git-hooks" "$REPO/tools/git-hooks"
    cp -p "$TOOLS/install-hooks.sh" "$REPO/tools/install-hooks.sh"
}

# hook <name> [repo]: run the hook script in that repository; sets RUN_*.
hook() {
    cd "${2:-$REPO}" || exit 1
    run_capture sh "$REPO/tools/git-hooks/$1"
    cd "$ROOT" || exit 1
}

case_true_is_refused_and_false_and_unset_pass_both_commit_hooks() {
    setup_repo
    local name
    for name in pre-commit pre-merge-commit; do
        hook "$name"
        assert_code 0 "$name with rerere unset"
        git -C "$REPO" config rerere.enabled false
        hook "$name"
        assert_code 0 "$name with rerere false"
        git -C "$REPO" config rerere.enabled true
        hook "$name"
        assert_code 1 "$name with rerere true"
        assert_contains "$RUN_ERR" "git config rerere.enabled false" "$name"
        git -C "$REPO" config --unset rerere.enabled
    done
}

case_a_linked_worktree_checks_the_common_config_even_with_a_false_override() {
    setup_repo
    git -C "$REPO" add tools
    git -C "$REPO" commit -q --no-verify -m seed
    local worktree="$ROOT/sandbox"
    git -C "$REPO" worktree add -q -b sandbox "$worktree"
    git -C "$REPO" config extensions.worktreeConfig true
    git -C "$worktree" config --worktree rerere.enabled false
    git -C "$REPO" config --local rerere.enabled true
    hook pre-merge-commit "$worktree"
    assert_code 1 "pre-merge-commit in the linked worktree"
}

case_an_included_true_is_refused_and_an_explicit_local_false_passes() {
    setup_repo
    printf '[rerere]\n\tenabled = true\n' >"$ROOT/included.conf"
    git -C "$REPO" config include.path "$ROOT/included.conf"
    hook pre-commit
    assert_code 1 "included true"
    git -C "$REPO" config rerere.enabled false
    hook pre-commit
    assert_code 0 "explicit local false"
}

case_a_clean_merge_invokes_the_guard_without_an_agent_role() {
    setup_repo
    git -C "$REPO" config core.hooksPath tools/git-hooks
    git -C "$REPO" add tools
    git -C "$REPO" commit -q -m base
    git -C "$REPO" branch other
    printf 'ours\n' >"$REPO/ours.txt"
    git -C "$REPO" add ours.txt
    git -C "$REPO" commit -q -m ours
    git -C "$REPO" checkout -q other
    printf 'theirs\n' >"$REPO/theirs.txt"
    git -C "$REPO" add theirs.txt
    git -C "$REPO" commit -q -m other
    git -C "$REPO" checkout -q -
    git -C "$REPO" config rerere.enabled true
    local failure
    if failure="$(git -C "$REPO" merge --no-ff -m merge other 2>&1)"; then
        tap_fail "the merge was not refused with rerere enabled"
    fi
    assert_contains "$failure" "git config rerere.enabled false"
    assert_file "$REPO/.git/MERGE_HEAD" "the pending merge"
    git -C "$REPO" config rerere.enabled false
    git -C "$REPO" commit -q -m merge
    assert_eq 3 "$(git -C "$REPO" rev-list --parents -n 1 HEAD | wc -w | tr -d ' ')" "commit and parents"
}

case_the_installer_disables_shared_rerere_and_preserves_the_cache() {
    setup_repo
    git -C "$REPO" config rerere.enabled true
    mkdir -p "$REPO/.git/rr-cache/fixture"
    printf 'neutral resolution\n' >"$REPO/.git/rr-cache/fixture/postimage"
    (cd "$REPO" && sh tools/install-hooks.sh >/dev/null 2>&1) || tap_fail "the installer failed"
    assert_eq false "$(git -C "$REPO" config --local --bool rerere.enabled)" "rerere.enabled"
    assert_file_is "$REPO/.git/rr-cache/fixture/postimage" $'neutral resolution\n' "the cache"
    assert_executable "$REPO/tools/git-hooks/pre-merge-commit"
}

run_case "true is refused and false and unset pass both commit hooks" case_true_is_refused_and_false_and_unset_pass_both_commit_hooks
run_case "a linked worktree checks the common config even with a false override" case_a_linked_worktree_checks_the_common_config_even_with_a_false_override
run_case "an included true is refused and an explicit local false passes" case_an_included_true_is_refused_and_an_explicit_local_false_passes
run_case "a clean merge invokes the guard without an agent role" case_a_clean_merge_invokes_the_guard_without_an_agent_role
run_case "the installer disables shared rerere and preserves the cache" case_the_installer_disables_shared_rerere_and_preserves_the_cache
tap_done
