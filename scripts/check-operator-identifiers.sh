#!/bin/sh
# Guard: added lines must not contain the operator's personal identifiers
# (e-mail, phone, address). See tools/check_operator_identifiers.py (#1454).
#
# Usage: scripts/check-operator-identifiers.sh --staged | --range A..B
set -u
ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$ROOT" || exit 2
exec python3 tools/check_operator_identifiers.py "$@"
