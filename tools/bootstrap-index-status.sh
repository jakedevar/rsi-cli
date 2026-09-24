#!/usr/bin/env bash
# bootstrap-index-status.sh — seed INDEX.status.json from an INDEX.md table.
#
# Usage: ./tools/bootstrap-index-status.sh <project> <path-to-INDEX.md>
#
# Parses ticket IDs and status strings from the INDEX.md table (permissive
# regex), infers IndexStatusValue, and calls UpdateIndexStatus via rsi-rpc.
#
# Status inference (case-insensitive, first match wins):
#   shipped / done / complete  → shipped
#   in_progress / in progress  → in_progress
#   blocked                    → blocked
#   ready                      → ready
#   (default)                  → not_started
#
# Requirements:
#   - A running rsid daemon (rsi-rpc talks to ~/.rsi/daemon.sock)
#   - cargo-built rsi-rpc binary at $RSI_RPC_BIN (default: repo-root/target/debug/rsi-rpc)
#   - jq (for pretty-printing verification output)

set -euo pipefail

PROJECT="${1:?usage: $0 <project> <INDEX.md>}"
INDEX="${2:?usage: $0 <project> <INDEX.md>}"

if [[ ! -f "${INDEX}" ]]; then
    echo "ERROR: INDEX.md not found at: ${INDEX}" >&2
    exit 1
fi

REPO_ROOT="$(git -C "$(dirname "${INDEX}")" rev-parse --show-toplevel 2>/dev/null || git rev-parse --show-toplevel)"
RPC_BIN="${RSI_RPC_BIN:-${REPO_ROOT}/target/debug/rsi-rpc}"

if [[ ! -x "${RPC_BIN}" ]]; then
    echo "ERROR: rsi-rpc binary not found or not executable at: ${RPC_BIN}" >&2
    echo "       Build it first: cargo build -p rsi-common --bin rsi-rpc" >&2
    exit 1
fi

# infer_status <raw-text>
# Outputs one of: shipped, in_progress, blocked, ready, not_started
infer_status() {
    local text="${1,,}"  # lowercase
    if   [[ "${text}" =~ shipped|done|complete ]]; then
        echo "shipped"
    elif [[ "${text}" =~ in_progress|in\ progress ]]; then
        echo "in_progress"
    elif [[ "${text}" =~ blocked ]]; then
        echo "blocked"
    elif [[ "${text}" =~ ready ]]; then
        echo "ready"
    else
        echo "not_started"
    fi
}

echo "Bootstrapping INDEX.status.json for project: ${PROJECT}"
echo "Reading: ${INDEX}"
echo ""

count=0
while IFS= read -r line; do
    # Match markdown table rows that contain a ticket ID like P1.1, P2.3, etc.
    if [[ "${line}" =~ \|[[:space:]]*(P[0-9]+\.[0-9]+)[[:space:]]*\| ]]; then
        ticket_id="${BASH_REMATCH[1]}"
        status_text=$(infer_status "${line}")

        echo "  ${ticket_id} → ${status_text}"

        "${RPC_BIN}" UpdateIndexStatus "$(cat <<PARAMS
{
    "project": "${PROJECT}",
    "ticket_id": "${ticket_id}",
    "status": "${status_text}"
}
PARAMS
)"
        count=$((count + 1))
    fi
done < "${INDEX}"

echo ""
echo "Done — ${count} ticket(s) written."
echo ""

# Verify the resulting sidecar
SIDECAR_PATH="${REPO_ROOT}/thoughts/shared/projects/${PROJECT}/INDEX.status.json"
if [[ -f "${SIDECAR_PATH}" ]]; then
    echo "Sidecar contents:"
    jq . "${SIDECAR_PATH}"
else
    echo "WARNING: sidecar not found at expected path: ${SIDECAR_PATH}" >&2
fi
