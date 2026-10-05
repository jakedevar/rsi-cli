//! `AgentQueryFailureSignatures` (#1016): the open Issues of one project that
//! carry known-failure signature blocks. Read-only, no migration.

use super::Store;
use crate::error::{DaemonError, Result};
use rsi_common::agent_failure_signatures::{FAILURE_SIGNATURE_BODY_MARKER, SignatureIssueSource};
use uuid::Uuid;

impl Store {
    /// Open, non-archived Issues of `project_id` whose body carries the
    /// signature fence marker, in display-number order. Closed, cancelled and
    /// archived Issues are never returned, so their records expire live.
    ///
    /// # Errors
    /// A persistence error, or a malformed Issue row.
    pub fn open_failure_signature_issues(
        &self,
        project_id: Uuid,
    ) -> Result<Vec<SignatureIssueSource>> {
        let mut statement = self.conn.prepare(
            "SELECT id, display_number, status, body FROM issues
             WHERE project_id = ?1
               AND status IN ('Open', 'InProgress')
               AND archived_at IS NULL
               AND instr(body, ?2) > 0
             ORDER BY display_number ASC, id ASC",
        )?;
        let rows = statement
            .query_map(
                rusqlite::params![project_id.to_string(), FAILURE_SIGNATURE_BODY_MARKER],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, number, status, body)| {
                let issue_id = Uuid::parse_str(&id)
                    .map_err(|error| DaemonError::Store(format!("invalid Issue id: {error}")))?;
                let display_number = u64::try_from(number)
                    .map_err(|_| DaemonError::Store("negative Issue display number".into()))?;
                Ok(SignatureIssueSource {
                    issue_id,
                    display_number,
                    status,
                    body,
                })
            })
            .collect()
    }
}
