.width 10 20 20 8 14 14
.headers on
.mode column

-- Known-good denominator: daemon rotation injections are distinct
-- User Message events whose content begins exactly with /create_handoff.
CREATE TEMP TABLE injected_sessions AS
SELECT DISTINCT ce.session_id
FROM conversation_events AS ce
WHERE ce.event_type = 'Message'
  AND ce.role = 'User'
  AND ce.content LIKE '/create_handoff%';

CREATE TEMP TABLE first_injection AS
SELECT ce.session_id, MIN(ce.created_at) AS injected_at, COUNT(*) AS injections
FROM conversation_events AS ce
WHERE ce.event_type = 'Message'
  AND ce.role = 'User'
  AND ce.content LIKE '/create_handoff%'
GROUP BY ce.session_id;

-- Rotation event counts.
SELECT COUNT(*) AS injected_sessions,
       SUM(i.injections) AS injected_events
FROM first_injection AS i;

SELECT s.created_at >= '2026-09-22' AS wave1,
       s.provider,
       s.model,
       COUNT(*) AS sessions,
       SUM(EXISTS(SELECT 1 FROM sessions AS c WHERE c.continued_from = s.id)) AS with_successor
FROM injected_sessions AS x
JOIN sessions AS s ON s.id = x.session_id
GROUP BY wave1, s.provider, s.model
ORDER BY wave1 DESC, sessions DESC;

-- Outcome when an injected session has a direct successor.
SELECT s.created_at >= '2026-09-22' AS wave1,
       s.provider,
       c.status,
       COUNT(*) AS successors
FROM injected_sessions AS x
JOIN sessions AS s ON s.id = x.session_id
JOIN sessions AS c ON c.continued_from = s.id
GROUP BY wave1, s.provider, c.status
ORDER BY wave1 DESC, successors DESC;

-- Interval from forced handoff request to successor creation.
WITH pairs AS (
  SELECT s.provider, s.model, i.injected_at, c.created_at AS successor_at
  FROM first_injection AS i
  JOIN sessions AS s ON s.id = i.session_id
  JOIN sessions AS c ON c.continued_from = s.id
  WHERE s.created_at >= '2026-09-22'
)
SELECT provider, model,
       COUNT(*) AS n,
       ROUND(AVG((julianday(successor_at) - julianday(injected_at)) * 86400), 1) AS avg_seconds,
       ROUND(MIN((julianday(successor_at) - julianday(injected_at)) * 86400), 1) AS min_seconds,
       ROUND(MAX((julianday(successor_at) - julianday(injected_at)) * 86400), 1) AS max_seconds
FROM pairs
GROUP BY provider, model
ORDER BY n DESC;

-- Handoff persistence and orphaned obligations.
SELECT handoff_filepath <> '' AS path_set,
       status,
       COUNT(*) AS sessions
FROM sessions
WHERE NOT EXISTS(SELECT 1 FROM sessions AS c WHERE c.continued_from = id)
  AND handoff_filepath <> ''
GROUP BY path_set, status
ORDER BY sessions DESC;

SELECT substr(s.id, 1, 8) AS predecessor,
       s.provider,
       s.model,
       s.status,
       s.handoff_filepath,
       s.created_at
FROM injected_sessions AS x
JOIN sessions AS s ON s.id = x.session_id
WHERE s.created_at >= '2026-09-22'
  AND NOT EXISTS(SELECT 1 FROM sessions AS c WHERE c.continued_from = s.id)
ORDER BY s.created_at;

