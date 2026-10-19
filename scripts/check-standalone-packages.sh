#!/usr/bin/env bash
# Standalone `cargo check -p P --all-targets` for every workspace package (#2444).
# The merge gate checks the selected packages as one merged invocation (#2424),
# which unifies features: a crate that compiles only because another selected
# package enables one of its dependency features passes there. The full-library
# QA recipe (`make rolling-library-qa`) runs this to catch that. Every package is
# checked, a failure does not stop the sweep, and each is named as
# `standalone_check_failed:<package>` so the run is an ordinary QA red.
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
cargo_command="${CHECK_STANDALONE_CARGO:-cargo}"

if ! metadata="$($cargo_command metadata --no-deps --format-version 1)"; then
    echo "check-standalone-packages: cannot read the workspace with $cargo_command metadata" >&2
    exit 2
fi
packages="$(printf '%s' "$metadata" | python3 -I -c '
import json, sys
for package in sorted(p["name"] for p in json.load(sys.stdin)["packages"]):
    print(package)
')" || { echo "check-standalone-packages: cannot parse the workspace metadata" >&2; exit 2; }
if [[ -z "$packages" ]]; then
    echo "check-standalone-packages: the workspace has no packages" >&2
    exit 2
fi

status=0
while IFS= read -r package; do
    echo "check-standalone-packages: cargo check -p $package --all-targets"
    if ! $cargo_command check -p "$package" --all-targets; then
        echo "standalone_check_failed:$package" >&2
        status=1
    fi
done <<<"$packages"
exit "$status"
