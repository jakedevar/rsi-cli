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
chmod +x tools/git-hooks/pre-commit tools/git-hooks/pre-merge-commit tools/git-hooks/commit-msg tools/git-hooks/pre-push
git config --local core.hooksPath tools/git-hooks
# Set explicitly: Git can implicitly enable rerere when rr-cache already exists.
# Leave the shared cache intact and inert; never delete recorded resolutions.
git config --local --replace-all rerere.enabled false
echo "Installed: core.hooksPath -> tools/git-hooks (pre-commit, pre-merge-commit, commit-msg and pre-push active across all worktrees); shared rerere disabled"
