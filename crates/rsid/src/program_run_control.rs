//! Construction-only `ProgramRun` authority boundary.

// The accepted symbol inventory requires explicit crate-private capability
// names even though this entire module is itself crate-private.
#![allow(clippy::redundant_pub_crate)]

use crate::idea_control::BoundControllerWriteAuthority;
use crate::session::AgentTokenRegistry;
use crate::session::types::TrackedSession;
use crate::store::Store;
use crate::store::program_runs::{
    ProgramRunExternalReferenceV1, ProgramRunStoreAuthority, ProgramRunStoreError,
    ProgramRunTransitionInputV1, program_run_controller_authority_from_live_grant,
    program_run_scheduler_authority_from_live_grant,
};
use chrono::{DateTime, Utc};
use rsi_common::program_runs::{
    AcknowledgeProgramRunWakeRequestV1, CancelProgramRunRequestV1, ClaimProgramRunActionRequestV1,
    CreateProgramRunRequestV1, ProgramRunActionAcknowledgementResultV1,
    ProgramRunActionClaimResultV1, ProgramRunActionKindV1, ProgramRunActionPublicationResultV1,
    ProgramRunActionV1, ProgramRunExternalReferenceResultV1, ProgramRunLockV1,
    ProgramRunMutationResultV1, ProgramRunOperationalStatusV1, ProgramRunPageCursorV1,
    ProgramRunPageV1, ProgramRunReconciliationPageV1, ProgramRunStatusV1,
    ProgramRunTransitionPageV1, ProgramRunTransitionRequestV1, ProgramRunV1,
    ResumeBlockedProgramRunRequestV1,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

pub(crate) trait ProgramRunClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub(crate) struct SystemProgramRunClock;

impl ProgramRunClock for SystemProgramRunClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub(crate) enum ProgramRunControlError {
    #[error("invalid_request")]
    InvalidRequest,
    #[error("forbidden")]
    Forbidden,
    #[error("not_found")]
    NotFound,
    #[error("active_run_exists")]
    ActiveRunExists,
    #[error("replay_conflict")]
    ReplayConflict,
    #[error("stale_run_version")]
    StaleRunVersion,
    #[error("stale_idea_version")]
    StaleIdeaVersion,
    #[error("stale_controller_epoch")]
    StaleControllerEpoch,
    #[error("controller_mismatch")]
    ControllerMismatch,
    #[error("stale_lease_generation")]
    StaleLeaseGeneration,
    #[error("stale_claim_generation")]
    StaleClaimGeneration,
    #[error("invalid_transition")]
    InvalidTransition,
    #[error("cursor_invariant")]
    CursorInvariant,
    #[error("gate_incomplete")]
    GateIncomplete,
    #[error("budget_exhausted")]
    BudgetExhausted,
    #[error("queue_backpressure")]
    QueueBackpressure,
    #[error("lock_unavailable")]
    LockUnavailable,
    #[error("terminal_run")]
    TerminalRun,
    #[error("quarantined")]
    Quarantined,
    #[error("downstream_replay_conflict")]
    DownstreamReplayConflict,
    #[error("contention")]
    Contention,
    #[error("constraint_violation")]
    ConstraintViolation,
    #[error("corrupt_stored_state")]
    CorruptStoredState,
    #[error("storage_failure")]
    StorageFailure,
}

impl From<ProgramRunStoreError> for ProgramRunControlError {
    fn from(error: ProgramRunStoreError) -> Self {
        match error {
            ProgramRunStoreError::InvalidRequest => Self::InvalidRequest,
            ProgramRunStoreError::Forbidden => Self::Forbidden,
            ProgramRunStoreError::NotFound => Self::NotFound,
            ProgramRunStoreError::ActiveRunExists => Self::ActiveRunExists,
            ProgramRunStoreError::ReplayConflict => Self::ReplayConflict,
            ProgramRunStoreError::StaleRunVersion => Self::StaleRunVersion,
            ProgramRunStoreError::StaleIdeaVersion => Self::StaleIdeaVersion,
            ProgramRunStoreError::StaleControllerEpoch => Self::StaleControllerEpoch,
            ProgramRunStoreError::ControllerMismatch => Self::ControllerMismatch,
            ProgramRunStoreError::StaleLeaseGeneration => Self::StaleLeaseGeneration,
            ProgramRunStoreError::StaleClaimGeneration => Self::StaleClaimGeneration,
            ProgramRunStoreError::InvalidTransition => Self::InvalidTransition,
            ProgramRunStoreError::CursorInvariant => Self::CursorInvariant,
            ProgramRunStoreError::GateIncomplete => Self::GateIncomplete,
            ProgramRunStoreError::BudgetExhausted => Self::BudgetExhausted,
            ProgramRunStoreError::QueueBackpressure => Self::QueueBackpressure,
            ProgramRunStoreError::LockUnavailable => Self::LockUnavailable,
            ProgramRunStoreError::TerminalRun => Self::TerminalRun,
            ProgramRunStoreError::Quarantined => Self::Quarantined,
            ProgramRunStoreError::DownstreamReplayConflict => Self::DownstreamReplayConflict,
            ProgramRunStoreError::Contention => Self::Contention,
            ProgramRunStoreError::ConstraintViolation => Self::ConstraintViolation,
            ProgramRunStoreError::CorruptStoredState => Self::CorruptStoredState,
            ProgramRunStoreError::StorageFailure => Self::StorageFailure,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ProgramRunControlHandle {
    store: Arc<Mutex<Store>>,
    clock: Arc<dyn ProgramRunClock>,
    active: Option<Arc<RwLock<HashMap<Uuid, TrackedSession>>>>,
    agent_tokens: Option<Arc<RwLock<AgentTokenRegistry>>>,
    boot_id: Option<Uuid>,
}

#[derive(Clone)]
pub(crate) struct BoundProgramRunOperatorAuthority {
    handle: ProgramRunControlHandle,
    project_id: Option<Uuid>,
}

#[derive(Clone)]
#[allow(dead_code)] // Construction-bound D05 controller capability; domain transports arrive later.
pub(crate) struct BoundProgramRunControllerAuthority {
    handle: ProgramRunControlHandle,
    project_id: Uuid,
    idea_id: Uuid,
    session_id: Uuid,
    epoch: u64,
    grant_incarnation: Uuid,
    a6_token: String,
}

#[derive(Clone)]
pub(crate) struct BoundProgramRunSchedulerAuthority {
    handle: ProgramRunControlHandle,
    project_id: Uuid,
    idea_id: Uuid,
    session_id: Uuid,
    epoch: u64,
    boot_id: Uuid,
    grant_incarnation: Uuid,
    a6_token: String,
}

impl ProgramRunControlHandle {
    #[cfg(test)]
    pub(crate) fn new(store: Arc<Mutex<Store>>, clock: Arc<dyn ProgramRunClock>) -> Self {
        Self {
            store,
            clock,
            active: None,
            agent_tokens: None,
            boot_id: None,
        }
    }

    pub(crate) fn new_with_live_witnesses(
        store: Arc<Mutex<Store>>,
        clock: Arc<dyn ProgramRunClock>,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        agent_tokens: Arc<RwLock<AgentTokenRegistry>>,
        boot_id: Uuid,
    ) -> Self {
        Self {
            store,
            clock,
            active: Some(active),
            agent_tokens: Some(agent_tokens),
            boot_id: Some(boot_id),
        }
    }

    pub(crate) fn bind_operator(
        &self,
        project_id: Uuid,
    ) -> Result<BoundProgramRunOperatorAuthority, ProgramRunControlError> {
        if project_id.is_nil() {
            return Err(ProgramRunControlError::InvalidRequest);
        }
        Ok(BoundProgramRunOperatorAuthority {
            handle: self.clone(),
            project_id: Some(project_id),
        })
    }

    pub(crate) fn bind_operator_all(&self) -> BoundProgramRunOperatorAuthority {
        BoundProgramRunOperatorAuthority {
            handle: self.clone(),
            project_id: None,
        }
    }

    pub(crate) fn bind_controller(
        &self,
        grant: &BoundControllerWriteAuthority,
        grant_incarnation: Uuid,
        a6_token: String,
    ) -> Result<BoundProgramRunControllerAuthority, ProgramRunControlError> {
        if grant_incarnation.is_nil() || a6_token.is_empty() || grant.controller_epoch() <= 0 {
            return Err(ProgramRunControlError::Forbidden);
        }
        Ok(BoundProgramRunControllerAuthority {
            handle: self.clone(),
            project_id: grant.project_id(),
            idea_id: grant.idea_id(),
            session_id: grant.controller_session_id(),
            epoch: u64::try_from(grant.controller_epoch())
                .map_err(|_| ProgramRunControlError::Forbidden)?,
            grant_incarnation,
            a6_token,
        })
    }

    pub(crate) fn bind_scheduler(
        &self,
        grant: &BoundControllerWriteAuthority,
        boot_id: Uuid,
        grant_incarnation: Uuid,
        a6_token: String,
    ) -> Result<BoundProgramRunSchedulerAuthority, ProgramRunControlError> {
        let epoch = u64::try_from(grant.controller_epoch())
            .map_err(|_| ProgramRunControlError::Forbidden)?;
        if grant.project_id().is_nil()
            || grant.idea_id().is_nil()
            || grant.controller_session_id().is_nil()
            || epoch == 0
            || boot_id.is_nil()
            || grant_incarnation.is_nil()
            || a6_token.is_empty()
        {
            return Err(ProgramRunControlError::Forbidden);
        }
        Ok(BoundProgramRunSchedulerAuthority {
            handle: self.clone(),
            project_id: grant.project_id(),
            idea_id: grant.idea_id(),
            session_id: grant.controller_session_id(),
            epoch,
            boot_id,
            grant_incarnation,
            a6_token,
        })
    }
}

impl BoundProgramRunOperatorAuthority {
    fn scope(&self, run: &ProgramRunV1) -> Result<(), ProgramRunControlError> {
        if self
            .project_id
            .is_none_or(|project_id| run.project_id == project_id)
        {
            Ok(())
        } else {
            Err(ProgramRunControlError::NotFound)
        }
    }

    pub(crate) async fn create(
        &self,
        request: &CreateProgramRunRequestV1,
    ) -> Result<ProgramRunMutationResultV1, ProgramRunControlError> {
        let result = self
            .handle
            .store
            .lock()
            .await
            .create_program_run_v1(&ProgramRunStoreAuthority::operator(), request)?;
        self.scope(&result.run)?;
        Ok(result)
    }

    pub(crate) async fn get(
        &self,
        run_id: Uuid,
    ) -> Result<Option<ProgramRunV1>, ProgramRunControlError> {
        let run = self.handle.store.lock().await.get_program_run_v1(run_id)?;
        match run {
            Some(run) if self.scope(&run).is_ok() => Ok(Some(run)),
            Some(_) | None => Ok(None),
        }
    }

    pub(crate) async fn list(
        &self,
        idea_id: Option<Uuid>,
        status: Option<rsi_common::program_runs::ProgramRunStatusV1>,
        cursor: Option<&ProgramRunPageCursorV1>,
        limit: u32,
    ) -> Result<ProgramRunPageV1, ProgramRunControlError> {
        self.handle
            .store
            .lock()
            .await
            .list_program_runs_v1(self.project_id, idea_id, status, cursor, limit)
            .map_err(Into::into)
    }

    pub(crate) async fn transitions(
        &self,
        run_id: Uuid,
        after: Option<u64>,
        limit: u32,
    ) -> Result<ProgramRunTransitionPageV1, ProgramRunControlError> {
        let run = self
            .get(run_id)
            .await?
            .ok_or(ProgramRunControlError::NotFound)?;
        self.scope(&run)?;
        self.handle
            .store
            .lock()
            .await
            .list_program_run_transitions_v1(run_id, after, limit)
            .map_err(Into::into)
    }

    pub(crate) async fn status(
        &self,
        run_id: Uuid,
        controller_a6_live: bool,
    ) -> Result<ProgramRunOperationalStatusV1, ProgramRunControlError> {
        self.get(run_id)
            .await?
            .ok_or(ProgramRunControlError::NotFound)?;
        self.handle
            .store
            .lock()
            .await
            .get_program_run_operational_status_v1(
                run_id,
                controller_a6_live,
                self.handle.clock.now(),
            )
            .map_err(Into::into)
    }

    pub(crate) async fn cancel(
        &self,
        request: &CancelProgramRunRequestV1,
    ) -> Result<ProgramRunMutationResultV1, ProgramRunControlError> {
        self.get(request.program_run_id)
            .await?
            .ok_or(ProgramRunControlError::NotFound)?;
        let input = ProgramRunTransitionInputV1::Simple(ProgramRunTransitionRequestV1 {
            program_run_id: request.program_run_id,
            expected_run_version: request.expected_run_version,
            expected_idea_version: request.expected_idea_version,
            operation: rsi_common::program_runs::ProgramRunOperationV1::OperatorCancelled,
            idempotency_key: request.idempotency_key.clone(),
            reason: Some(request.reason.clone()),
        });
        self.handle
            .store
            .lock()
            .await
            .apply_program_run_transition_v1(&ProgramRunStoreAuthority::operator(), &input)
            .map_err(Into::into)
    }

    pub(crate) async fn resume(
        &self,
        request: &ResumeBlockedProgramRunRequestV1,
    ) -> Result<ProgramRunMutationResultV1, ProgramRunControlError> {
        self.get(request.program_run_id)
            .await?
            .ok_or(ProgramRunControlError::NotFound)?;
        let input = ProgramRunTransitionInputV1::Resume(request.clone());
        self.handle
            .store
            .lock()
            .await
            .apply_program_run_transition_v1(&ProgramRunStoreAuthority::operator(), &input)
            .map_err(Into::into)
    }

    pub(crate) async fn reconcile(
        &self,
        cursor: Option<&ProgramRunPageCursorV1>,
        limit: u32,
        time_budget_ms: u32,
        dry_run: bool,
    ) -> Result<ProgramRunReconciliationPageV1, ProgramRunControlError> {
        let active = self
            .handle
            .active
            .as_ref()
            .ok_or(ProgramRunControlError::Forbidden)?
            .read()
            .await;
        let store = self.handle.store.lock().await;
        let tokens = self
            .handle
            .agent_tokens
            .as_ref()
            .ok_or(ProgramRunControlError::Forbidden)?
            .read()
            .await;
        let mut live_controllers = HashMap::new();
        for session_id in active.keys().copied() {
            if tokens.token_for_session(session_id).is_some()
                && let Some(grant) = store.controller_grant_v1(session_id)
                && let Ok(epoch) = u64::try_from(grant.controller_epoch())
            {
                live_controllers.insert(session_id, epoch);
            }
        }
        store
            .reconcile_program_runs_page_v1(
                self.project_id,
                cursor,
                limit,
                time_budget_ms,
                dry_run,
                self.handle.clock.now(),
                &live_controllers,
                self.handle
                    .boot_id
                    .ok_or(ProgramRunControlError::Forbidden)?,
            )
            .map_err(Into::into)
    }
}

#[allow(dead_code)] // Kept typed and private so D05 cannot expose a generic transition transport.
impl BoundProgramRunControllerAuthority {
    pub(crate) async fn transition(
        &self,
        input: &ProgramRunTransitionInputV1,
    ) -> Result<ProgramRunMutationResultV1, ProgramRunControlError> {
        let run_id = match input {
            ProgramRunTransitionInputV1::Simple(v) => v.program_run_id,
            ProgramRunTransitionInputV1::ClaimAction(v) => v.program_run_id,
            ProgramRunTransitionInputV1::AcknowledgeWake(v) => v.program_run_id,
            ProgramRunTransitionInputV1::Resume(v) => v.program_run_id,
            ProgramRunTransitionInputV1::CommitOutput(v) => v.program_run_id,
            ProgramRunTransitionInputV1::Gate(v) => v.program_run_id,
        };
        let active = self
            .handle
            .active
            .as_ref()
            .ok_or(ProgramRunControlError::Forbidden)?
            .read()
            .await;
        if !active.contains_key(&self.session_id) {
            return Err(ProgramRunControlError::Forbidden);
        }
        let store = self.handle.store.lock().await;
        let (grant, grant_incarnation) = store
            .controller_grant_witness_v1(self.session_id)
            .ok_or(ProgramRunControlError::Forbidden)?;
        if grant.project_id() != self.project_id
            || grant.idea_id() != self.idea_id
            || u64::try_from(grant.controller_epoch()).ok() != Some(self.epoch)
            || grant_incarnation != self.grant_incarnation
        {
            return Err(ProgramRunControlError::Forbidden);
        }
        let tokens = self
            .handle
            .agent_tokens
            .as_ref()
            .ok_or(ProgramRunControlError::Forbidden)?
            .read()
            .await;
        if tokens.get(&self.a6_token).copied() != Some(self.session_id) {
            return Err(ProgramRunControlError::Forbidden);
        }
        let run = store
            .get_program_run_v1(run_id)?
            .ok_or(ProgramRunControlError::Forbidden)?;
        if run.project_id != self.project_id || run.idea_id != self.idea_id {
            return Err(ProgramRunControlError::Forbidden);
        }
        store
            .apply_program_run_transition_v1(
                &program_run_controller_authority_from_live_grant(&grant),
                input,
            )
            .map_err(|_| ProgramRunControlError::Forbidden)
    }

    pub(crate) async fn heartbeat_locks(
        &self,
        run_id: Uuid,
        boot_id: Uuid,
        generation: u64,
    ) -> Result<Vec<ProgramRunLockV1>, ProgramRunControlError> {
        let active = self
            .handle
            .active
            .as_ref()
            .ok_or(ProgramRunControlError::Forbidden)?
            .read()
            .await;
        if !active.contains_key(&self.session_id) {
            return Err(ProgramRunControlError::Forbidden);
        }
        let store = self.handle.store.lock().await;
        let (grant, grant_incarnation) = store
            .controller_grant_witness_v1(self.session_id)
            .ok_or(ProgramRunControlError::Forbidden)?;
        if grant.project_id() != self.project_id
            || grant.idea_id() != self.idea_id
            || u64::try_from(grant.controller_epoch()).ok() != Some(self.epoch)
            || grant_incarnation != self.grant_incarnation
        {
            return Err(ProgramRunControlError::Forbidden);
        }
        let tokens = self
            .handle
            .agent_tokens
            .as_ref()
            .ok_or(ProgramRunControlError::Forbidden)?
            .read()
            .await;
        if tokens.get(&self.a6_token).copied() != Some(self.session_id) {
            return Err(ProgramRunControlError::Forbidden);
        }
        store
            .heartbeat_program_run_locks_v1(
                &program_run_controller_authority_from_live_grant(&grant),
                run_id,
                boot_id,
                generation,
                self.handle.clock.now(),
            )
            .map_err(|_| ProgramRunControlError::Forbidden)
    }
}

impl BoundProgramRunSchedulerAuthority {
    async fn live_store(
        &self,
    ) -> Result<
        (
            tokio::sync::RwLockReadGuard<'_, HashMap<Uuid, TrackedSession>>,
            tokio::sync::MutexGuard<'_, Store>,
            tokio::sync::RwLockReadGuard<'_, AgentTokenRegistry>,
            BoundControllerWriteAuthority,
        ),
        ProgramRunControlError,
    > {
        let active = self
            .handle
            .active
            .as_ref()
            .ok_or(ProgramRunControlError::Forbidden)?
            .read()
            .await;
        if !active.contains_key(&self.session_id) {
            return Err(ProgramRunControlError::Forbidden);
        }
        let store = self.handle.store.lock().await;
        let (grant, grant_incarnation) = store
            .controller_grant_witness_v1(self.session_id)
            .ok_or(ProgramRunControlError::Forbidden)?;
        if u64::try_from(grant.controller_epoch()).ok() != Some(self.epoch)
            || grant.project_id() != self.project_id
            || grant.idea_id() != self.idea_id
            || grant_incarnation != self.grant_incarnation
        {
            return Err(ProgramRunControlError::Forbidden);
        }
        let tokens = self
            .handle
            .agent_tokens
            .as_ref()
            .ok_or(ProgramRunControlError::Forbidden)?
            .read()
            .await;
        if tokens.get(&self.a6_token).copied() != Some(self.session_id) {
            return Err(ProgramRunControlError::Forbidden);
        }
        Ok((active, store, tokens, grant))
    }

    #[cfg(test)]
    pub(crate) async fn claim(
        &self,
        kinds: &[ProgramRunActionKindV1],
        limit: u32,
    ) -> Result<Vec<ProgramRunActionClaimResultV1>, ProgramRunControlError> {
        let (_active, store, _tokens, grant) = self.live_store().await?;
        store
            .claim_due_program_run_actions_v1(
                &program_run_scheduler_authority_from_live_grant(&grant),
                self.boot_id,
                kinds,
                self.handle.clock.now(),
                limit,
            )
            .map_err(Into::into)
    }

    pub(crate) async fn claim_dispatch_candidate(
        &self,
        kinds: &[ProgramRunActionKindV1],
        action_id: Uuid,
    ) -> Result<Option<ProgramRunActionClaimResultV1>, ProgramRunControlError> {
        let (_active, store, _tokens, grant) = self.live_store().await?;
        store
            .claim_program_run_dispatch_candidate_v1(
                &program_run_scheduler_authority_from_live_grant(&grant),
                self.boot_id,
                kinds,
                self.handle.clock.now(),
                action_id,
            )
            .map_err(Into::into)
    }

    pub(crate) async fn published(
        &self,
        action_id: Uuid,
        generation: u64,
    ) -> Result<ProgramRunActionPublicationResultV1, ProgramRunControlError> {
        let (_active, store, _tokens, grant) = self.live_store().await?;
        store
            .record_program_run_publication_v1(
                &program_run_scheduler_authority_from_live_grant(&grant),
                action_id,
                self.boot_id,
                generation,
                self.handle.clock.now(),
            )
            .map_err(Into::into)
    }

    pub(crate) async fn claim_action(
        &self,
        request: &ClaimProgramRunActionRequestV1,
    ) -> Result<ProgramRunMutationResultV1, ProgramRunControlError> {
        let (_active, store, _tokens, grant) = self.live_store().await?;
        store
            .apply_program_run_transition_v1(
                &program_run_scheduler_authority_from_live_grant(&grant),
                &ProgramRunTransitionInputV1::ClaimAction(request.clone()),
            )
            .map_err(Into::into)
    }

    pub(crate) async fn bind_reference(
        &self,
        action_id: Uuid,
        generation: u64,
        reference: ProgramRunExternalReferenceV1,
    ) -> Result<ProgramRunExternalReferenceResultV1, ProgramRunControlError> {
        let (_active, store, _tokens, grant) = self.live_store().await?;
        store
            .bind_program_run_external_reference_v1(
                &program_run_scheduler_authority_from_live_grant(&grant),
                action_id,
                self.boot_id,
                generation,
                reference,
                self.handle.clock.now(),
            )
            .map_err(Into::into)
    }

    pub(crate) async fn fail(
        &self,
        action_id: Uuid,
        generation: u64,
        error_class: &str,
        error_message: &str,
    ) -> Result<ProgramRunActionV1, ProgramRunControlError> {
        let (_active, store, _tokens, grant) = self.live_store().await?;
        store
            .acknowledge_program_run_action_v1(
                &program_run_scheduler_authority_from_live_grant(&grant),
                action_id,
                self.boot_id,
                generation,
                Some((error_class, error_message, true)),
                self.handle.clock.now(),
            )
            .map(|result| result.action)
            .map_err(Into::into)
    }

    pub(crate) async fn acknowledge(
        &self,
        action_id: Uuid,
        generation: u64,
    ) -> Result<ProgramRunActionAcknowledgementResultV1, ProgramRunControlError> {
        let (_active, store, _tokens, grant) = self.live_store().await?;
        let authority = program_run_scheduler_authority_from_live_grant(&grant);
        let acknowledgement = store.acknowledge_program_run_action_v1(
            &authority,
            action_id,
            self.boot_id,
            generation,
            None,
            self.handle.clock.now(),
        )?;
        if acknowledgement.action.action_kind == ProgramRunActionKindV1::Wake {
            let run = store
                .get_program_run_v1(acknowledgement.action.program_run_id)?
                .ok_or(ProgramRunControlError::NotFound)?;
            if run.status == ProgramRunStatusV1::RetryPending {
                let claim_boot_id = acknowledgement
                    .action
                    .claim_boot_id
                    .ok_or(ProgramRunControlError::StaleClaimGeneration)?;
                let claim_run_version = acknowledgement
                    .action
                    .claim_run_version
                    .ok_or(ProgramRunControlError::StaleRunVersion)?;
                let claim_lease_generation = acknowledgement
                    .action
                    .claim_lease_generation
                    .ok_or(ProgramRunControlError::StaleLeaseGeneration)?;
                store.apply_program_run_transition_v1(
                    &authority,
                    &ProgramRunTransitionInputV1::AcknowledgeWake(
                        AcknowledgeProgramRunWakeRequestV1 {
                            program_run_id: run.id,
                            expected_run_version: run.row_version,
                            expected_idea_version: run.idea_row_version,
                            idempotency_key: format!(
                                "program-run-wake-ack:{action_id}:{claim_boot_id}:{generation}"
                            ),
                            action_id,
                            claim_boot_id,
                            claim_generation: generation,
                            claim_run_version,
                            claim_lease_generation,
                        },
                    ),
                )?;
            } else if run.status != ProgramRunStatusV1::Ready {
                return Err(ProgramRunControlError::InvalidTransition);
            }
        }
        Ok(acknowledgement)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn d05_operator_authority_is_construction_bound() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let handle = ProgramRunControlHandle::new(store, Arc::new(SystemProgramRunClock));
        let project_id = Uuid::new_v4();
        let bound = handle.bind_operator(project_id).unwrap();
        assert_eq!(bound.project_id, Some(project_id));
        assert!(handle.bind_operator(Uuid::nil()).is_err());
    }
}
