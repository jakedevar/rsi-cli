#!/usr/bin/env bash
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
SRC_DIR="$ROOT/.codex/prompts"
CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
DST_DIR="$CODEX_HOME/prompts"

usage() {
  echo "usage: scripts/sync-codex-prompts.sh [--check]" >&2
}

check_one() {
  local src="$1"
  local dst="$2"

  if [[ ! -f "$dst" ]]; then
    echo "missing: $dst"
    return 1
  fi

  if ! cmp -s "$src" "$dst"; then
    echo "out of sync: $dst"
    return 1
  fi
}

copy_one() {
  local src="$1"
  local dst="$2"
  local dir

  dir="$(dirname "$dst")"

  if [[ -e "$dst" && ! -w "$dst" ]]; then
    echo "skipped read-only file: $dst" >&2
    return 1
  fi

  mkdir -p "$dir" 2>/dev/null || true

  if [[ ! -w "$dir" ]]; then
    echo "skipped read-only directory: $dir" >&2
    return 1
  fi

  cp "$src" "$dst"
  echo "synced ${dst#$HOME/}"
}

mode="${1:-sync}"
if [[ "$mode" != "sync" && "$mode" != "--check" ]]; then
  usage
  exit 2
fi

shopt -s nullglob
prompts=("$SRC_DIR"/*.md)

if [[ ${#prompts[@]} -eq 0 ]]; then
  echo "no Codex prompts found in $SRC_DIR" >&2
  exit 1
fi

status=0
for src in "${prompts[@]}"; do
  dst="$DST_DIR/$(basename "$src")"
  if [[ "$mode" == "--check" ]]; then
    check_one "$src" "$dst" || status=1
  else
    copy_one "$src" "$dst" || status=1
  fi
done

if [[ "$mode" == "--check" && "$status" -eq 0 ]]; then
  echo "Codex prompts are in sync"
fi

exit "$status"
