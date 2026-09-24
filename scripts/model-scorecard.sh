#!/usr/bin/env bash
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
DB="${RSI_DB_PATH:-$HOME/.rsi/rsi.db}"
AS_OF='2026-09-22'
cd "$ROOT"
[[ -r "$DB" ]] || { printf 'database is not readable: %s\n' "$DB" >&2; exit 1; }
[[ -n "${RSI_SESSION_ID:-}" ]] || { printf 'RSI_SESSION_ID is required for author attribution\n' >&2; exit 1; }

# Every SQL statement printed in the scorecard is executed here against the read-only DB.
sql() {
  sqlite3 -readonly -header -separator $'\t' "$DB" "$1" |
    awk -F '\t' '{
      printf "|";
      for (i=1;i<=NF;i++) { gsub(/\|/, "\\\\|", $i); printf "%s|", $i; }
      printf "\n";
      if (NR==1) { printf "|"; for (i=1;i<=NF;i++) printf "---|"; printf "\n"; }
    }'
}
author="$(sqlite3 -readonly -separator ' / ' "$DB" "SELECT provider,COALESCE(model,'(NULL)') FROM sessions WHERE id='$RSI_SESSION_ID';")"
[[ -n "$author" ]] || { printf 'author session is absent from daemon DB\n' >&2; exit 1; }
sha="$(git rev-parse origin/rolling)"
generated="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"

cat <<EOF
# Model scorecard

## Refresh

- [observed] Generated: $generated; daemon DB read-only snapshot during generation.
- [source] \`origin/rolling\`: \`$sha\`; wave 1 starts $AS_OF UTC.
- [observed] Author model: $author.
- [source] Regenerate with \`bash scripts/model-scorecard.sh > thoughts/shared/manager/model-scorecard.md\`.
- [source] Exact executed SQL and git commands are in [the generator](../../../scripts/model-scorecard.sh). NULL is shown as (NULL); USD totals are not combined across session and invocation sources.

## Sessions

### Wave 1 (created_at >= $AS_OF)

[observed] Every model is retained. One row represents one provider, model, and effort.

EOF
sql "SELECT provider,COALESCE(model,'(NULL)') model,COALESCE(effort,'(NULL)') effort,
 count(*) sessions,sum(status='Completed') completed,sum(status='Failed') failed,
 sum(status='Interrupted') interrupted,sum(status IN ('Starting','Running','WaitingApproval')) active,
 sum(status IN ('Archived','Deleted')) archived_deleted
 FROM sessions WHERE created_at >= '$AS_OF'
 GROUP BY provider,model,effort ORDER BY sessions DESC,provider,model,effort;"
cat <<'EOF'

### All time

[observed] Model threshold is at least 10 sessions across efforts. Other includes every lower-volume provider/model combination in one row; its effort is aggregated.

EOF
SESSION_SCOPE="WITH model_counts AS (SELECT provider,model,count(*) n FROM sessions GROUP BY provider,model),
 scoped AS (SELECT CASE WHEN mc.n>=10 THEN s.provider ELSE 'other' END provider,
 CASE WHEN mc.n>=10 THEN COALESCE(s.model,'(NULL)') ELSE 'other' END model,
 CASE WHEN mc.n>=10 THEN COALESCE(s.effort,'(NULL)') ELSE '(all)' END effort,
 s.status,s.cost_usd,s.total_input_tokens,s.total_output_tokens,s.total_cache_creation_tokens,s.total_cache_read_tokens,
 s.stop_reason,s.terminal_reason
 FROM sessions s JOIN model_counts mc ON mc.provider=s.provider AND mc.model IS s.model)"
sql "$SESSION_SCOPE SELECT provider,model,effort,count(*) sessions,
 sum(status='Completed') completed,sum(status='Failed') failed,sum(status='Interrupted') interrupted,
 sum(status IN ('Starting','Running','WaitingApproval')) active,
 sum(status IN ('Archived','Deleted')) archived_deleted
 FROM scoped GROUP BY provider,model,effort ORDER BY sessions DESC,provider,model,effort;"

cat <<'EOF'

### Tracked model roster (wave 1)

[source] Historical list from the operator model policy of 2026-09-24T00:25Z; models with no wave-1 sessions show zeros. This table does not grant launch authorization; the 05:55Z directive supersedes that policy.

EOF
sql "WITH roster(provider,model) AS (VALUES ('Claude','claude-opus-5-5'),('Claude','claude-sonnet-5'),('Claude','claude-haiku-4-5-20251001'),
 ('Codex','gpt-6-sol'),('Codex','gpt-6-luna'),('Codex','gpt-6-astra'),('OpenRouter','z-ai/glm-5.3-flashx'),('OpenRouter','deepseek/deepseek-v4.1-flash'))
 SELECT r.provider,r.model,count(s.id) sessions,COALESCE(sum(s.status='Completed'),0) completed,
 COALESCE(sum(s.status IN ('Failed','Interrupted')),0) failed_interrupted,COALESCE(sum(s.status IN ('Archived','Deleted')),0) outcome_unknown,
 COALESCE(sum(s.status IN ('Running','Starting','WaitingApproval')),0) live,
 CASE WHEN COALESCE(sum(s.status IN ('Completed','Failed','Interrupted')),0)>0 THEN printf('%.0f%%',100.0*sum(s.status='Completed')/sum(s.status IN ('Completed','Failed','Interrupted'))) ELSE '-' END known_outcome_completion
 FROM roster r LEFT JOIN sessions s ON s.provider=r.provider AND s.model=r.model AND s.created_at>='$AS_OF'
 GROUP BY r.provider,r.model ORDER BY sessions DESC,r.provider,r.model;"

cat <<'EOF'

## Failures and causes

[observed] Failure share counts Failed and Interrupted sessions over all sessions. Archived/Deleted sessions have an unknown outcome (`outcome_unknown`), so a low failure share with many archived rows is NOT a high success rate; `known_outcome_completion` = Completed / (Completed + Failed + Interrupted). Cause coverage counts failed/interrupted sessions whose stop_reason or terminal_reason is present.

EOF
sql "$SESSION_SCOPE SELECT provider,model,effort,count(*) sessions,
 sum(status IN ('Failed','Interrupted')) failed_interrupted,
 printf('%.1f%%',100.0*sum(status IN ('Failed','Interrupted'))/count(*)) failure_share,
 sum(status='Completed') completed,
 sum(status IN ('Archived','Deleted')) outcome_unknown,
 CASE WHEN sum(status IN ('Completed','Failed','Interrupted'))>0 THEN printf('%.0f%%',100.0*sum(status='Completed')/sum(status IN ('Completed','Failed','Interrupted'))) ELSE '-' END known_outcome_completion,
 sum(status IN ('Failed','Interrupted') AND stop_reason IS NOT NULL) with_stop_reason,
 sum(status IN ('Failed','Interrupted') AND terminal_reason IS NOT NULL) with_terminal_reason
 FROM scoped GROUP BY provider,model,effort ORDER BY sessions DESC,provider,model,effort;"
cat <<'EOF'

[observed] Invocation failure fields are separate from session terminal state. Rows below include only non-success invocations; an absent error class is counted explicitly.

EOF
sql "WITH model_counts AS (SELECT provider,model,count(*) n FROM sessions GROUP BY provider,model),
 scoped AS (SELECT CASE WHEN COALESCE(mc.n,0)>=10 THEN COALESCE(i.provider,'(NULL)') ELSE 'other' END provider,
 CASE WHEN COALESCE(mc.n,0)>=10 THEN COALESCE(i.model,'(NULL)') ELSE 'other' END model,
 CASE WHEN COALESCE(mc.n,0)>=10 THEN COALESCE(i.effort,'(NULL)') ELSE '(all)' END effort,
 i.status,i.admission_status,i.error_class FROM model_invocations i
 LEFT JOIN model_counts mc ON mc.provider=i.provider AND mc.model IS i.model)
 SELECT provider,model,effort,count(*) non_success,
 sum(error_class IS NULL) null_error_class,
 sum(admission_status!='admitted') not_admitted
 FROM scoped WHERE status NOT IN ('completed','succeeded') OR error_class IS NOT NULL OR admission_status!='admitted'
 GROUP BY provider,model,effort ORDER BY non_success DESC,provider,model,effort;"

cat <<'EOF'

## Tokens, cost, and coverage

[observed] Session cost and tokens are summed from session fields. A missing count means at least one of input, output, or cost is NULL in that session. Other follows the all-time threshold above.

EOF
sql "$SESSION_SCOPE SELECT provider,model,effort,count(*) sessions,
 COALESCE(sum(total_input_tokens),0) input_tokens,COALESCE(sum(total_output_tokens),0) output_tokens,
 COALESCE(sum(total_cache_creation_tokens),0) cache_create,COALESCE(sum(total_cache_read_tokens),0) cache_read,
 printf('%.2f',COALESCE(sum(cost_usd),0)) cost_usd,
 sum(total_input_tokens IS NULL OR total_output_tokens IS NULL OR cost_usd IS NULL) incomplete_rows
 FROM scoped GROUP BY provider,model,effort ORDER BY sessions DESC,provider,model,effort;"
cat <<'EOF'

[observed] Invocation usage is a separate accounting surface. Confidence is summarized in scalar counts; no session and invocation costs are added together. Embedding invocations are excluded from every invocation sum: failed embedding-index rows copy the owning session's provider usage (Issue #700), which would double count it.

EOF
sql "WITH model_counts AS (SELECT provider,model,count(*) n FROM sessions GROUP BY provider,model),
 scoped AS (SELECT CASE WHEN COALESCE(mc.n,0)>=10 THEN COALESCE(i.provider,'(NULL)') ELSE 'other' END provider,
 CASE WHEN COALESCE(mc.n,0)>=10 THEN COALESCE(i.model,'(NULL)') ELSE 'other' END model,
 CASE WHEN COALESCE(mc.n,0)>=10 THEN COALESCE(i.effort,'(NULL)') ELSE '(all)' END effort,
 i.input_tokens,i.output_tokens,i.estimated_cost_usd,i.usage_confidence FROM model_invocations i
 LEFT JOIN model_counts mc ON mc.provider=i.provider AND mc.model IS i.model
 WHERE i.invocation_kind IS NOT 'embedding')
 SELECT provider,model,effort,count(*) invocations,COALESCE(sum(input_tokens),0) input_tokens,
 COALESCE(sum(output_tokens),0) output_tokens,printf('%.2f',COALESCE(sum(estimated_cost_usd),0)) estimated_usd,
 sum(input_tokens IS NULL OR output_tokens IS NULL OR estimated_cost_usd IS NULL) incomplete_rows,
 sum(usage_confidence='reported') reported_usage
 FROM scoped GROUP BY provider,model,effort ORDER BY invocations DESC,provider,model,effort;"

cat <<'EOF'

## Review verdicts and rework

[observed] An assignment is one review round. Missing receipts are counted without a verdict; received reviews are attributed to the author session, given reviews to the reviewer session.

EOF
sql "WITH reviews AS (
 SELECT 'received' direction,au.provider,au.model,au.effort,a.work_key,a.assignment_id,r.verdict,r.receipt_id,a.superseded_by_assignment_id
 FROM manager_review_assignments a LEFT JOIN manager_review_receipts r USING(assignment_id)
 LEFT JOIN sessions au ON au.id=a.author_session_id
 UNION ALL
 SELECT 'given',rv.provider,rv.model,rv.effort,a.work_key,a.assignment_id,r.verdict,r.receipt_id,a.superseded_by_assignment_id
 FROM manager_review_assignments a LEFT JOIN manager_review_receipts r USING(assignment_id)
 LEFT JOIN sessions rv ON rv.id=a.reviewer_session_id
 ), findings AS (SELECT receipt_id,count(*) n,sum(blocking) blocking FROM manager_review_findings GROUP BY receipt_id),
 model_counts AS (SELECT provider,model,count(*) n FROM sessions GROUP BY provider,model),
 scoped AS (SELECT reviews.*,findings.n finding_count,findings.blocking blocking_count,
 CASE WHEN COALESCE(mc.n,0)>=10 THEN COALESCE(reviews.provider,'(NULL)') ELSE 'other' END shown_provider,
 CASE WHEN COALESCE(mc.n,0)>=10 THEN COALESCE(reviews.model,'(NULL)') ELSE 'other' END shown_model,
 CASE WHEN COALESCE(mc.n,0)>=10 THEN COALESCE(reviews.effort,'(NULL)') ELSE '(all)' END shown_effort
 FROM reviews LEFT JOIN findings USING(receipt_id)
 LEFT JOIN model_counts mc ON mc.provider=reviews.provider AND mc.model IS reviews.model)
 SELECT direction,shown_provider provider,shown_model model,shown_effort effort,
 count(*) rounds,count(DISTINCT work_key) work_keys,sum(superseded_by_assignment_id IS NOT NULL) superseded,
 sum(verdict IS NULL) no_receipt,sum(verdict='Approved') approved,
 COALESCE(sum(finding_count),0) findings,COALESCE(sum(blocking_count),0) blocking
 FROM scoped GROUP BY direction,shown_provider,shown_model,shown_effort
 ORDER BY rounds DESC,direction,provider,model,effort;"

cat <<'EOF'

### Reviewer reliability (DB-native reviews, wave 1)

[source] One row per reviewer model over `manager_review_assignments` created since 2026-09-22.
Minutes run from assignment creation to the receipt, or to the reviewer session's last update when no receipt exists.
`no_receipt_failed` counts assignments whose reviewer session ended Failed/Interrupted without a receipt.

EOF

sql "SELECT s.provider,s.model,count(*) assignments,sum(r.receipt_id IS NOT NULL) receipts,
 sum(r.receipt_id IS NULL AND s.status IN ('Failed','Interrupted')) no_receipt_failed,
 sum(r.receipt_id IS NULL AND s.status NOT IN ('Failed','Interrupted')) no_receipt_other,
 printf('%.0f%%',100.0*sum(r.receipt_id IS NOT NULL)/count(*)) completion,
 printf('%.1f',avg((julianday(COALESCE(r.created_at,s.updated_at))-julianday(a.created_at))*1440)) avg_min,
 printf('%.1f',max((julianday(COALESCE(r.created_at,s.updated_at))-julianday(a.created_at))*1440)) max_min
 FROM manager_review_assignments a JOIN sessions s ON s.id=a.reviewer_session_id
 LEFT JOIN manager_review_receipts r ON r.assignment_id=a.assignment_id
 WHERE a.created_at >= '$AS_OF'
 GROUP BY s.provider,s.model ORDER BY assignments DESC,s.provider,s.model;"

cat <<'EOF'

### Reviewer calibration arms

[source] Sessions with `agent_role` `cal-reviewer-*` / `cal2-reviewer-*` (blind seeded-defect review; accuracy is graded in
`thoughts/shared/bench/reviewer-calibration/run-*-report.md`, not in the DB). Wall time is first to last event.

EOF

sql "SELECT s.agent_role,s.provider,s.model,s.status,
 printf('%.0f',(julianday(max(e.created_at))-julianday(min(e.created_at)))*86400) wall_s
 FROM sessions s JOIN conversation_events e ON e.session_id=s.id
 WHERE s.agent_role LIKE 'cal%-reviewer-%'
 GROUP BY s.id ORDER BY s.created_at,s.agent_role;"

cat <<'EOF'

## Landed on `origin/rolling`

[source] Git merge subjects since 2026-09-22 that name `rsi/<uuid>` are joined to the session UUID.
[inferred] A matched merge is strong evidence for its session model, but a merge can include work from other authors.
LANDED manager replies are a medium-confidence textual signal and can refer to the same delivery; counts are not deduplicated.

EOF
# Git merge subjects are read once; use the daemon's session rows for exact UUID attribution.
declare -A model_by_uuid merge_count reply_count
while IFS=$'\t' read -r sid provider model; do model_by_uuid["$sid"]="$provider / $model"; done \
  < <(sqlite3 -readonly -separator $'\t' "$DB" "SELECT id,provider,COALESCE(model,'(NULL)') FROM sessions;")
while IFS= read -r subject; do
  if [[ "$subject" =~ rsi/([0-9a-f-]{36}) ]]; then
    sid="${BASH_REMATCH[1]}"
    key="${model_by_uuid[$sid]:-unmatched / (NULL)}"
    merge_count["$key"]=$(( ${merge_count["$key"]:-0} + 1 ))
  fi
done < <(git log origin/rolling --merges --since="$AS_OF 00:00:00 +0000" --format='%s')
while IFS=$'\t' read -r provider model replies; do
  key="$provider / $model"
  reply_count["$key"]="$replies"
done < <(sqlite3 -readonly -separator $'\t' "$DB" "SELECT COALESCE(s.provider,'(NULL)'),COALESCE(s.model,'(NULL)'),count(*) FROM harness_manager_messages m JOIN sessions s ON s.id=m.sender_session_id WHERE m.created_at >= '$AS_OF' AND m.request_id IS NOT NULL AND (upper(ltrim(m.message)) LIKE 'LANDED %' OR upper(ltrim(m.message)) LIKE 'LANDED:%' OR instr(upper(m.message),char(10)||'LANDED ')>0 OR instr(upper(m.message),char(10)||'LANDED:')>0) GROUP BY s.provider,s.model;")
printf '| Provider | Model | Landed merges | LANDED replies | Confidence |\n|---|---|---:|---:|---|\n'
{ for key in "${!merge_count[@]}" "${!reply_count[@]}"; do printf '%s\n' "$key"; done; } | sort -u | while IFS= read -r key; do
  if [[ "$key" == 'unmatched / (NULL)' ]]; then note='unmatched merge UUID';
  elif [[ ${merge_count["$key"]:-0} -gt 0 ]]; then note='merge UUID high; reply text medium';
  else note='reply text medium'; fi
  printf '| %s | %s | %s | %s | %s |\n' "${key%% / *}" "${key#* / }" "${merge_count["$key"]:-0}" "${reply_count["$key"]:-0}" "$note"
done

cat <<'EOF'

## Time to land

[source] Issue lifecycle uses `issue_events` status_updated transitions: creation to the last Closed, and first InProgress to that Closed.
Percentiles use nearest-rank order statistics; durations are minutes. The InProgress count exposes start-time coverage.
[inferred] Created-to-Closed includes backlog waiting time and should not be read as active work time.

EOF
ISSUE_TIMES="WITH transitions AS (
 SELECT issue_id,min(CASE WHEN json_extract(request_json,'$.status')='InProgress' THEN occurred_at END) started,
 max(CASE WHEN json_extract(request_json,'$.status')='Closed' THEN occurred_at END) closed
 FROM issue_events WHERE operation='status_updated' GROUP BY issue_id),
 closed AS (SELECT i.id,i.created_at,t.started,t.closed,
 (unixepoch(t.closed)-unixepoch(i.created_at))/60.0 created_min,
 CASE WHEN t.started<=t.closed THEN (unixepoch(t.closed)-unixepoch(t.started))/60.0 END active_min
 FROM issues i JOIN transitions t ON t.issue_id=i.id WHERE t.closed IS NOT NULL)"
sql "$ISSUE_TIMES, durations AS (
 SELECT 'created_to_closed' metric,created_min minutes FROM closed
 UNION ALL SELECT 'inprogress_to_closed',active_min FROM closed WHERE active_min IS NOT NULL),
 ranked AS (SELECT metric,minutes,row_number() OVER(PARTITION BY metric ORDER BY minutes) rn,
 count(*) OVER(PARTITION BY metric) n FROM durations)
 SELECT metric,max(n) closed_or_covered,
 (SELECT count(*) FROM closed) closed_issues,
 (SELECT count(*) FROM closed WHERE active_min IS NOT NULL) with_inprogress,
 printf('%.1f',max(CASE WHEN rn=(n+1)/2 THEN minutes END)) p50_min,
 printf('%.1f',max(CASE WHEN rn=(9*n+9)/10 THEN minutes END)) p90_min
 FROM ranked GROUP BY metric ORDER BY metric;"
cat <<'EOF'

[source] Wave-1 Epic cost walks each owning Epic's full session subtree recursively and counts sessions created since 2026-09-22.
Per session, stored cost wins; otherwise invocation estimated costs are summed. Coverage is the share of subtree sessions with either source.
Cost per issue divides the subtree total by wave-1 Closed issues; it is an Epic-level allocation, not a causal cost for an individual issue.

EOF
sql "WITH RECURSIVE wave_closed AS (
 SELECT e.issue_id,e.owning_epic_id FROM issue_events e
 WHERE e.operation='status_updated' AND json_extract(e.request_json,'$.status')='Closed'
 AND e.occurred_at>='$AS_OF' AND e.owning_epic_id IS NOT NULL
 GROUP BY e.issue_id,e.owning_epic_id),
 epics AS (SELECT owning_epic_id,count(DISTINCT issue_id) closed_issues FROM wave_closed GROUP BY owning_epic_id),
 tree(epic_id,session_id) AS (
 SELECT owning_epic_id,owning_epic_id FROM epics
 UNION ALL SELECT tree.epic_id,s.id FROM sessions s JOIN tree ON s.parent_id=tree.session_id),
 invocation_cost AS (SELECT session_id,sum(estimated_cost_usd) usd FROM model_invocations WHERE invocation_kind IS NOT 'embedding' GROUP BY session_id),
 cost AS (SELECT tree.epic_id,s.id,s.status,COALESCE(s.cost_usd,ic.usd) usd
 FROM tree JOIN sessions s ON s.id=tree.session_id LEFT JOIN invocation_cost ic ON ic.session_id=s.id
 WHERE s.created_at>='$AS_OF')
 SELECT substr(epics.owning_epic_id,1,8) epic_id,COALESCE(root.title,'(untitled)') epic_title,
 epics.closed_issues,count(cost.id) subtree_sessions,
 sum(cost.id!=epics.owning_epic_id AND cost.status IN ('Failed','Interrupted')) failed_interrupted_children,
 printf('%.2f',COALESCE(sum(cost.usd),0)) subtree_usd,
 printf('%.2f',COALESCE(sum(cost.usd),0)/epics.closed_issues) usd_per_closed,
 printf('%.1f%%',100.0*count(cost.usd)/nullif(count(cost.id),0)) cost_coverage
 FROM epics JOIN sessions root ON root.id=epics.owning_epic_id
 LEFT JOIN cost ON cost.epic_id=epics.owning_epic_id
 GROUP BY epics.owning_epic_id,root.title,epics.closed_issues
 ORDER BY epics.closed_issues DESC,epic_id;"

cat <<'EOF'

## Controlled benchmarks

[source] Measured task quality, as opposed to the live-traffic counts above, comes from fixed benchmark packets:
reviewer calibration runs 1-2 (`thoughts/shared/bench/reviewer-calibration/run-2-report.md`) and
the #694 Harness-vs-CLI Phase 1 report (`thoughts/shared/bench/harness-vs-cli/phase1-repeat1-report.md`:
T1 hits /23 are opus 18, sol 17, deepseek-v4.1-flash 16, glm-5.3-flashx 16, qwen3.8-flash 16, luna 14,
minimax-m3 13, qwen3-coder-next 11).

## Data gaps

[observed] Issue #580 tracks acceptance-linked telemetry. Issue #700: embedding invocations copy session usage (excluded above). Issue #704: OpenRouter server-tool calls are not recorded as tool calls. Issues #584–#588 are its concrete sub-gaps:
session cost, invocation cost, token coverage, effort identity, and terminal failure cause. The NULL and cause counts above quantify the current snapshot.
[inferred] Sparse InProgress transitions limit work-time measurement; merge and reply attribution does not prove a single author's contribution or link every Closed issue to a landed SHA.

## Reproducibility

[source] The exact read-only SQL statements and git commands used for every number are in [scripts/model-scorecard.sh](../../../scripts/model-scorecard.sh). The daemon DB is live, so a later refresh can change counts.
EOF
