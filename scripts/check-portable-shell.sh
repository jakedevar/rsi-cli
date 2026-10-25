#!/bin/sh
# Guard: no new non-portable shell in scripts/ (BSD-only and GNU-only forms).
#
# Scripts run on the Linux hub and on the macOS satellite (bash 3.2, BSD userland).
# This flags `mktemp -t` without X's, `sed -i ''` and bare `sed -i`, `stat -f` /
# `stat -c`, `date -j` / `date -d`, `readlink -f`, `grep -P`, `xargs -r` and bash 4+
# features (`declare -A`, `mapfile`, `${var,,}`, `wait -n`), and names the portable
# alternative for each. A `uname` branch or a `# portability: linux-only` /
# `# portability: macos-only` marker allows a form; existing uses are counted per
# file in tools/portable-shell-baseline.json, which may only go down.
#
# Usage: scripts/check-portable-shell.sh [--base REF] [paths...]   (default: scripts/)
#        scripts/repo-guard portable-shell --refresh                (lower the baseline)
# Exit 1 on any finding above the baseline.

set -u
ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$ROOT" || exit 2
exec "$ROOT/scripts/repo-guard" portable-shell "$@"
