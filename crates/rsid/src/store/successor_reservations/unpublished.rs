//! Pre-publication admissions have no committed custody generation to adopt.
//! Preserve the exact obligation until runtime proves its process cohort gone.

use super::*;
use rsi_common::model_control::{
    AdmissionStatus, InvocationOwner, ModelInvocationPurpose, ModelInvocationRecord,
    ModelInvocationStatus,
};

pub(crate) const UNPUBLISHED_SUCCESSOR_CLEANUP: &str = "agent_successor_unpublished_not_restorable";
pub(crate) const UNPUBLISHED_SUCCESSOR_ADMISSION_DENIED: &str = "agent_successor_admission_denied";

#[derive(Debug)]
pub(crate) enum UnpublishedSuccessorCleanupClaim {
    Missing,
    SettledDenied,
    Admitted(ModelInvocationRecord),
}

impl Store {
    /// A cleanup claim, never an execution permit. Cancellation still consumes
    /// capacity and makes the existing pre-effect INSERT (requires running)
    /// refuse. It survives cancellation/reopen even if the orphan path changes.
    /// This grants no execution/lead authority: cleanup remains valid after a
    /// policy pause or scope/lead change and never updates those live pointers.
    pub(crate) fn claim_unpublished_agent_successor_cleanup(
        &self,
        expected: &AgentSuccessorReservation,
        allocation_root: &std::path::Path,
    ) -> Result<UnpublishedSuccessorCleanupClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let Some(record) = self.unpublished_agent_successor_admission(expected, allocation_root)?
        else {
            tx.commit()?;
            return Ok(UnpublishedSuccessorCleanupClaim::Missing);
        };
        if record.admission_status == AdmissionStatus::Denied {
            tx.commit()?;
            self.finish_unpublished_agent_successor_cleanup(expected, allocation_root)?;
            return Ok(UnpublishedSuccessorCleanupClaim::SettledDenied);
        }
        tx.execute(
            "UPDATE model_invocations SET status='cancellation_requested',error_class=?2,
                 cancellation_requested_at=COALESCE(cancellation_requested_at,?3)
             WHERE id=?1 AND admission_status='admitted'
               AND status IN ('running','cancellation_requested')",
            params![
                record.id.to_string(),
                UNPUBLISHED_SUCCESSOR_CLEANUP,
                now_string()
            ],
        )?;
        tx.commit()?;
        Ok(UnpublishedSuccessorCleanupClaim::Admitted(record))
    }

    /// Recheck every durable identity before marking the reservation terminal.
    /// Ledger settlement comes first; the cancellation claim makes an aborted
    /// two-step settlement resumable without reissuing provider authority.
    pub(crate) fn finish_unpublished_agent_successor_cleanup(
        &self,
        expected: &AgentSuccessorReservation,
        allocation_root: &std::path::Path,
    ) -> Result<()> {
        let record = self
            .unpublished_agent_successor_admission(expected, allocation_root)?
            .ok_or_else(|| DaemonError::Store("successor cleanup admission disappeared".into()))?;
        if record.admission_status == AdmissionStatus::Denied {
            self.settle_agent_successor_failed(
                expected.reservation_id,
                expected.state_version,
                expected.launch_attempt_id.expect("validated launch attempt"),
                Uuid::new_v4(),
                "successor model admission was denied before provider execution; allocation retained",
                UNPUBLISHED_SUCCESSOR_ADMISSION_DENIED,
            )?;
            return Ok(());
        }
        if matches!(
            record.status,
            ModelInvocationStatus::Running | ModelInvocationStatus::CancellationRequested
        ) {
            return Err(DaemonError::PolicyDenied(
                "successor cleanup invocation is not settled".into(),
            ));
        }
        self.settle_agent_successor_failed(
            expected.reservation_id,
            expected.state_version,
            expected.launch_attempt_id.expect("validated launch attempt"),
            Uuid::new_v4(),
            "admitted successor has no published custody generation; allocation retained, exact cohort settled",
            UNPUBLISHED_SUCCESSOR_CLEANUP,
        )?;
        Ok(())
    }

    /// Any durable publication or custody trace of this candidate or root.
    fn agent_successor_candidate_published(
        &self,
        candidate: Uuid,
        allocation_root: &std::path::Path,
    ) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)
                 OR EXISTS(SELECT 1 FROM session_execution_projections WHERE session_id=?1)
                 OR EXISTS(SELECT 1 FROM sandbox_custody_roots WHERE owner_session_id=?1)
                 OR EXISTS(SELECT 1 FROM sandbox_custody_roots WHERE sandbox_root=?2)",
            params![candidate.to_string(), allocation_root.to_string_lossy()],
            |row| row.get(0),
        )?)
    }

    /// True only when the candidate left no durable trace at all: no row,
    /// projection, custody root, or model invocation. Only then can a root at
    /// its deterministic path be the pre-admission allocation (Issue #620);
    /// admitted candidates stay with the exact unpublished cleanup above.
    pub(crate) fn agent_successor_candidate_untraced(
        &self,
        reservation: &AgentSuccessorReservation,
        allocation_root: &std::path::Path,
    ) -> Result<bool> {
        if self.agent_successor_candidate_published(
            reservation.candidate_session_id,
            allocation_root,
        )? {
            return Ok(false);
        }
        let invoked: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM model_invocations WHERE session_id=?1 OR id=?2)",
            params![
                reservation.candidate_session_id.to_string(),
                reservation
                    .model_invocation_id
                    .map(|id| id.to_string())
                    .unwrap_or_default()
            ],
            |row| row.get(0),
        )?;
        Ok(!invoked)
    }

    fn unpublished_agent_successor_admission(
        &self,
        expected: &AgentSuccessorReservation,
        allocation_root: &std::path::Path,
    ) -> Result<Option<ModelInvocationRecord>> {
        let current = self.get_agent_successor(expected.reservation_id)?;
        if current.as_ref() != Some(expected)
            || !matches!(
                expected.state,
                AgentSuccessorStateV1::Launching | AgentSuccessorStateV1::Uncertain
            )
            || expected.launch_attempt_id.is_none()
            || expected.model_invocation_id.is_none()
            || expected.establishment_evidence_json.is_some()
            || expected.establishment_digest.is_some()
            || expected.published_at.is_some()
        {
            return Err(DaemonError::PolicyDenied(
                "agent_successor_cleanup_reservation_changed".into(),
            ));
        }
        let id = expected.candidate_session_id.to_string();
        // Missing Session alone is insufficient. Custody allocation/events are
        // immutable; publication binds both before any execution claim. Refuse
        // even an orphan/historical root or projection rather than fabricate a
        // new custody generation from a path observation.
        if self
            .agent_successor_candidate_published(expected.candidate_session_id, allocation_root)?
        {
            return Err(DaemonError::PolicyDenied(
                "agent_successor_cleanup_publication_exists".into(),
            ));
        }
        let Some(record) =
            self.load_model_invocation_record(expected.model_invocation_id.unwrap())?
        else {
            return Ok(None);
        };
        let frozen = expected.inherited_launch()?;
        let provider = frozen.provider()?;
        let model = expected.request.model.clone().or(frozen.model);
        let model = if provider == rsi_common::types::SessionProvider::Pioneer {
            Some(crate::pioneer::pioneer_launch_model(model.as_deref()).to_string())
        } else if provider == rsi_common::types::SessionProvider::Bedrock && model.is_none() {
            Some(crate::bedrock::BEDROCK_DEFAULT_MODEL.to_string())
        } else if provider == rsi_common::types::SessionProvider::OpenRouter && model.is_none() {
            Some(crate::openrouter::OPENROUTER_DEFAULT_MODEL.to_string())
        } else {
            model
        };
        let key = format!(
            "agent:reserve:successor:{}:{}",
            expected.reservation_id, record.id
        );
        let another: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM model_invocations WHERE session_id=?1 AND id!=?2)",
            params![id, record.id.to_string()],
            |row| row.get(0),
        )?;
        let recoverable_admission = record.admission_status == AdmissionStatus::Admitted
            || (record.admission_status == AdmissionStatus::Denied
                && record.status == ModelInvocationStatus::Denied);
        if !recoverable_admission
            || record.purpose != ModelInvocationPurpose::AgentReserveSuccessor
            || record.owner
                != (InvocationOwner {
                    session_id: Some(expected.candidate_session_id),
                    project_id: frozen.project_id,
                    workflow_id: frozen.workflow_id,
                    topology_node_id: expected.request.topology_node.clone(),
                    ..Default::default()
                })
            || record.trigger != "launch_session"
            || record.dedup_key.as_deref() != Some(key.as_str())
            || record.request_fingerprint.as_deref() != Some(expected.request_fingerprint.as_str())
            || record.provider.as_deref() != Some(format!("{provider:?}").as_str())
            || record.backend != record.provider
            || record.model != model
            || record.effort != expected.request.effort.clone().or(frozen.effort)
            || another
        {
            return Err(DaemonError::PolicyDenied(
                "agent_successor_cleanup_admission_changed".into(),
            ));
        }
        Ok(Some(record))
    }
}
