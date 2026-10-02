//! Sessions charged to a manager's `max_created_sessions` by its topology
//! executions (moved down from `topology::agent`; the three failure classes it
//! excludes live here and `topology::store::failure` re-exports them).

use crate::error::Result;
use rusqlite::{Connection, params};
use uuid::Uuid;

pub(crate) const LOST_BEFORE_SESSION: &str = "lost_before_session";
pub(crate) const LAUNCH_REFUSED: &str = "launch_refused";
/// An agent-requested launch refused by live manager policy (#633,
/// plan §5.3): no session was created; the execution blocks.
pub(crate) const POLICY_REFUSED: &str = "policy_refused";

/// Sessions charged to the manager's `max_created_sessions` by executions it
/// requested within this appointment scope (#633). An attempt is charged
/// once its launch began (`boot_id` stamped) unless it provably created no
/// session.
pub(crate) fn topology_created_usage(
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
