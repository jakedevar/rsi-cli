#!/usr/bin/env bash
# Generates and executes ArchiveSession RPCs for every visible session not in keep_sessions.txt.
#
# Usage:
#   scripts/cleanup/archive_sessions.sh --dry-run   # prints planned RPC calls and snapshot path
#   scripts/cleanup/archive_sessions.sh             # executes (with one y/N confirmation)
#
# Source: thoughts/shared/plans/2026-05-11-session-archive-and-branch-cleanup.md §2

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
KEEP_FILE="$ROOT/scripts/cleanup/keep_sessions.txt"
RPC_BIN="$ROOT/target/debug/rsi-rpc"
DB="$HOME/.rsi/rsi.db"

[[ -x "$RPC_BIN" ]] || { echo "rsi-rpc not built; cargo build -p rsi-common --bin rsi-rpc"; exit 1; }
pgrep -x rsid >/dev/null || { echo "rsid not running; start the daemon first"; exit 1; }
[[ -f "$KEEP_FILE" ]] || { echo "keep_sessions.txt missing at $KEEP_FILE"; exit 1; }

# Snapshot before mutation (for rollback)
SNAPSHOT="$ROOT/scripts/cleanup/snapshot-$(date +%Y%m%d-%H%M%S).txt"
sqlite3 "$DB" "SELECT id||','||status||','||datetime(created_at) FROM sessions WHERE status NOT IN ('Archived','Deleted') AND COALESCE(session_kind,'Standard') != 'TaskRabbit' AND scheduled_job_id IS NULL ORDER BY created_at" > "$SNAPSHOT"
echo "Pre-archive snapshot: $SNAPSHOT ($(wc -l < "$SNAPSHOT") sessions)"

# Compute archive list = visible - keep
KEEP=$(grep -Ev '^\s*(#|$)' "$KEEP_FILE" | sort -u)
VISIBLE=$(awk -F, '{print $1}' "$SNAPSHOT" | sort -u)
TO_ARCHIVE=$(comm -23 <(echo "$VISIBLE") <(echo "$KEEP"))
COUNT=$(echo "$TO_ARCHIVE" | grep -c . || echo 0)

echo "About to archive $COUNT sessions. First 10:"
echo "$TO_ARCHIVE" | head -10
echo "..."

if [[ "${1:-}" == "--dry-run" ]]; then
  echo "Dry run — exiting before RPCs"
  exit 0
fi

read -r -p "Proceed with $COUNT ArchiveSession RPCs? (y/N) " ans
[[ "$ans" == "y" ]] || { echo "Aborted"; exit 1; }

FAILED=()
ARCHIVED=0
while IFS= read -r sid; do
  [[ -z "$sid" ]] && continue
  if "$RPC_BIN" ArchiveSession --params "{\"session_id\":\"$sid\"}" >/dev/null 2>&1; then
    echo "archived $sid"
    ARCHIVED=$((ARCHIVED + 1))
  else
    echo "FAILED $sid"
    FAILED+=("$sid")
  fi
done <<< "$TO_ARCHIVE"

echo "---"
echo "Archived: $ARCHIVED"
if (( ${#FAILED[@]} > 0 )); then
  echo "${#FAILED[@]} failures:"
  printf '  %s\n' "${FAILED[@]}"
  exit 2
fi
echo "Done. Post-archive visible session count:"
sqlite3 "$DB" "SELECT COUNT(*) FROM sessions WHERE status NOT IN ('Archived','Deleted') AND COALESCE(session_kind,'Standard') != 'TaskRabbit' AND scheduled_job_id IS NULL"
