#!/usr/bin/env bash
# suite-gate: tap
# Local sweep fixtures require existing, physically contained Git origins.
# Port of the Python suite of the same name (#366).
source "$(dirname "$0")/lib/tap.sh"

SCRIPT="$SCRIPTS_DIR/cloud-sweep.sh"

setup() {
    ROOT="$CASE_DIR"
    ALLOWED="$ROOT/allowed"
    ORIGIN="$ALLOWED/origin.git"
    CALLER="$ROOT/caller"
    mkdir -p "$ALLOWED" "$ORIGIN" "$CALLER"
    git -C "$ORIGIN" init -q --bare
    fixture_repo "$CALLER"
    git -C "$CALLER" branch -q -m rolling
    git -C "$CALLER" commit -q --allow-empty -m tip
    SHA="$(git -C "$CALLER" rev-parse HEAD)"
    git -C "$CALLER" push -q "$ORIGIN" rolling
    git -C "$CALLER" remote add origin "$ORIGIN"
}

# bundle <origin url or path> [allowed roots...]: the script's `bundle` subcommand.
bundle() {
    local origin="$1" root
    shift
    git -C "$CALLER" config remote.origin.url "$origin"
    local -a args=(bundle "$SHA" "$ROOT/tip.bundle" --repo "$CALLER" --mirror "$ROOT/mirror.git")
    for root in "$@"; do
        args+=(--allow-local-root "$root")
    done
    rm -f "$ROOT/tip.bundle"
    CDPATH="$ROOT" run_capture bash "$SCRIPT" "${args[@]}"
}

assert_bundled() { # <origin> [roots...]
    bundle "$@"
    assert_code 0 "bundling $1 (stderr: $RUN_ERR)"
    assert_eq "$SHA refs/remotes/origin/rolling" "$(git -C "$ROOT" bundle list-heads "$ROOT/tip.bundle")" "bundle heads for $1"
}

assert_refused() { # <origin> [roots...]
    bundle "$@"
    assert_ne 0 "$RUN_CODE" "exit code for $1"
    assert_contains "$RUN_ERR" "cloud-sweep: refusing origin URL" "refusal of $1"
}

case_an_existing_local_path_and_file_url_bundle_the_exact_tip() {
    setup
    assert_bundled "$ORIGIN" "$ALLOWED"
    assert_bundled "file://$ORIGIN" "$ALLOWED"
    assert_bundled "$ALLOWED/../allowed/origin.git" "$ALLOWED"
}

case_a_symlinked_origin_and_root_are_resolved_physically() {
    setup
    ln -s "$ALLOWED" "$ROOT/alias"
    ln -s "$ORIGIN" "$ALLOWED/link.git"
    assert_bundled "$ALLOWED/link.git" "$ROOT/alias"
    assert_bundled "$ROOT/alias/origin.git" "$ALLOWED"
}

case_local_origins_require_an_explicit_allowed_root() {
    setup
    assert_refused "$ORIGIN"
    assert_refused "file://$ORIGIN"
}

case_missing_paths_roots_and_regular_files_are_refused() {
    setup
    local origin
    printf 'not a repository' >"$ALLOWED/regular"
    ln -s "$ALLOWED/missing" "$ALLOWED/dangling"
    for origin in "$ALLOWED/missing" "$ALLOWED/dangling" "$ALLOWED/regular" "$ALLOWED/missing/../origin.git"; do
        assert_refused "$origin" "$ALLOWED"
    done
    assert_refused "$ORIGIN" "$ROOT/missing"
    assert_refused "$ORIGIN" "$ALLOWED/regular"
    assert_bundled "$ORIGIN" "$ROOT/missing" "$ALLOWED"
}

case_a_sibling_prefix_parent_traversal_and_symlink_escape_are_refused() {
    setup
    local origin
    mkdir "$ROOT/allowed-other"
    ln -s "$ROOT/allowed-other" "$ALLOWED/escaped"
    for origin in "$ROOT/allowed-other" "$ALLOWED/escaped" "$ALLOWED/../allowed-other" "$ALLOWED"; do
        assert_refused "$origin" "$ALLOWED"
    done
}

case_unsupported_and_hostile_transports_are_refused_even_with_local_permission() {
    setup
    local origin
    for origin in "ext::sh -c true" "evil::repo" "-oProxyCommand=x" \
        "http://example.com/x.git" "git://example.com/x.git" \
        "ssh://-oProxyCommand=x/y" "https://-host/x" \
        "git@github.com:-x" "file://localhost/etc" \
        "file://relative" "https://example.com/a b" $'ssh://host/a\nb'; do
        assert_refused "$origin" "$ALLOWED"
    done
}

run_case "an existing local path and file URL bundle the exact tip" case_an_existing_local_path_and_file_url_bundle_the_exact_tip
run_case "a symlinked origin and root are resolved physically" case_a_symlinked_origin_and_root_are_resolved_physically
run_case "local origins require an explicit allowed root" case_local_origins_require_an_explicit_allowed_root
run_case "missing paths, roots and regular files are refused" case_missing_paths_roots_and_regular_files_are_refused
run_case "a sibling prefix, parent traversal and symlink escape are refused" case_a_sibling_prefix_parent_traversal_and_symlink_escape_are_refused
run_case "unsupported and hostile transports are refused even with local permission" case_unsupported_and_hostile_transports_are_refused_even_with_local_permission
tap_done
