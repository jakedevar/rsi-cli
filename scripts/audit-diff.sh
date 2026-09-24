#!/usr/bin/env bash
#
# Phase 4.2 — weekly audit diff machinery.
#
# Compares this week's six audit reports against the most recent prior
# weekly snapshot (or the Phase 1 baseline if no prior weekly run exists)
# and emits a single markdown delta summary at:
#
#   thoughts/shared/research/<STAMP>-hardening-audit-diff.md
#
# Counts new (+) and resolved (-) lines in each report's "Pre-existing
# backlog" section, plus a small excerpt of net new findings.
#
# Usage:
#   ./scripts/audit-diff.sh <STAMP>
# where STAMP is the YYYY-WWNN identifier for the current run.

set -euo pipefail

if [ $# -lt 1 ]; then
  echo "usage: $0 <STAMP>" >&2
  exit 2
fi
STAMP="$1"

RESEARCH_DIR="thoughts/shared/research"
SLUGS=(edge-cases invariants error-paths cross-crate census stale-branches)
CURRENT_HEAD="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
OUT="${RESEARCH_DIR}/${STAMP}-hardening-audit-diff.md"

mkdir -p "$RESEARCH_DIR"

# Locate the prior week's report for a given slug. Prefer the most recent
# weekly report (matches `*-WW*-hardening-audit-<slug>.md`), fall back to
# the Phase 1 baseline (the canonical 2026-04-26 reports) when no prior
# weekly exists.
find_prior() {
  local slug="$1"
  local current="${RESEARCH_DIR}/${STAMP}-hardening-audit-${slug}.md"
  # All matching weekly reports, sorted newest-first by name.
  local candidates
  candidates=$(ls "$RESEARCH_DIR"/*-WW*-hardening-audit-"${slug}".md 2>/dev/null | sort -r || true)
  for c in $candidates; do
    if [ "$c" != "$current" ]; then
      echo "$c"
      return 0
    fi
  done
  # Fall back to the Phase 1 baseline if it exists.
  if [ -f "${RESEARCH_DIR}/2026-04-26-hardening-audit-${slug}.md" ]; then
    echo "${RESEARCH_DIR}/2026-04-26-hardening-audit-${slug}.md"
    return 0
  fi
  return 1
}

# Compute count of net-new / resolved lines for one slug.
count_diff() {
  local prior="$1"
  local current="$2"
  local new_count=0
  local resolved_count=0
  if [ -f "$prior" ] && [ -f "$current" ]; then
    new_count=$(diff -u "$prior" "$current" 2>/dev/null | grep -c '^+[^+]' || true)
    resolved_count=$(diff -u "$prior" "$current" 2>/dev/null | grep -c '^-[^-]' || true)
  fi
  echo "$new_count $resolved_count"
}

# ── Header ──────────────────────────────────────────────────────────────────
PRIOR_WEEK="(none)"
if [ -f "${RESEARCH_DIR}/$(ls "$RESEARCH_DIR" 2>/dev/null | grep -E '^.+-WW[0-9]+-hardening-audit-edge-cases\.md$' | grep -v "^${STAMP}-" | sort -r | head -1)" ] 2>/dev/null; then
  PRIOR_WEEK=$(ls "$RESEARCH_DIR" 2>/dev/null \
    | grep -E '^.+-WW[0-9]+-hardening-audit-edge-cases\.md$' \
    | grep -v "^${STAMP}-" \
    | sort -r | head -1 \
    | sed 's/-hardening-audit-edge-cases\.md//')
fi

cat > "$OUT" <<EOF
---
audit_phase: 4
type: weekly-diff
prior_week: ${PRIOR_WEEK}
current_week: ${STAMP}
audited_head: ${CURRENT_HEAD}
generated_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)
---

# Hardening Audit Diff — ${STAMP}

| Audit | New (+) | Resolved (−) | Net |
|-------|---------|--------------|-----|
EOF

TOTAL_NEW=0
TOTAL_RES=0

for slug in "${SLUGS[@]}"; do
  current="${RESEARCH_DIR}/${STAMP}-hardening-audit-${slug}.md"
  prior=""
  if find_prior "$slug" >/dev/null 2>&1; then
    prior=$(find_prior "$slug")
  fi

  if [ -z "$prior" ] || [ ! -f "$current" ]; then
    printf "| %s | — | — | — |\n" "$slug" >> "$OUT"
    continue
  fi

  read -r NEW RES <<< "$(count_diff "$prior" "$current")"
  NET=$((NEW - RES))
  TOTAL_NEW=$((TOTAL_NEW + NEW))
  TOTAL_RES=$((TOTAL_RES + RES))
  printf "| %s | %d | %d | %d |\n" "$slug" "$NEW" "$RES" "$NET" >> "$OUT"
done

cat >> "$OUT" <<EOF
| **TOTAL** | **${TOTAL_NEW}** | **${TOTAL_RES}** | **$((TOTAL_NEW - TOTAL_RES))** |

## New findings (this week)

EOF

for slug in "${SLUGS[@]}"; do
  current="${RESEARCH_DIR}/${STAMP}-hardening-audit-${slug}.md"
  if ! find_prior "$slug" >/dev/null 2>&1; then continue; fi
  prior=$(find_prior "$slug")
  if [ ! -f "$current" ]; then continue; fi

  echo "### ${slug}" >> "$OUT"
  echo >> "$OUT"
  echo '```diff' >> "$OUT"
  diff -u "$prior" "$current" 2>/dev/null | grep '^+[^+]' | head -30 >> "$OUT" || true
  echo '```' >> "$OUT"
  echo >> "$OUT"
done

cat >> "$OUT" <<EOF

## Resolved findings (this week)

EOF

for slug in "${SLUGS[@]}"; do
  current="${RESEARCH_DIR}/${STAMP}-hardening-audit-${slug}.md"
  if ! find_prior "$slug" >/dev/null 2>&1; then continue; fi
  prior=$(find_prior "$slug")
  if [ ! -f "$current" ]; then continue; fi

  echo "### ${slug}" >> "$OUT"
  echo >> "$OUT"
  echo '```diff' >> "$OUT"
  diff -u "$prior" "$current" 2>/dev/null | grep '^-[^-]' | head -30 >> "$OUT" || true
  echo '```' >> "$OUT"
  echo >> "$OUT"
done

echo "[✓] wrote $OUT"
echo "    new: $TOTAL_NEW    resolved: $TOTAL_RES    net: $((TOTAL_NEW - TOTAL_RES))"
