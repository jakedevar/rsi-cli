#!/usr/bin/env bash
# render-project-index.sh
#
# Regenerates the `Status` column of a project index table from the
# canonical source of truth: each ticket file's YAML frontmatter
# `status:` key.
#
# This is the "single source of truth + rendered cache" design that
# replaced the dual-write pattern previously embedded in
# master_implement.md (see commit 507aa5d7 for the design that this
# script now obsoletes).
#
# USAGE:
#   scripts/render-project-index.sh <project-dir>
#
# EXAMPLES:
#   scripts/render-project-index.sh thoughts/shared/projects/session-views-redesign
#
# The script reads every `<project-dir>/tickets/*.md` file, extracts
# its `id` and `status` from YAML frontmatter, then rewrites the
# matching row in `<project-dir>/index.md` so the Status column
# reflects the current ticket frontmatter. Rows in the index that
# don't have a matching ticket file are left untouched. Tickets
# without a row in the index emit a warning but don't fail the run.
#
# The script is idempotent — running twice in a row is a no-op.
# Exit non-zero only on usage errors or file-system failures.

set -euo pipefail

PROJECT_DIR="${1:-}"

if [[ -z "$PROJECT_DIR" || ! -d "$PROJECT_DIR" ]]; then
    echo "usage: $0 <project-dir>" >&2
    echo "  <project-dir> must contain a tickets/ subdirectory and an index.md" >&2
    exit 2
fi

if [[ ! -d "$PROJECT_DIR/tickets" ]]; then
    echo "$PROJECT_DIR/tickets/ does not exist" >&2
    exit 2
fi

INDEX="$PROJECT_DIR/index.md"
if [[ ! -f "$INDEX" ]]; then
    echo "$INDEX does not exist" >&2
    exit 2
fi

declare -A STATUS_MAP

# Parse each ticket file's YAML frontmatter
shopt -s nullglob
for ticket_file in "$PROJECT_DIR"/tickets/*.md; do
    id=""
    status=""
    in_fm=false
    fm_done=false

    while IFS= read -r line; do
        $fm_done && break
        if [[ "$line" == "---" ]]; then
            if $in_fm; then
                fm_done=true
                continue
            else
                in_fm=true
                continue
            fi
        fi
        $in_fm || continue
        case "$line" in
            id:*)
                id="${line#id:}"
                id="${id#"${id%%[![:space:]]*}"}"
                id="${id%"${id##*[![:space:]]}"}"
                ;;
            status:*)
                status="${line#status:}"
                status="${status#"${status%%[![:space:]]*}"}"
                status="${status%"${status##*[![:space:]]}"}"
                ;;
        esac
    done < "$ticket_file"

    if [[ -n "$id" && -n "$status" ]]; then
        STATUS_MAP["$id"]="$status"
    fi
done

if [[ ${#STATUS_MAP[@]} -eq 0 ]]; then
    echo "no tickets with id+status frontmatter found under $PROJECT_DIR/tickets/" >&2
    exit 0
fi

# Rewrite the Status column in index.md table rows.
# A row that opens with `| <TICKET-ID> |` is rewritten so the last
# pipe-delimited cell (the Status column) carries the ticket's
# current frontmatter status.
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

awk -v status_keys="${!STATUS_MAP[*]}" '
BEGIN {
    n = split(status_keys, keys, " ");
    for (i = 1; i <= n; i++) {
        status_lookup[keys[i]] = 1;
    }
}
{
    # Match table rows that begin with `| <TICKET-ID> |`
    # Capture the ticket id (e.g. SVR-001, RSI-021).
    if (match($0, /^\|[[:space:]]*[A-Z]+-[0-9]+[[:space:]]*\|/)) {
        row = $0;
        # Pull out the ticket id without modifying the rest.
        n_id = split(row, parts, "|");
        ticket_id = parts[2];
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", ticket_id);

        if (ticket_id in status_lookup) {
            # The Status column is the last non-empty cell.
            # parts[N] is the trailing empty cell after the closing pipe,
            # so the Status cell is parts[N-1]. Replace it, preserving
            # the leading single space.
            new_status = ENVIRON["STATUS_FOR_" ticket_id];
            if (new_status != "") {
                parts[n_id - 1] = " " new_status " ";
                row = parts[1];
                for (i = 2; i <= n_id; i++) {
                    row = row "|" parts[i];
                }
            }
        }
        print row;
        next;
    }
    print;
}
' "$INDEX" > "$TMP" 2>/dev/null || true

# Export the status map as environment variables so awk can look them up.
# (Awk's ENVIRON requires environment-variable access; we set each one before
# running awk, which we cannot do in a pipeline — so we rerun awk with env.)
ENV_ARGS=()
for ticket_id in "${!STATUS_MAP[@]}"; do
    ENV_ARGS+=("STATUS_FOR_${ticket_id}=${STATUS_MAP[$ticket_id]}")
done

env "${ENV_ARGS[@]}" awk -v status_keys="${!STATUS_MAP[*]}" '
BEGIN {
    n = split(status_keys, keys, " ");
    for (i = 1; i <= n; i++) {
        status_lookup[keys[i]] = 1;
    }
}
{
    if (match($0, /^\|[[:space:]]*[A-Z]+-[0-9]+[[:space:]]*\|/)) {
        row = $0;
        n_id = split(row, parts, "|");
        ticket_id = parts[2];
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", ticket_id);

        if (ticket_id in status_lookup) {
            new_status = ENVIRON["STATUS_FOR_" ticket_id];
            if (new_status != "") {
                parts[n_id - 1] = " " new_status " ";
                row = parts[1];
                for (i = 2; i <= n_id; i++) {
                    row = row "|" parts[i];
                }
            }
        }
        print row;
        next;
    }
    print;
}
' "$INDEX" > "$TMP"

if ! cmp -s "$INDEX" "$TMP"; then
    mv "$TMP" "$INDEX"
    echo "updated: $INDEX"
else
    echo "no changes: $INDEX"
fi
