//! #1879: storage_control admits project managers and portfolio seats only.
//! This fence is specific to audited cohort settlement; it does not widen
//! the portfolio arm's delegated operator-call allowlist.

use crate::error::{DaemonError, Result};
use crate::store::Store;
use rsi_common::harness_manager_v2::{ManagerCapabilityV2, ManagerOperatingModeV2};
use rsi_common::types::SessionStatus;
use uuid::Uuid;

pub fn agent_settlement_refusal(code: &str, reason: &str) -> DaemonError {
    DaemonError::StructuredRpc {
        rpc_code: rsi_common::rpc::INVALID_PARAMS,
        message: code.into(),
        data: serde_json::json!({"code": code, "reason": reason}),
    }
}

pub fn normalize_agent_settlement_error(error: DaemonError) -> DaemonError {
    match error {
        DaemonError::InvalidParam(reason) => {
            agent_settlement_refusal("storage_control_proof_refused", &reason)
        }
        other => other,
    }
}

impl Store {
    /// Resolve current operator grants. A permitting seat contributes its
    /// coverage; a paused, monitor, revoked or retired seat contributes none.
    /// Area seats and Epic leads cannot delete an entire repository cohort.
    pub fn agent_source_worktree_projects(&self, caller: Uuid) -> Result<Vec<Uuid>> {
        let session = self
            .get_session(caller)?
            .filter(|s| !matches!(s.status, SessionStatus::Archived | SessionStatus::Deleted))
            .ok_or_else(|| {
                agent_settlement_refusal(
                    "storage_control_not_authorized",
                    "a live manager seat with storage_control is required",
                )
            })?;
        let permits = |policy: &rsi_common::harness_manager_v2::ManagerPolicyV2| {
            policy.mode == ManagerOperatingModeV2::Execute
                && !policy.paused
                && policy
                    .capabilities
                    .contains(&ManagerCapabilityV2::StorageControl)
        };
        let mut projects = Vec::new();
        if let Some(grant) = self.portfolio_seat_grant(caller)?
            && permits(&grant.project_policy)
        {
            projects.extend(grant.project_ids);
        }
        if let Some(project) = session.project_id
            && let Some(config) = self.get_harness_manager(project)?
            && config.current_session_id == Some(caller)
            && let Some(grant) = self.get_harness_manager_policy(project)?
            && !grant.revoked
            && grant.manager_session_id == config.manager_session_id
            && grant.scope_version == config.row_version
            && permits(&grant.policy)
        {
            projects.push(project);
        }
        projects.sort_unstable();
        projects.dedup();
        if projects.is_empty() {
            return Err(agent_settlement_refusal(
                "storage_control_not_authorized",
                "a current Execute-mode, unpaused PM or portfolio seat with storage_control is required",
            ));
        }
        Ok(projects)
    }

    /// Admission proves a repository association, not authority over every root.
    /// Retained receipts keep this association after the last covered root settles.
    /// Per-root exclusions are computed separately and included in the audit digest.
    pub fn authorize_agent_source_worktree_cohort(
        &self,
        caller: Uuid,
        identity: &str,
    ) -> Result<()> {
        for project in self.agent_source_worktree_projects(caller)? {
            let covered: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions s WHERE s.project_id=?2 AND s.id IN (
                     SELECT owner_session_id FROM sandbox_custody_roots
                      WHERE repository_identity=?1 AND state='live'
                     UNION SELECT s.id FROM sessions s JOIN sandbox_custody_roots r
                       ON s.sandbox_custody_id=r.custody_id
                      WHERE r.repository_identity=?1 AND r.state='live'
                     UNION SELECT session_id FROM source_worktree_settlement_items
                      WHERE repository_identity=?1
                 )) OR EXISTS(SELECT 1 FROM projects p JOIN sandbox_custody_roots r
                      ON p.path=r.canonical_repo_dir
                      WHERE p.id=?2 AND r.repository_identity=?1 AND r.state='live')",
                rusqlite::params![identity, project.to_string()],
                |row| row.get(0),
            )?;
            if covered {
                return Ok(());
            }
        }
        Err(agent_settlement_refusal(
            "storage_control_not_in_scope",
            "no repository cohort in your coverage",
        ))
    }

    fn source_worktree_root_scope_disposition(
        &self,
        projects: &[Uuid],
        custody_id: Uuid,
    ) -> Result<Option<rsi_common::cohort_settlement::SourceWorktreeDispositionV1>> {
        use rsi_common::cohort_settlement::SourceWorktreeDispositionV1 as D;
        let mut statement = self.conn.prepare(
            "SELECT s.project_id FROM (
                 SELECT owner_session_id AS session_id FROM sandbox_custody_roots WHERE custody_id=?1
                 UNION SELECT id FROM sessions WHERE sandbox_custody_id=?1
                 UNION SELECT session_id FROM source_worktree_settlement_items WHERE custody_id=?1
             ) linked LEFT JOIN sessions s ON s.id=linked.session_id
             JOIN sandbox_custody_roots r ON r.custody_id=?1 LIMIT ?2",
        )?;
        let rows = statement.query_map(
            rusqlite::params![
                custody_id.to_string(),
                super::SETTLEMENT_DEPENDENCY_SCAN_MAX_RECORDS as i64 + 1
            ],
            |row| row.get::<_, Option<String>>(0),
        )?;
        let mut count = 0;
        let mut foreign = false;
        for row in rows {
            count += 1;
            let Some(project) = row?.and_then(|p| Uuid::parse_str(&p).ok()) else {
                return Ok(Some(D::RetainedUnknownOwner));
            };
            foreign |= !projects.contains(&project);
        }
        Ok(
            if count == 0 || count > super::SETTLEMENT_DEPENDENCY_SCAN_MAX_RECORDS {
                Some(D::RetainedUnknownOwner)
            } else if foreign {
                Some(D::RetainedForeignOwner)
            } else {
                None
            },
        )
    }

    pub fn scope_agent_source_worktree_inventory(
        &self,
        caller: Uuid,
        identity: &str,
        inventory: &mut [super::SourceWorktreeInventoryRow],
    ) -> Result<()> {
        self.authorize_agent_source_worktree_cohort(caller, identity)?;
        let projects = self.agent_source_worktree_projects(caller)?;
        for row in inventory {
            row.agent_scope_disposition =
                self.source_worktree_root_scope_disposition(&projects, row.custody_id)?;
        }
        Ok(())
    }

    /// #2690: keep only the retained-root snapshot entries whose root the caller
    /// covers today (the same ownership proof as the fresh audit items); a
    /// foreign or unknown-owner root is left out of the agent's view.
    pub fn scope_agent_retained_roots(
        &self,
        caller: Uuid,
        roots: &mut Vec<rsi_common::cohort_settlement::SourceWorktreeRetainedRootV1>,
    ) -> Result<()> {
        if roots.is_empty() {
            return Ok(());
        }
        let projects = self.agent_source_worktree_projects(caller)?;
        let mut kept = Vec::with_capacity(roots.len());
        for root in roots.drain(..) {
            if self
                .source_worktree_root_scope_disposition(&projects, root.custody_id)?
                .is_none()
            {
                kept.push(root);
            }
        }
        *roots = kept;
        Ok(())
    }

    /// Recheck this exact root under the Store/custody fence before any effect.
    pub fn authorize_agent_source_worktree_root(
        &self,
        caller: Uuid,
        identity: &str,
        custody_id: Uuid,
    ) -> Result<()> {
        self.authorize_agent_source_worktree_cohort(caller, identity)?;
        let projects = self.agent_source_worktree_projects(caller)?;
        let matches_repository: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sandbox_custody_roots WHERE custody_id=?1 AND repository_identity=?2 AND state='live')",
            rusqlite::params![custody_id.to_string(), identity], |row| row.get(0),
        )?;
        if !matches_repository {
            return Err(agent_settlement_refusal(
                "storage_control_not_in_scope",
                "custody root does not belong to this live repository cohort",
            ));
        }
        if self
            .source_worktree_root_scope_disposition(&projects, custody_id)?
            .is_some()
        {
            return Err(agent_settlement_refusal(
                "storage_control_not_in_scope",
                "this custody root has foreign or unknown ownership",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
