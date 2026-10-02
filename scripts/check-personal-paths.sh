#!/bin/sh
# Guard: tracked files (outside thoughts/) must not contain a personal home
# path / account name or a public IP literal.
#
# Why: the private rsi-cli export preserves exact source bytes; before any
# public visibility those must be portable (Issue #730). Use $HOME, ~/,
# repo-relative paths, or placeholders (remote.example.net, 203.0.113.0/24).
#
# Catches: the maintainer's account/home names and any globally-routable IPv4
# literal. Private, loopback, link-local, CGNAT (100.64/10) and documentation
# ranges are fine. `thoughts/` is historical and skipped.
#
# Allowlist (in tools/check_personal_paths.py, intentionally tiny): GitHub
# repository slugs (github.com/<owner>/...) and the public resolver 8.8.8.8.
#
# Usage: scripts/check-personal-paths.sh [paths...]   (default: all tracked files)
# Exit 1 on any finding.

set -u
ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$ROOT" || exit 2
exec python3 tools/check_personal_paths.py "$@"
