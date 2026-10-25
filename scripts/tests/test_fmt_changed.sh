#!/usr/bin/env bash
# suite-gate: tap
# #1121: formatting a changed file leaves its untouched child modules alone.
# Port of the Python suite of the same name (#366).
#
# Case map (legacy test_fmt_changed.py FmtChangedTest -> here, same order, same names):
#   formats_the_changed_parent_and_restores_its_untouched_child, a_changed_child_is_formatted_with_its_parent,
#   base_mode_formats_files_committed_since_the_base, no_changed_rust_file_is_a_no_op,
#   moving_base_formats_only_worker_changes, includes_staged_unstaged_and_untracked_rust_files (both bases),
#   generated_include_fragment_is_skipped_and_ordinary_file_formatted,
#   only_a_generated_fragment_changed_exits_zero_and_prints_nothing.
# Every file comparison is byte-exact (assert_content -> cmp): a missing final newline, a doubled one,
# or a changed untouched child fails. Without rustfmt the whole suite skips and the runner reports
# it red (`no_tests_executed`): rustfmt is a pinned-toolchain prerequisite, not an optional extra.
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/fmt-changed.sh"
UNFORMATTED_CHILD=$'pub fn child( ) -> u8 {  1 }'
FORMATTED_PARENT_PLUS_ONE=$'mod child;\npub fn parent() -> u8 {\n    child::child() + 1\n}'
FORMATTED_PARENT=$'mod child;\npub fn parent() -> u8 {\n    child::child()\n}'

# A file must hold EXACTLY the expected bytes plus the final newline rustfmt writes: command
# substitution would drop trailing newlines and let a missing or doubled one pass (#474).
assert_content() { # <path under the repo> <expected without its final newline> <label>
    assert_file_is "$ROOT/$1" "$2"$'\n' "$3"
}

setup_root() {
    ROOT="$CASE_DIR/repo"
    fixture_repo "$ROOT"
    mkdir "$ROOT/src"
    write_file "$ROOT/src/lib.rs" $'mod child;\npub fn parent() -> u8 {\n    child::child()\n}\n'
    write_file "$ROOT/src/child.rs" "$UNFORMATTED_CHILD"$'\n'
    fixture_commit "$ROOT" base
}

run_script() { # [args...]
    cd "$ROOT" || exit 1
    run_capture bash "$SCRIPT" "$@"
    cd "$CASE_DIR" || exit 1
}

case_formats_the_changed_parent_and_restores_its_untouched_child() {
    setup_root
    write_file "$ROOT/src/lib.rs" $'mod child;\npub fn parent( ) -> u8 { child::child() + 1 }\n'
    run_script
    assert_code 0 "exit code"
    assert_stdout_is src/lib.rs$'\n' "printed files"
    assert_content src/lib.rs "$FORMATTED_PARENT_PLUS_ONE" "parent"
    assert_content src/child.rs "$UNFORMATTED_CHILD" "child"
    assert_eq " M src/lib.rs" "$(git -C "$ROOT" status --porcelain)" "status"
}

case_a_changed_child_is_formatted_with_its_parent() {
    setup_root
    write_file "$ROOT/src/lib.rs" $'mod child;\npub fn parent( ) -> u8 { child::child() }\n'
    write_file "$ROOT/src/child.rs" $'pub fn child( ) -> u8 {  2 }\n'
    run_script
    assert_code 0 "exit code"
    assert_content src/child.rs $'pub fn child() -> u8 {\n    2\n}' "child"
}

case_base_mode_formats_files_committed_since_the_base() {
    setup_root
    local base
    base="$(git -C "$ROOT" rev-parse HEAD)"
    write_file "$ROOT/src/lib.rs" $'mod child;\npub fn parent( ) -> u8 { child::child() }\n'
    git -C "$ROOT" commit -q -am "unformatted change"
    run_script "$base"
    assert_code 0 "exit code"
    assert_content src/lib.rs "$FORMATTED_PARENT" "parent"
    assert_content src/child.rs "$UNFORMATTED_CHILD" "child"
}

case_no_changed_rust_file_is_a_no_op() {
    setup_root
    write_file "$ROOT/README.md" $'notes\n'
    run_script
    assert_code 0 "exit code"
    assert_stdout_is "" "printed files"
    assert_content src/child.rs "$UNFORMATTED_CHILD" "child"
}

case_moving_base_formats_only_worker_changes() {
    setup_root
    local base
    base="$(git -C "$ROOT" rev-parse HEAD)"
    write_file "$ROOT/src/child.rs" $'pub fn child( ) -> u8 {  2 }\n'
    git -C "$ROOT" commit -q -am "upstream change"
    git -C "$ROOT" update-ref refs/remotes/origin/rolling HEAD
    # Build the worker's divergent history in this disposable fixture.
    git -C "$ROOT" reset -q --hard "$base"
    write_file "$ROOT/src/lib.rs" $'mod child;\npub fn parent( ) -> u8 { child::child() + 1 }\n'
    git -C "$ROOT" commit -q -am "worker change"
    run_script origin/rolling
    assert_code 0 "exit code"
    assert_stdout_is src/lib.rs$'\n' "printed files"
    assert_content src/child.rs "$UNFORMATTED_CHILD" "child"
    assert_content src/lib.rs "$FORMATTED_PARENT_PLUS_ONE" "parent"
}

# Staged, unstaged and untracked Rust files, with and without an explicit HEAD base.
case_includes_staged_unstaged_and_untracked_rust_files() {
    setup_root
    local args
    for args in "" "HEAD"; do
        write_file "$ROOT/src/staged.rs" $'pub fn staged( ) { }\n'
        git -C "$ROOT" add src/staged.rs
        write_file "$ROOT/src/child.rs" $'pub fn child( ) -> u8 {  2 }\n'
        write_file "$ROOT/src/untracked.rs" $'pub fn untracked( ) { }\n'
        # shellcheck disable=SC2086
        run_script $args
        assert_code 0 "exit code (base [$args])"
        assert_stdout_is $'src/child.rs\nsrc/staged.rs\nsrc/untracked.rs'$'\n' "printed files (base [$args])"
        assert_content src/child.rs $'pub fn child() -> u8 {\n    2\n}' "child (base [$args])"
        assert_content src/staged.rs 'pub fn staged() {}' "staged (base [$args])"
        assert_content src/untracked.rs 'pub fn untracked() {}' "untracked (base [$args])"
        # Start the second round from the original state.
        git -C "$ROOT" reset -q --hard HEAD
        rm -f "$ROOT/src/untracked.rs" "$ROOT/src/staged.rs"
        write_file "$ROOT/src/child.rs" "$UNFORMATTED_CHILD"$'\n'
    done
}

case_a_generated_include_fragment_is_skipped_and_an_ordinary_file_formatted() {
    setup_root
    # #76: an `include!` expression fragment is not parseable by rustfmt.
    local fragment=$'// GENERATED by `node scripts/slices.mjs`. Do not edit.\ntauri::generate_handler![\n    a,\n    b\n]'
    local marked=$'// GENERATED by a tool\nfoo![ 1,2 ]'
    write_file "$ROOT/src/handlers.generated.rs" "$fragment"$'\n'
    write_file "$ROOT/src/marked.rs" "$marked"$'\n'
    write_file "$ROOT/src/lib.rs" $'mod child;\npub fn parent( ) -> u8 { child::child() }\n'
    run_script
    assert_code 0 "exit code"
    assert_stdout_is src/lib.rs$'\n' "printed files"
    assert_content src/lib.rs "$FORMATTED_PARENT" "parent"
    assert_content src/handlers.generated.rs "$fragment" "fragment"
    assert_content src/marked.rs "$marked" "marked file"
    assert_contains "$RUN_ERR" "src/handlers.generated.rs"
    assert_contains "$RUN_ERR" "src/marked.rs"
    assert_content src/child.rs "$UNFORMATTED_CHILD" "child"
}

case_only_a_generated_fragment_changed_exits_zero_and_prints_nothing() {
    setup_root
    write_file "$ROOT/src/handlers.generated.rs" $'tauri::generate_handler![ a ]\n'
    run_script
    assert_code 0 "exit code"
    assert_stdout_is "" "printed files"
}

if command -v rustfmt >/dev/null 2>&1; then
    run_case "formats the changed parent and restores its untouched child" case_formats_the_changed_parent_and_restores_its_untouched_child
    run_case "a changed child is formatted with its parent" case_a_changed_child_is_formatted_with_its_parent
    run_case "base mode formats files committed since the base" case_base_mode_formats_files_committed_since_the_base
    run_case "no changed Rust file is a no-op" case_no_changed_rust_file_is_a_no_op
    run_case "a moving base formats only worker changes" case_moving_base_formats_only_worker_changes
    run_case "includes staged, unstaged and untracked Rust files" case_includes_staged_unstaged_and_untracked_rust_files
    run_case "a generated include fragment is skipped and an ordinary file formatted" case_a_generated_include_fragment_is_skipped_and_an_ordinary_file_formatted
    run_case "only a generated fragment changed exits zero and prints nothing" case_only_a_generated_fragment_changed_exits_zero_and_prints_nothing
else
    skip_case "fmt-changed formatting cases" "rustfmt is not installed"
fi
tap_done
