#!/usr/bin/env bash
# Format only the Rust files this change touched (AGENTS.md rule 6).
#
# usage: scripts/fmt-changed.sh [BASE]
#   no BASE: the .rs files that differ from HEAD (staged or not), plus untracked
#   BASE:    the .rs files that differ between merge-base(BASE, HEAD) and the
#            working tree, plus untracked files
#            (for example an integration worktree: scripts/fmt-changed.sh origin/rolling)
#
# rustfmt also rewrites every out-of-line child module (`mod foo;`) of a file
# it is given, and `skip_children` is nightly-only, so a file the formatter
# rewrote that was clean before and is not part of the change is restored
# from the index afterwards (#1121). Prints the files it formatted.
set -euo pipefail

base=${1:-HEAD}
top=$(git rev-parse --show-toplevel)
cd "$top"
base=$(git merge-base "$base" HEAD)

mapfile -t changed < <(
    {
        git diff --name-only --diff-filter=d "$base" -- '*.rs'
        git ls-files --others --exclude-standard -- '*.rs'
    } | sort -u
)
if [ "${#changed[@]}" -eq 0 ]; then
    exit 0
fi

declare -A in_change=()
for file in "${changed[@]}"; do
    in_change["$file"]=1
done
declare -A dirty_before=()
while IFS= read -r file; do
    [ -n "$file" ] && dirty_before["$file"]=1
done < <(git diff --name-only)

rustfmt --edition 2024 "${changed[@]}"

while IFS= read -r file; do
    [ -n "$file" ] || continue
    if [ -z "${in_change[$file]:-}" ] && [ -z "${dirty_before[$file]:-}" ]; then
        git checkout --quiet -- "$file"
    fi
done < <(git diff --name-only)

printf '%s\n' "${changed[@]}"
