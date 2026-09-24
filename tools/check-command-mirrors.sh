#!/bin/sh
# CI entrypoint for the single Claude-canonical command mirror pipeline.
set -eu

ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
exec "$ROOT/scripts/sync-agent-commands.sh" --check "$@"
