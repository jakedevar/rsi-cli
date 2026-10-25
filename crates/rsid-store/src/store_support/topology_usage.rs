//! Sessions charged to a manager's `max_created_sessions` by its topology
//! executions (moved down from `topology::agent`; the three failure classes it
//! excludes live here and `topology::store::failure` re-exports them).

use crate::error::Result;
use rusqlite::{Connection, params};
use uuid::Uuid;

/// #3495: the instant an attempt `a` counts from in the rolling creation
/// window (`$now`: the SQL parameter holding the clock). The charge starts
/// when the session is created; an attempt still launching (`reserved` or
/// `launching`) with no session yet stays charged as of `now`, however long
/// the launch was held; any other attempt without a session counts from its
/// reservation. The clock parameter goes between [`ATTEMPT_AT_HEAD`] and
/// [`ATTEMPT_AT_TAIL`].
const ATTEMPT_AT_HEAD: &str = "COALESCE((SELECT s.created_at FROM sessions s WHERE s.id=a.session_id),(CASE WHEN a.status IN ('reserved','launching') THEN ";
/// See [`ATTEMPT_AT_HEAD`].
const ATTEMPT_AT_TAIL: &str = " ELSE a.created_at END))";

pub const LOST_BEFORE_SESSION: &str = "lost_before_session";
pub const LAUNCH_REFUSED: &str = "launch_refused";
/// An agent-requested launch refused by live manager policy (#633,
/// plan §5.3): no session was created; the execution blocks.
pub const POLICY_REFUSED: &str = "policy_refused";

/// Sessions charged to the manager's `max_created_sessions` by executions it
/// requested within this appointment scope (#633). An attempt is charged
/// once its launch began (`boot_id` stamped) unless it provably created no
/// session.
///
/// #1283: an execution is charged to the ledger principal that requested it,
/// not to every principal sharing `(project, scope_version)`: a project
/// manager and a global seat can hold equal versions in one project. The
/// requester acts for `principal` when it is `principal` itself or a rotation
/// successor of it (`continued_from`, or a manager rotation edge), however
/// many seats it has been through.
pub fn topology_created_usage(
    conn: &Connection,
    project_id: Uuid,
    principal: Uuid,
    scope_version: i64,
) -> Result<i64> {
    Ok(topology_created_times(
        conn,
        project_id,
        principal,
        scope_version,
        "",
        chrono::Utc::now(),
    )?
    .len() as i64)
}

/// [`topology_created_usage`] as the `created_at` of each charged attempt at
/// or after `since` (RFC3339; `""` for all), oldest first (#3416: the
/// rolling creation window counts these).
pub fn topology_created_times(
    conn: &Connection,
    project_id: Uuid,
    principal: Uuid,
    scope_version: i64,
    since: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<String>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {ATTEMPT_AT_HEAD}?8{ATTEMPT_AT_TAIL} AS charged_at FROM topology_node_attempts a JOIN topology_executions e ON e.id=a.execution_id \
         WHERE e.project_id=?1 AND e.requested_by_kind='manager' AND e.scope_version=?2 \
           AND a.node_kind='session' AND a.boot_id IS NOT NULL AND charged_at>=?7 \
           AND (a.failure_class IS NULL OR a.failure_class NOT IN (?3,?4,?5)) \
           AND EXISTS(WITH RECURSIVE seat(id,depth) AS ( \
                 SELECT e.requested_by_session_id,0 \
                 UNION SELECT s.continued_from,q.depth+1 FROM seat q JOIN sessions s ON s.id=q.id \
                   WHERE s.continued_from IS NOT NULL AND q.depth<64 \
                 UNION SELECT r.predecessor_session_id,q.depth+1 FROM seat q \
                   JOIN harness_manager_rotation_edges r ON r.successor_session_id=q.id \
                   WHERE q.depth<64 \
               ) SELECT 1 FROM seat WHERE id=?6) \
         ORDER BY charged_at",
    ))?; // sql-dynamic-ok: static ATTEMPT_AT_HEAD/TAIL clauses
    let rows = statement
        .query_map(
            params![
                project_id.to_string(),
                scope_version,
                LAUNCH_REFUSED,
                LOST_BEFORE_SESSION,
                POLICY_REFUSED,
                principal.to_string(),
                since,
                now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(rows)
}

/// #1275: the sessions charged by every manager-requested topology execution
/// in `project` whose attempt began at or after `since` (the earliest
/// portfolio epoch start in the chain), whichever principal requested it.
/// The same charged-attempt rule as [`topology_created_usage`]. #1301: each
/// row is `(attempt created_at, requesting session, scope version)`, so the
/// caller charges it only to its originating node and that node's ancestors.
pub fn topology_created_charges_since(
    conn: &Connection,
    project_id: Uuid,
    since: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<(String, Option<String>, Option<i64>)>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {ATTEMPT_AT_HEAD}?6{ATTEMPT_AT_TAIL} AS charged_at,e.requested_by_session_id,e.scope_version \
         FROM topology_node_attempts a JOIN topology_executions e ON e.id=a.execution_id \
         WHERE e.project_id=?1 AND e.requested_by_kind='manager' AND charged_at>=?2 \
           AND a.node_kind='session' AND a.boot_id IS NOT NULL \
           AND (a.failure_class IS NULL OR a.failure_class NOT IN (?3,?4,?5))",
    ))?; // sql-dynamic-ok: static ATTEMPT_AT_HEAD/TAIL clauses
    let rows = statement
        .query_map(
            params![
                project_id.to_string(),
                since,
                LAUNCH_REFUSED,
                LOST_BEFORE_SESSION,
                POLICY_REFUSED,
                now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
