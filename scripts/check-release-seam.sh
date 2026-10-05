#!/usr/bin/env bash
# Refuse a release build whose normal/build dependency graph enables a test
# seam (#1021 S4). `test-seam` carries fake capabilities and route-validation
# bypasses and is meant to reach only test builds, through the dev-dependency
# self edges. The crates' build scripts also refuse it in the release profile;
# this runs before the build so a release install fails fast and loudly.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
cargo_command="${CHECK_RELEASE_SEAM_CARGO:-cargo}"

if ! tree="$($cargo_command tree --workspace --edges normal,build --prefix none --target all --format '{p} {f}' 2>/dev/null)"; then
    echo "check-release-seam: cannot read the dependency graph with $cargo_command tree" >&2
    exit 2
fi
offenders="$(printf '%s\n' "$tree" | grep -E '(^|[ ,])test-seam([ ,]|$)' | sort -u || true)"
if [[ -n "$offenders" ]]; then
    echo "check-release-seam: test-seam is enabled in the release dependency graph:" >&2
    printf '  %s\n' "$offenders" >&2
    echo "Release builds must not compile the test seam; remove the feature from the non-dev dependency edge." >&2
    exit 1
fi
echo "check-release-seam: ok (no test-seam in the normal or build dependency graph)"
