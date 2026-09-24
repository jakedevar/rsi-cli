#!/usr/bin/env bash
#
# Phase 4.4 — stale-branch alarm.
#
# Lists branches whose tip commit is older than STALE_DAYS (default 60) and
# which have NOT been merged into main. Files a Linear issue per branch the
# first time it crosses the threshold; subsequent runs skip already-tracked
# branches via the dedup cache at thoughts/shared/research/stale-branch-issues.json.
#
# Branches whose latest commit message contains `wip-exempt` (case-insensitive)
# are skipped — a way to silence intentional long-running branches.
#
# Required env (gracefully degrade when absent):
#   LINEAR_API_KEY  — issues skipped if unset
#   LINEAR_TEAM_ID  — required by Linear API for issueCreate
#
# Usage: ./scripts/stale-branch-check.sh

set -euo pipefail

STALE_DAYS="${STALE_DAYS:-60}"
RESEARCH_DIR="thoughts/shared/research"
CACHE="${RESEARCH_DIR}/stale-branch-issues.json"
THRESHOLD_EPOCH=$(( $(date +%s) - STALE_DAYS * 86400 ))

mkdir -p "$RESEARCH_DIR"
[ -f "$CACHE" ] || echo '{}' > "$CACHE"

echo "[*] stale-branch check (threshold: ${STALE_DAYS} days)"

# Ensure we have all remote branches with up-to-date metadata.
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  git fetch --prune origin >/dev/null 2>&1 || true
else
  echo "[!] not a git checkout — exiting cleanly"
  exit 0
fi

NEW_ISSUES=0
SKIPPED_EXEMPT=0
SKIPPED_DEDUPED=0
SKIPPED_MERGED=0

# Iterate remote branches. `for-each-ref` emits one line per ref, suitable for
# `read` in the loop. The format `refname:short committerdate:unix` lets us
# filter and parse with no further forks.
while IFS=' ' read -r BRANCH BRANCH_EPOCH; do
  # Strip the origin/ prefix; skip HEAD pointer and main itself.
  case "$BRANCH" in
    origin/HEAD|origin/main)         continue ;;
    origin/*)                        BRANCH_NAME="${BRANCH#origin/}" ;;
    *)                               continue ;;
  esac

  # Fresh enough? skip silently.
  if [ "$BRANCH_EPOCH" -ge "$THRESHOLD_EPOCH" ]; then
    continue
  fi

  # Already merged into main? skip silently.
  if git merge-base --is-ancestor "$BRANCH" origin/main 2>/dev/null; then
    SKIPPED_MERGED=$((SKIPPED_MERGED + 1))
    continue
  fi

  # Latest commit message says wip-exempt? skip with telemetry.
  if git log -1 --format=%B "$BRANCH" 2>/dev/null | grep -iq 'wip-exempt'; then
    echo "    skip (wip-exempt): $BRANCH_NAME"
    SKIPPED_EXEMPT=$((SKIPPED_EXEMPT + 1))
    continue
  fi

  # Already tracked? skip.
  if jq -e --arg k "$BRANCH_NAME" 'has($k)' "$CACHE" >/dev/null 2>&1; then
    echo "    skip (already tracked): $BRANCH_NAME"
    SKIPPED_DEDUPED=$((SKIPPED_DEDUPED + 1))
    continue
  fi

  AGE_DAYS=$(( (BRANCH_EPOCH > 0 ? ($(date +%s) - BRANCH_EPOCH) : 0) / 86400 ))
  echo "    new stale: $BRANCH_NAME (${AGE_DAYS}d old)"

  # File Linear issue if API configured. Otherwise stash a "pending" marker.
  ISSUE_ID="(skipped — no Linear key)"
  if [ -n "${LINEAR_API_KEY:-}" ] && [ -n "${LINEAR_TEAM_ID:-}" ]; then
    TITLE="Stale branch: ${BRANCH_NAME}"
    DESC="Branch \`${BRANCH_NAME}\` last touched ${AGE_DAYS} days ago and is not yet merged into \`main\`. Add \`wip-exempt: <reason>\` to the latest commit message to silence this alarm, or rebase / close the branch."
    GQL=$(jq -n \
      --arg teamId "$LINEAR_TEAM_ID" \
      --arg title "$TITLE" \
      --arg description "$DESC" \
      '{
        query: "mutation IssueCreate($teamId: String!, $title: String!, $description: String!) { issueCreate(input: { teamId: $teamId, title: $title, description: $description }) { success issue { id identifier } } }",
        variables: { teamId: $teamId, title: $title, description: $description }
      }')
    RESP=$(curl -sS https://api.linear.app/graphql \
      -H "Authorization: ${LINEAR_API_KEY}" \
      -H "Content-Type: application/json" \
      -d "$GQL" 2>/dev/null || echo '{}')
    ISSUE_ID=$(echo "$RESP" | jq -r '.data.issueCreate.issue.identifier // empty')
    if [ -z "$ISSUE_ID" ]; then
      ISSUE_ID="(failed: $(echo "$RESP" | jq -r '.errors[0].message // "unknown"'))"
    fi
  fi

  TMP=$(mktemp)
  jq --arg k "$BRANCH_NAME" --arg v "$ISSUE_ID" '.[$k] = $v' "$CACHE" > "$TMP"
  mv "$TMP" "$CACHE"
  NEW_ISSUES=$((NEW_ISSUES + 1))

done < <(git for-each-ref --format='%(refname:short) %(committerdate:unix)' refs/remotes/origin/)

echo
echo "[✓] new stale issues: $NEW_ISSUES    deduped: $SKIPPED_DEDUPED    exempt: $SKIPPED_EXEMPT    merged: $SKIPPED_MERGED"
