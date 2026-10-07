#!/bin/sh
# Guard theme-sensitive tests against concurrent active-theme changes (#1206).
# Usage: scripts/check-theme-test-guards.sh [paths...] (default: crates/rsi/src)
set -eu
ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$ROOT"
exec python3 tools/check_theme_test_guards.py "$@"
