#!/usr/bin/env bash
# Removes workflow rows with titles starting with '/' (raw session prompts
# captured as workflows by a now-disabled feature). Idempotent.
#
# Defense-in-depth: never deletes a row a session points at via
# sessions.workflow_id, even if the title is junk.
set -euo pipefail

DB="${RSI_DB:-$HOME/.rsi/rsi.db}"

if [[ ! -f "$DB" ]]; then
    echo "RSI DB not found at $DB" >&2
    exit 1
fi

BEFORE=$(sqlite3 "$DB" "SELECT COUNT(*) FROM workflows WHERE title LIKE '/%';")

sqlite3 "$DB" <<SQL
DELETE FROM workflows
WHERE title LIKE '/%'
   AND id NOT IN (
     SELECT DISTINCT workflow_id FROM sessions WHERE workflow_id IS NOT NULL
   );
SQL

AFTER=$(sqlite3 "$DB" "SELECT COUNT(*) FROM workflows WHERE title LIKE '/%';")
DELETED=$((BEFORE - AFTER))

echo "Deleted $DELETED junk workflow rows ($BEFORE -> $AFTER remaining)."
