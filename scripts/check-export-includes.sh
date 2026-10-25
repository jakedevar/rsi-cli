#!/bin/sh
# Guard: no Rust include_str!/include_bytes!/include!/#[path] may reach a path
# that scripts/export-rsi-cli.sh excludes from the public tree (Issue #1630).
exec "$(dirname "$0")/repo-guard" export-includes "$@"
