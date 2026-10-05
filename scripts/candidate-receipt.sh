#!/bin/bash
# candidate-receipt.sh <ref>: the body of the `candidate_receipt` job (#1099).
#
# Verifies one candidate (a branch or full commit of THIS repository) against
# origin/rolling in a temporary detached worktree and prints the one-line
# summary plus a final `RECEIPT_JSON <compact json>` line the daemon parses
# into the job result. Run from a repository checkout (the job's cwd); uses
# that checkout's scripts/check-touched-shards so a candidate cut before the
# script existed still gets a receipt.
#
# The worktree is temporary and always removed. The cargo target directory is
# its own (never the caller's sandbox target), shared across receipts so they
# are incremental: $RSI_CANDIDATE_TARGET_DIR or ~/.cache/rsi-candidate-receipt/target.
set -u
ref="${1:?usage: candidate-receipt.sh <ref>}"
repo="$(git rev-parse --show-toplevel)" || exit 2
checker="$repo/scripts/check-touched-shards"
[ -x "$checker" ] || { echo "candidate-receipt: $checker missing" >&2; exit 2; }

# Best effort: refresh rolling, and a branch that is not local yet.
timeout 60 git -C "$repo" fetch --quiet origin rolling 2>/dev/null || true
resolve() {
  git -C "$repo" rev-parse --verify --quiet "${ref}^{commit}" \
    || git -C "$repo" rev-parse --verify --quiet "origin/${ref}^{commit}"
}
sha="$(resolve)" || {
  timeout 60 git -C "$repo" fetch --quiet origin "refs/heads/${ref}:refs/remotes/origin/${ref}" 2>/dev/null || true
  sha="$(resolve)" || { echo "candidate-receipt: cannot resolve ref $ref" >&2; exit 2; }
}

cache="${RSI_CANDIDATE_TARGET_DIR:-$HOME/.cache/rsi-candidate-receipt/target}"
mkdir -p "$cache"
work="$(mktemp -d "${TMPDIR:-/tmp}/candidate-receipt.XXXXXX")" || exit 2
cleanup() {
  git -C "$repo" worktree remove --force "$work/wt" >/dev/null 2>&1 || true
  rm -rf "$work"
  git -C "$repo" worktree prune >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 143' TERM INT HUP
git -C "$repo" worktree add --detach --quiet "$work/wt" "$sha" || exit 2

# One receipt at a time per target dir (cargo would serialise on its lock anyway).
exec 9>"$cache/.lock"
flock 9
export CARGO_TARGET_DIR="$cache"
python3 "$checker" --repo "$work/wt" --base origin/rolling --head "$sha" --receipt-line
