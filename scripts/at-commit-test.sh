#!/bin/bash
# at-commit-test.sh <sha> <cargo test args...>: the body of a `test` job with
# `params.at_commit` (#3410).
#
# Runs one exact package test at a past commit of THIS repository, in a
# temporary detached worktree that is always removed, so the caller's sandbox,
# branch and index are never touched (hard rule 2). The job runner builds the
# argv (`test -p <pkg> [--lib|--test T] --no-fail-fast -- <names> --exact`) and
# runs this script inside the cargo build slot with the sandbox's own
# CARGO_TARGET_DIR, so the build is bounded and its output is reclaimed with
# the sandbox. The exit code is cargo's; 2 means the commit could not be
# checked out.
set -u
sha="${1:?usage: at-commit-test.sh <sha> <cargo args...>}"
shift
case "$sha" in
  *[!0-9a-f]* | "") echo "at-commit-test: $sha is not a lowercase hex commit id" >&2; exit 2 ;;
esac
[ "${#sha}" -eq 40 ] || { echo "at-commit-test: $sha is not a 40-hex commit id" >&2; exit 2; }
repo="$(git rev-parse --show-toplevel)" || exit 2

have() { git -C "$repo" cat-file -e "${sha}^{commit}" 2>/dev/null; }
if ! have; then
  # Best effort: a commit only the remote has.
  timeout 60 git -C "$repo" fetch --quiet origin "$sha" 2>/dev/null || true
  have || { echo "at-commit-test: commit $sha is not in this repository" >&2; exit 2; }
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/at-commit-test.XXXXXX")" || exit 2
cleanup() {
  git -C "$repo" worktree remove --force "$work/wt" >/dev/null 2>&1 || true
  rm -rf "$work"
  git -C "$repo" worktree prune >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 143' TERM INT HUP
# Hooks off: this checkout is scratch, a repository hook must not run in it.
git -C "$repo" -c core.hooksPath=/dev/null worktree add --detach --quiet "$work/wt" "$sha" || exit 2
echo "at-commit-test: $(git -C "$work/wt" log -1 --format='%H %s' "$sha")"
cd "$work/wt" || exit 2
cargo "$@"
