//! Sessions charged to a manager's `max_created_sessions` by its topology
//! executions (moved down from `topology::agent`; the three failure classes it
//! excludes live here and `topology::store::failure` re-exports them).

use crate::error::Result;
use rusqlite::{Connection, params};
use uuid::Uuid;

pub const LOST_BEFORE_SESSION: &str = "lost_before_session";
pub const LAUNCH_REFUSED: &str = "launch_refused";
/// An agent-requested launch refused by live manager policy (#633,
/// plan §5.3): no session was created; the execution blocks.
pub const POLICY_REFUSED: &str = "policy_refused";

/// Sessions charged to the manager's `max_created_sessions` by executions it
/// requested within this appointment scope (#633). An attempt is charged
/// once its launch began (`boot_id` stamped) unless it provably created no
/// session.
pub fn topology_created_usage(
    conn: &Connection,
    project_id: Uuid,
    scope_version: i64,
) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM topology_node_attempts a JOIN topology_executions e ON e.id=a.execution_id \
         WHERE e.project_id=?1 AND e.requested_by_kind='manager' AND e.scope_version=?2 \
           AND a.node_kind='session' AND a.boot_id IS NOT NULL \
           AND (a.failure_class IS NULL OR a.failure_class NOT IN (?3,?4,?5))",
        params![
            project_id.to_string(),
            scope_version,
            LAUNCH_REFUSED,
            LOST_BEFORE_SESSION,
            POLICY_REFUSED,
        ],
        |row| row.get(0),
    )?)
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
) -> Result<Vec<(String, Option<String>, Option<i64>)>> {
    let mut statement = conn.prepare(
        "SELECT a.created_at,e.requested_by_session_id,e.scope_version \
         FROM topology_node_attempts a JOIN topology_executions e ON e.id=a.execution_id \
         WHERE e.project_id=?1 AND e.requested_by_kind='manager' AND a.created_at>=?2 \
           AND a.node_kind='session' AND a.boot_id IS NOT NULL \
           AND (a.failure_class IS NULL OR a.failure_class NOT IN (?3,?4,?5))",
    )?;
    let rows = statement
        .query_map(
            params![
                project_id.to_string(),
                since,
                LAUNCH_REFUSED,
                LOST_BEFORE_SESSION,
                POLICY_REFUSED,
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
