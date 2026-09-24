#!/usr/bin/env bash
#
# Phase 4.1 — single-agent audit dispatch.
#
# Invoked by .github/workflows/audit-weekly.yml once per agent slug. Each
# invocation reads the corresponding Phase 1 audit prompt template from
# thoughts/shared/research/audit-prompts/<slug>.md (if present) or falls
# back to a placeholder report when ANTHROPIC_API_KEY is unset.
#
# Required env (passed by workflow):
#   AGENT_SLUG  — one of: edge-cases | invariants | error-paths |
#                  cross-crate | census | stale-branches
#   AGENT_MODEL — Anthropic model id (e.g. claude-sonnet-5)
#   STAMP       — run identifier, format YYYY-WWNN
#   ANTHROPIC_API_KEY — optional. If absent, write a placeholder report.
#
# Output:
#   thoughts/shared/research/<STAMP>-hardening-audit-<slug>.md

set -euo pipefail

: "${AGENT_SLUG:?AGENT_SLUG required}"
: "${AGENT_MODEL:?AGENT_MODEL required}"
: "${STAMP:?STAMP required}"

OUT="thoughts/shared/research/${STAMP}-hardening-audit-${AGENT_SLUG}.md"
PROMPT="thoughts/shared/research/audit-prompts/${AGENT_SLUG}.md"
HEAD_SHA="$(git rev-parse HEAD)"

mkdir -p "$(dirname "$OUT")"

write_placeholder() {
  local reason="$1"
  cat > "$OUT" <<EOF
---
audit_phase: 4
type: weekly-audit-placeholder
slug: ${AGENT_SLUG}
stamp: ${STAMP}
audited_head: ${HEAD_SHA}
generated_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)
---

# ${AGENT_SLUG^^} Audit — ${STAMP} (placeholder)

> ${reason}

## Pre-existing backlog

(no agent run available; refer to the most recent successful weekly report
for the prior backlog snapshot).
EOF
}

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "[!] ANTHROPIC_API_KEY not set — writing placeholder report"
  write_placeholder "ANTHROPIC_API_KEY secret not configured. The cron job ran but no agent was dispatched. Set the secret in GitHub Settings → Secrets and re-run via workflow_dispatch."
  exit 0
fi

if [ ! -f "$PROMPT" ]; then
  echo "[!] no agent prompt template at $PROMPT — writing placeholder"
  write_placeholder "Agent prompt template missing at \`$PROMPT\`. Add the per-slug prompt files from the Phase 1 dispatch (mirroring \`thoughts/shared/research/2026-04-26-hardening-audit-${AGENT_SLUG}.md\` instruction style) to enable real agent runs."
  exit 0
fi

# Real dispatch path — calls the Anthropic Messages API with the prompt body
# and stores the response body to the output file. The exact CLI/SDK is
# pluggable; the simplest portable option is curl + jq.
echo "[*] dispatching agent ($AGENT_SLUG, $AGENT_MODEL)"

PROMPT_BODY=$(cat "$PROMPT")
REQUEST=$(jq -n \
  --arg model "$AGENT_MODEL" \
  --arg system "You are a hardening audit agent. Re-run the assigned audit and emit a markdown report following the canonical Phase 1 structure. Audited HEAD: $HEAD_SHA." \
  --arg user "$PROMPT_BODY" \
  '{
    model: $model,
    max_tokens: 8192,
    system: $system,
    messages: [{ role: "user", content: $user }]
  }')

RESPONSE=$(curl -sS https://api.anthropic.com/v1/messages \
  -H "x-api-key: ${ANTHROPIC_API_KEY}" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d "$REQUEST" || true)

if [ -z "$RESPONSE" ]; then
  echo "[!] empty response from Anthropic API"
  write_placeholder "Anthropic API call returned empty response. Check rate limits / network reachability."
  exit 0
fi

# Extract the assistant's text content. content[0].text is the standard shape.
TEXT=$(echo "$RESPONSE" | jq -r '.content[0].text // empty')
if [ -z "$TEXT" ]; then
  echo "[!] could not extract content from API response — writing placeholder"
  ERR=$(echo "$RESPONSE" | jq -r '.error.message // "unknown error"')
  write_placeholder "Anthropic API returned no content. Error: $ERR"
  exit 0
fi

cat > "$OUT" <<EOF
---
audit_phase: 4
type: weekly-audit
slug: ${AGENT_SLUG}
stamp: ${STAMP}
audited_head: ${HEAD_SHA}
generated_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)
model: ${AGENT_MODEL}
---

EOF
echo "$TEXT" >> "$OUT"

echo "[✓] wrote $OUT"
