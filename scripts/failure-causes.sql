-- failure-causes.sql — classify ONE primary cause per Failed/Interrupted session.
--
-- Usage (read-only; never writes the DB):
--   sqlite3 -readonly ~/.rsi/rsi.db < scripts/failure-causes.sql
-- Output columns (pipe-separated): session_id|provider|model|status|cause|evidence
--
-- Scope: every OpenRouter session (all time) plus the Codex/Claude comparison
-- set created on/after 2026-09-20. Edit the `scope` CTE to change it.
--
-- Evidence sources (all DB-resident):
--   * sessions.stop_reason                         (sandbox custody, codex usage limit)
--   * the session's last 8 conversation_events that are daemon-authored
--     provider errors ("**Process Error (codex_event)**" / "**Process Error (provider)**"
--     = terminal provider errors; see crates/rsid/src/session/monitor.rs).
--     Tool results are deliberately excluded: agents quote other sessions'
--     errors in tool output, which would contaminate the classifier.
--   * model_invocations: interrupted-settlement clustering (daemon restart).
--
-- Precedence (first match wins):
--   sandbox_custody > provider_credit_exhausted > provider_rate_limit >
--   rsi_killed_transient_stream > rsi_killed_codex_warning >
--   daemon_restart_mass_interrupt > interrupt_unattributed > unknown
-- Doc: thoughts/shared/research/2026-09-22-openrouter-failure-forensics.md
.headers off
.mode list
WITH scope AS (
  SELECT s.* FROM sessions s
  WHERE s.status IN ('Failed','Interrupted')
    AND (s.provider = 'OpenRouter'
         OR (s.provider IN ('Codex','Claude') AND s.created_at >= '2026-09-20'))
),
ranked AS (
  SELECT e.session_id, e.sequence, e.event_type, e.content,
         ROW_NUMBER() OVER (PARTITION BY e.session_id ORDER BY e.sequence DESC) AS rn
  FROM conversation_events e JOIN scope ON scope.id = e.session_id
),
perr AS (  -- daemon-authored terminal provider errors among the last 8 events
  SELECT session_id, sequence,
         substr(replace(replace(content, char(10), ' '), '```', ''), 1, 160) AS line,
         content
  FROM ranked
  -- event_type filter: a ToolResult can quote the same prefix (review finding, Issue #608)
  WHERE rn <= 8 AND event_type = 'Message' AND content LIKE '**Process Error (%'
),
pick AS (  -- newest matching error per class
  SELECT session_id,
    (SELECT line FROM perr p WHERE p.session_id = s.id
       AND (p.content LIKE '%Key limit exceeded%' OR p.content LIKE '%usage limit%'
            OR p.content LIKE '%insufficient credit%' OR p.content LIKE '%402 Payment%')
       ORDER BY sequence DESC LIMIT 1) AS credit,
    (SELECT line FROM perr p WHERE p.session_id = s.id
       AND (p.content LIKE '%429%' OR p.content LIKE '%rate limit%' OR p.content LIKE '%Too Many Requests%')
       ORDER BY sequence DESC LIMIT 1) AS ratelim,
    (SELECT line FROM perr p WHERE p.session_id = s.id
       AND p.content LIKE '%Reconnecting...%'
       ORDER BY sequence DESC LIMIT 1) AS transient,
    (SELECT line FROM perr p WHERE p.session_id = s.id
       AND p.content LIKE '%service tier%not advertised%'
       ORDER BY sequence DESC LIMIT 1) AS warning,
    (SELECT COUNT(DISTINCT m.session_id) FROM model_invocations m
       WHERE m.error_class = 'interrupted' AND m.completed_at IS NOT NULL
         AND abs(julianday(m.completed_at) - julianday(s.updated_at)) * 86400 <= 3) AS cluster
  FROM (SELECT id AS session_id, id, updated_at FROM scope) s
),
classified AS (
  SELECT sc.id AS session_id, sc.provider, coalesce(sc.model, '') AS model, sc.status,
    CASE
      WHEN sc.stop_reason LIKE 'sandbox_custody:%' THEN 'sandbox_custody'
      WHEN sc.stop_reason = 'provider_error:codex_usage_limit' OR p.credit IS NOT NULL
        THEN 'provider_credit_exhausted'
      WHEN p.ratelim IS NOT NULL THEN 'provider_rate_limit'
      WHEN p.transient IS NOT NULL THEN 'rsi_killed_transient_stream'
      WHEN p.warning IS NOT NULL THEN 'rsi_killed_codex_warning'
      WHEN sc.status = 'Interrupted' AND p.cluster >= 3 THEN 'daemon_restart_mass_interrupt'
      WHEN sc.status = 'Interrupted' THEN 'interrupt_unattributed'
      ELSE 'unknown'
    END AS cause,
    CASE
      WHEN sc.stop_reason LIKE 'sandbox_custody:%' THEN 'stop_reason=' || sc.stop_reason
      WHEN sc.stop_reason = 'provider_error:codex_usage_limit' THEN 'stop_reason=' || sc.stop_reason
      WHEN p.credit IS NOT NULL THEN p.credit
      WHEN p.ratelim IS NOT NULL THEN p.ratelim
      WHEN p.transient IS NOT NULL THEN p.transient
      WHEN p.warning IS NOT NULL THEN p.warning
      WHEN sc.status = 'Interrupted' AND p.cluster >= 3
        THEN p.cluster || ' sessions had an interrupted invocation within 3s of updated_at=' || sc.updated_at
      WHEN sc.status = 'Interrupted'
        THEN 'no provider error in last 8 events; cluster=' || p.cluster || '; updated_at=' || sc.updated_at
      ELSE 'stop_reason=' || coalesce(sc.stop_reason, 'NULL') || '; terminal_reason=' || coalesce(sc.terminal_reason, 'NULL')
    END AS evidence
  FROM scope sc JOIN pick p ON p.session_id = sc.id
)
SELECT session_id, provider, model, status, cause, evidence
FROM classified
ORDER BY provider, model, cause, session_id;
