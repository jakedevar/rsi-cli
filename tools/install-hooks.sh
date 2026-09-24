#!/bin/sh
# RSI-021 — Install version-controlled git hooks via core.hooksPath.
#
# `git config core.hooksPath` is stored in `.git/config`, which is shared
# across all worktrees of a single repo (per `man git-worktree`). Running
# this script once at the repo root activates the hooks for every existing
# and future worktree without per-worktree manual install steps.
#
# Idempotent — re-running just re-asserts the config and the +x bit.

set -e

cd "$(git rev-parse --show-toplevel)"
chmod +x tools/git-hooks/pre-commit tools/git-hooks/pre-push
git config core.hooksPath tools/git-hooks
echo "Installed: core.hooksPath -> tools/git-hooks (pre-commit and pre-push active across all worktrees)"
