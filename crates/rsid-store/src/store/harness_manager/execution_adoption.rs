//! #3274: a replacement project manager seat adopts the topology executions
//! the displaced seat requested.
//!
//! A manager-requested execution records the appointment's `scope_version` and
//! every agent node launch re-checks it (#1145): a *narrowed* scope must stop
//! old work. An operator re-appointment of another session bumps that version
//! even when the new scope still covers the execution's Epic, which stranded
//! every in-flight execution (`manager_scope_changed`, not retryable). The
//! appointment transaction therefore re-stamps each non-final execution of the
//! displaced appointment whose Epic the new scope still covers, and records an
//! `execution_adopted` event naming both seats. An execution whose Epic left
//! the scope keeps its old version and still blocks. Nothing here grants
//! authority: the policy grant is revoked by the same scope bump, and the
//! launch, landing and close gates re-prove the new seat's live authority.

use chrono::{SecondsFormat, Utc};
use rsi_common::types::{GraphExecutionUpdate, WorkflowExecutionStatus};

use super::*;

/// Event kind appended to an adopted execution's log.
pub(crate) const EXECUTION_ADOPTED_EVENT: &str = "execution_adopted";

impl Store {
    /// Re-stamp the displaced appointment's open executions onto `config`'s
    /// scope version when the new scope still covers their Epic. Returns the
    /// adopted execution ids.
    pub(crate) fn adopt_displaced_seat_executions(
        &self,
        previous: &HarnessManagerConfigV1,
        config: &HarnessManagerConfigV1,
    ) -> Result<Vec<Uuid>> {
        if config.is_revoked()
            || config.current_session_id.is_none()
            || previous.manager_session_id == config.manager_session_id
        {
            return Ok(Vec::new());
        }
        // The same requester-to-seat walk the creation ledger uses (#1283): the
        // requester is the displaced appointment's seat or a rotation
        // successor of it, however many seats it has been through.
        let mut statement = self.conn.prepare(
            "SELECT e.id,e.workflow_id,e.status,COALESCE(e.epic_id,e.parent_session_id) \
             FROM topology_executions e \
             WHERE e.project_id=?1 AND e.requested_by_kind='manager' AND e.scope_version=?2 \
               AND e.status IN ('accepted','running','cancelling','blocked') \
               AND EXISTS(WITH RECURSIVE seat(id,depth) AS ( \
                     SELECT e.requested_by_session_id,0 \
                     UNION SELECT s.continued_from,q.depth+1 FROM seat q JOIN sessions s ON s.id=q.id \
                       WHERE s.continued_from IS NOT NULL AND q.depth<64 \
                     UNION SELECT r.predecessor_session_id,q.depth+1 FROM seat q \
                       JOIN harness_manager_rotation_edges r ON r.successor_session_id=q.id \
                       WHERE q.depth<64 \
                   ) SELECT 1 FROM seat WHERE id=?3) \
             ORDER BY e.created_at,e.id",
        )?;
        let candidates = statement
            .query_map(
                params![
                    previous.project_id.to_string(),
                    previous.row_version,
                    previous.manager_session_id.to_string()
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut adopted = Vec::new();
        for (id, workflow_id, status, epic) in candidates {
            let (Ok(id), Ok(workflow_id)) = (Uuid::parse_str(&id), Uuid::parse_str(&workflow_id))
            else {
                continue;
            };
            let in_scope = epic
                .as_deref()
                .and_then(|epic| Uuid::parse_str(epic).ok())
                .is_some_and(|epic| config.epic_ids.contains(&epic));
            if !in_scope {
                continue;
            }
            let now = Utc::now();
            let stamp = now.to_rfc3339_opts(SecondsFormat::Nanos, true);
            let changed = self.conn.execute(
                "UPDATE topology_executions SET scope_version=?2,row_version=row_version+1,updated_at=?3 \
                 WHERE id=?1 AND scope_version=?4",
                params![id.to_string(), config.row_version, stamp, previous.row_version],
            )?;
            if changed == 0 {
                continue;
            }
            let sequence: i64 = self.conn.query_row(
                "SELECT COALESCE(MAX(execution_seq),0)+1 FROM topology_events WHERE execution_id=?1",
                [id.to_string()],
                |row| row.get(0),
            )?;
            let update = GraphExecutionUpdate {
                execution_id: id,
                workflow_id,
                node_id: None,
                sequence: u64::try_from(sequence).unwrap_or_default(),
                status: if status == "blocked" {
                    WorkflowExecutionStatus::Blocked
                } else if status == "accepted" {
                    WorkflowExecutionStatus::Accepted
                } else {
                    WorkflowExecutionStatus::Running
                },
                node_state: None,
                finished: false,
                error: None,
                output_preview: None,
                updated_at: now,
            };
            let payload = serde_json::json!({
                "update": update,
                "detail": {
                    "kind": EXECUTION_ADOPTED_EVENT,
                    "previous_seat": previous.manager_session_id,
                    "previous_current_seat": previous.current_session_id,
                    "previous_scope_version": previous.row_version,
                    "seat": config.manager_session_id,
                    "current_seat": config.current_session_id,
                    "scope_version": config.row_version,
                },
            });
            self.conn.execute(
                "INSERT INTO topology_events (execution_id,execution_seq,topology_id,node_id,attempt_id,kind,actor_kind,actor_session_id,payload_json,created_at) \
                 SELECT id,?2,topology_id,NULL,NULL,?3,'daemon',NULL,?4,?5 FROM topology_executions WHERE id=?1",
                params![
                    id.to_string(),
                    sequence,
                    EXECUTION_ADOPTED_EVENT,
                    payload.to_string(),
                    stamp
                ],
            )?;
            adopted.push(id);
        }
        Ok(adopted)
    }

    /// #3274: the project manager's current seat for a session that held the
    /// slot under an appointment an explicit re-appointment displaced (the
    /// operator seated a fresh, parentless session), so it is no hop of the
    /// current appointment's lineage ([`Self::project_manager_seat_now`]). The
    /// durable proof is `harness_manager_notice_scope_retirements`, written by
    /// the appointment transaction. `None` when `session` is the current seat,
    /// is a hop of the current appointment, or no recorded displaced
    /// appointment's lineage reaches it. Only a name: the caller re-proves the
    /// returned seat's live authority and scope.
    pub fn displaced_manager_seat_now(&self, session: Uuid) -> Result<Option<Uuid>> {
        let Some(project) = self
            .get_session(session)?
            .and_then(|session| session.project_id)
        else {
            return Ok(None);
        };
        let Some(config) = self
            .get_harness_manager_notice_config(project)?
            .filter(|config| !config.is_revoked())
        else {
            return Ok(None);
        };
        let Some(current) = config.current_session_id else {
            return Ok(None);
        };
        if current == session
            || manager_lineage_hops_on(&self.conn, config.manager_session_id, project)
                .contains(&session)
        {
            return Ok(None);
        }
        let mut retired = self.conn.prepare(
            "SELECT manager_session_id FROM harness_manager_notice_scope_retirements \
             WHERE project_id=?1 AND manager_session_id<>?2 \
             GROUP BY manager_session_id ORDER BY MAX(retired_at) DESC LIMIT 64",
        )?;
        let anchors = retired
            .query_map(
                params![project.to_string(), config.manager_session_id.to_string()],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for anchor in anchors {
            let Ok(anchor) = Uuid::parse_str(&anchor) else {
                continue;
            };
            if manager_lineage_hops_on(&self.conn, anchor, project).contains(&session) {
                return Ok(Some(current));
            }
        }
        Ok(None)
    }
}
