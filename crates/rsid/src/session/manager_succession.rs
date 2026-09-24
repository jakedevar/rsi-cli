//! One root succession occurrence, using the ordinary confirmed provider engine.
//! Runtime guards and filesystem proofs never come from caller JSON or receipts.
use super::agent_verbs::AgentControlHandle;
use super::spawn_single_flight::{SpawnGuard, acquire_provider_cwd_admission, acquire_spawn_guard};
use super::types::{LaunchPurpose, ProspectiveAgentTokenWitness};
use super::{AgentTokenRegistry, CompletedSession, SessionManager, TrackedSession};
use crate::bus::{DaemonEvent, EventBus};
use crate::claude::LaunchConfig;
use crate::error::Result;
use crate::sandbox::custody::CustodyExecutionRuntime;
use crate::store::Store;
use crate::store::harness_manager_v2::refused;
use crate::store::manager_actions::ManagerActionClaimV2;
use crate::store::manager_successions::*;
use rsi_common::harness_manager_v2::*;
use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Mutex, RwLock, oneshot};
use uuid::Uuid;

#[derive(Clone)]
struct Runtime {
    active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: Arc<Mutex<Store>>,
    tokens: Arc<RwLock<AgentTokenRegistry>>,
    bus: Arc<EventBus>,
    custody: CustodyExecutionRuntime,
}

/// The predecessor guard survives cancellation of the reconciler's caller until
/// the provider task publishes or settles. No Store custody shard is retained.
#[derive(Clone)]
pub(super) struct ManagerSuccessionLaunchContext {
    candidate: Uuid,
    refusal: Arc<std::sync::Mutex<&'static str>>,
    claim: Arc<Mutex<ManagerSuccessionClaim>>,
    runtime: Runtime,
    predecessor_guard: Arc<Mutex<Option<SpawnGuard>>>,
    confirmation: Arc<Mutex<Option<oneshot::Sender<Result<ManagerActionReceiptV2>>>>>,
    pub(super) prospective_a6: ProspectiveAgentTokenWitness,
    pub(super) generation: Arc<AtomicU64>,
}
impl std::fmt::Debug for ManagerSuccessionLaunchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerSuccessionLaunchContext")
            .field("candidate", &self.candidate)
            .finish_non_exhaustive()
    }
}

impl AgentControlHandle {
    pub(super) async fn enqueue_root_manager_succession(
        &self,
        caller: Uuid,
        request: AgentManagerControlRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        // This is admission, not interruption. The caller is still running and
        // must be able to receive the queued receipt before draining naturally.
        let _guard = acquire_spawn_guard(caller).await;
        let _cwd = acquire_provider_cwd_admission().await;
        let runtime = self
            .custody_runtime
            .as_ref()
            .ok_or_else(|| refused("manager_succession_custody_runtime_required"))?;
        let predecessor = {
            let store = self.store.lock().await;
            store.manager_succession_observation(caller)?;
            store
                .get_session(caller)?
                .ok_or_else(|| refused("manager_session_unavailable"))?
        };
        let ManagerActionV2::SucceedManager { handoff, .. } = &request.operation else {
            return Err(refused("manager_succession_action_required"));
        };
        let (verified, _) = runtime
            .prepare_manager_handoff(&predecessor, handoff)
            .await?;
        self.store
            .lock()
            .await
            .enqueue_manager_succession(caller, &request, &verified)
    }
}

impl SessionManager {
    fn manager_succession_runtime(&self) -> Runtime {
        Runtime {
            active: self.active.clone(),
            completed: self.completed.clone(),
            store: self.store.clone(),
            tokens: self.agent_tokens.clone(),
            bus: self.event_bus.clone(),
            custody: self.custody_execution_runtime(),
        }
    }

    pub(super) async fn execute_manager_succession(
        &self,
        action: &ManagerActionClaimV2,
    ) -> Result<()> {
        let mut claim = self.store.lock().await.claim_manager_succession(action)?;
        let predecessor = claim.reservation.predecessor_session_id;
        let guard = acquire_spawn_guard(predecessor).await;
        // A terminal DB row is not a physical drain proof. Never interrupt the
        // predecessor from this tool/reconciler, including a late monitor drain.
        if self.active.read().await.contains_key(&predecessor) {
            return self
                .store
                .lock()
                .await
                .defer_manager_succession_drain(&claim);
        }
        if let Some(old) = self.completed.read().await.get(&predecessor) {
            if old.retry_cancel.is_some()
                || old.retry_fired_at.is_some()
                || old.superseded_by_retry.is_some()
                || old.session.pending_question.is_some()
                || old.session.pending_archive
            {
                return Err(refused("manager_succession_predecessor_unsettled"));
            }
        }
        if !self.context_rotation_enabled {
            return Err(refused("manager_succession_rotation_disabled"));
        }
        tokio::task::spawn_blocking(move || super::reaper::reap_orphans_for_session(predecessor))
            .await
            .map_err(|_| refused("manager_succession_predecessor_unsettled"))?
            .map_err(|error| {
                tracing::warn!(session_id=%predecessor, %error, "manager predecessor drain remains unproved");
                refused("manager_succession_predecessor_unsettled")
            })?;
        let source = self
            .store
            .lock()
            .await
            .get_session(predecessor)?
            .ok_or_else(|| refused("manager_session_unavailable"))?;
        let proof = ManagerPredecessorSettledWitness::after_checked_drain(
            &source,
            claim.reservation.frozen.predecessor_invocation_id,
            self.program_run_boot_id,
        )?;
        claim = self
            .store
            .lock()
            .await
            .record_manager_succession_predecessor_settled(&claim, &proof)?;
        let (_, content) = self
            .custody_execution_runtime()
            .prepare_manager_handoff(&source, &claim.reservation.frozen.handoff)
            .await?;
        let config = launch_config(&claim.reservation, content)?;
        let (tx, rx) = oneshot::channel();
        let context = ManagerSuccessionLaunchContext {
            candidate: claim.reservation.candidate_session_id,
            refusal: Arc::new(std::sync::Mutex::new(
                "manager_succession_establishment_uncertain",
            )),
            claim: Arc::new(Mutex::new(claim)),
            runtime: self.manager_succession_runtime(),
            predecessor_guard: Arc::new(Mutex::new(Some(guard))),
            confirmation: Arc::new(Mutex::new(Some(tx))),
            prospective_a6: ProspectiveAgentTokenWitness::default(),
            generation: Arc::new(AtomicU64::new(0)),
        };
        let disabled = source.rotation_disabled_at.is_some();
        if let Err(error) = self
            .launch_session_with_retry_admission(
                config,
                None,
                disabled,
                LaunchPurpose::ManagerSuccessor(Box::new(context.clone())),
                None,
            )
            .await
        {
            // The launch guards have unwound. Reacquire the exact candidate
            // guard before process-first settlement, including no-Session exits.
            let _candidate = acquire_spawn_guard(context.candidate_id()).await;
            let _cwd = acquire_provider_cwd_admission().await;
            context.remember_refusal(&error);
            context.fail_and_settle().await?;
            return Err(error);
        }
        rx.await
            .map_err(|_| refused("manager_succession_establishment_uncertain"))??;
        Ok(())
    }

    pub(super) async fn reconcile_manager_succession_cleanup(&self) -> Result<usize> {
        let rows = self
            .store
            .lock()
            .await
            .list_recoverable_manager_successions(None, 64)?;
        let runtime = self.manager_succession_runtime();
        let mut settled = 0;
        for root in rows {
            // The guard, not map membership, identifies a still-owned launch.
            // A failed checked kill can leave a tracked process after its task
            // exits; that exact obligation must remain actively recoverable.
            let Some(_guard) =
                super::spawn_single_flight::try_acquire_spawn_guard(root.candidate_session_id)
            else {
                continue;
            };
            let _cwd = acquire_provider_cwd_admission().await;
            let generation = self
                .active
                .read()
                .await
                .get(&root.candidate_session_id)
                .map(|tracked| tracked.spawn_generation);
            match runtime.cleanup(&root, generation).await {
                Ok(()) => settled += 1,
                Err(error) => {
                    tracing::warn!(operation_id=%root.operation_id, error=%error, "manager succession cleanup remains owed")
                }
            }
        }
        Ok(settled)
    }
}

impl ManagerSuccessionLaunchContext {
    pub(super) fn remember_refusal(&self, error: &crate::error::DaemonError) {
        *self
            .refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            super::manager_actions::safe_action_error(error);
    }
    pub(super) fn candidate_id(&self) -> Uuid {
        self.candidate
    }
    pub(super) async fn claim(&self) -> ManagerSuccessionClaim {
        self.claim.lock().await.clone()
    }
    pub(super) async fn refresh(&self) -> Result<()> {
        let mut claim = self.claim.lock().await;
        *claim = self
            .runtime
            .store
            .lock()
            .await
            .claim_manager_succession(&claim.action)?;
        Ok(())
    }
    pub(super) async fn source_gate(&self) -> Result<()> {
        let claim = self.claim().await;
        self.runtime
            .custody
            .prepare_manager_handoff(
                &claim.reservation.frozen.predecessor,
                &claim.reservation.frozen.handoff,
            )
            .await?;
        if let Some(custody) = claim.reservation.candidate_custody.as_ref() {
            let candidate = self
                .runtime
                .store
                .lock()
                .await
                .get_session(self.candidate)?
                .ok_or_else(|| refused("manager_succession_candidate_missing"))?;
            self.runtime
                .custody
                .authenticate_manager_candidate(&candidate, custody)
                .await?;
        }
        self.runtime
            .store
            .lock()
            .await
            .manager_succession_effect_gate(&claim)
    }
    pub(super) async fn claim_effect(&self) -> Result<()> {
        self.source_gate().await?;
        let mut claim = self.claim.lock().await;
        *claim = self
            .runtime
            .store
            .lock()
            .await
            .claim_manager_succession_provider_effect(&claim)?;
        Ok(())
    }
    /// Called with the actual candidate spawn/cwd guards, after the process is
    /// installed and before its event monitor can publish any uncommitted state.
    pub(super) async fn publish(&self) -> Result<()> {
        self.source_gate().await?;
        let claim = self.claim().await;
        let generation = self.generation.load(Ordering::Acquire);
        let token = self
            .prospective_a6
            .current_token()
            .await
            .ok_or_else(|| refused("manager_succession_token_changed"))?;
        let mut active = self.runtime.active.write().await;
        let tracked = active
            .get_mut(&self.candidate)
            .ok_or_else(|| refused("manager_succession_candidate_unconfirmed"))?;
        if generation == 0 || tracked.spawn_generation != generation || tracked.interrupt_requested
        {
            return Err(refused("manager_succession_candidate_unconfirmed"));
        }
        let confirmed = tracked.process.as_mut().is_some_and(|process| {
            if tracked.session.provider == SessionProvider::CodexAppServer {
                matches!(process, super::types::ProviderProcess::CodexAppServer(_))
                    && process.is_alive()
            } else {
                super::provider_spawn::installed_provider_confirmation(
                    tracked.session.provider,
                    process,
                )
                .is_some()
            }
        });
        if !confirmed {
            return Err(refused("manager_succession_candidate_unconfirmed"));
        }
        let store = self.runtime.store.lock().await;
        let mut tokens = self.runtime.tokens.write().await;
        if tokens.get(&token).copied() != Some(self.candidate) {
            return Err(refused("manager_succession_token_changed"));
        }
        let proof = ManagerSuccessionPublicationWitness::after_provider_established(
            self.candidate,
            claim.reservation.model_invocation_id,
            claim.reservation.launch_attempt_id,
            claim.action.boot_id,
        )?;
        // Store acquires distinct sorted custody shards itself. Runtime retains
        // only spawn/cwd/process/token witnesses here, never those shard locks.
        let mut predecessor = store
            .get_session(claim.reservation.predecessor_session_id)?
            .ok_or_else(|| refused("manager_session_unavailable"))?;
        let receipt = store.commit_manager_succession(&claim, &proof)?;
        // No fallible post-commit read may abandon the established writer before
        // its monitor takes ownership. This is the exact metadata-only mutation
        // committed above; candidate runtime counters remain intact.
        predecessor.status = SessionStatus::Archived;
        predecessor.updated_at = chrono::Utc::now();
        tokens.revoke_session(predecessor.id);
        drop(tokens);
        drop(store);
        drop(active);
        let old_id = predecessor.id;
        self.runtime
            .completed
            .write()
            .await
            .entry(old_id)
            .and_modify(|old| old.session = predecessor.clone())
            .or_insert_with(|| completed_row(predecessor));
        self.runtime.bus.publish(DaemonEvent::SessionArchived {
            session_id: old_id,
            projection_id: None,
        });
        self.predecessor_guard.lock().await.take();
        if let Some(tx) = self.confirmation.lock().await.take() {
            let _ = tx.send(Ok(receipt));
        }
        Ok(())
    }
    /// Also used by the deferred task's exit guard. An unknown process keeps a
    /// CleanupRequired record and admitted capacity; it never triggers resend.
    pub(super) async fn fail_and_settle(&self) -> Result<()> {
        let claim = self.claim().await;
        let root = {
            let store = self.runtime.store.lock().await;
            let root = store
                .manager_succession(claim.reservation.operation_id)?
                .ok_or_else(|| refused("manager_succession_unavailable"))?;
            if root.state == ManagerRootState::Committed {
                return Ok(());
            }
            if store
                .manager_action_operation(root.operation_id)?
                .is_some_and(|op| op.receipt.state == ManagerActionStateV2::Running)
            {
                store.finish_manager_action(
                    &claim.action,
                    if root.admission_recorded || root.effect_claimed {
                        ManagerActionStateV2::Uncertain
                    } else {
                        ManagerActionStateV2::Blocked
                    },
                    *self
                        .refusal
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                )?;
            }
            store
                .manager_succession(root.operation_id)?
                .ok_or_else(|| refused("manager_succession_unavailable"))?
        };
        let result = if root.state == ManagerRootState::CleanupRequired {
            self.runtime
                .cleanup(&root, Some(self.generation.load(Ordering::Acquire)))
                .await
        } else {
            Ok(())
        };
        self.predecessor_guard.lock().await.take();
        if let Some(tx) = self.confirmation.lock().await.take() {
            let code = *self
                .refusal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = tx.send(Err(refused(code)));
        }
        result
    }
}

impl Runtime {
    async fn cleanup(&self, root: &ManagerRootSuccession, generation: Option<u64>) -> Result<()> {
        let id = root.candidate_session_id;
        {
            let store = self.store.lock().await;
            if store
                .session_model_invocation_id(id)?
                .is_some_and(|inv| inv != root.model_invocation_id)
            {
                return Err(refused("manager_succession_cleanup_changed"));
            }
            if let Some(expected) = root.candidate_custody.as_ref() {
                if ManagerRootCustody::from(&store.live_custody_for_session(id)?) != *expected {
                    return Err(refused("manager_succession_cleanup_changed"));
                }
            }
        }
        {
            let mut active = self.active.write().await;
            if let Some(tracked) = active.get_mut(&id) {
                if generation != Some(tracked.spawn_generation) {
                    return Err(refused("manager_succession_cleanup_changed"));
                }
                tracked.interrupt_requested = true;
                if let Some(process) = tracked.process.as_mut() {
                    process.kill().await?;
                    if process.is_alive() {
                        return Err(refused("manager_succession_cleanup_unsettled"));
                    }
                }
            }
        }
        tokio::task::spawn_blocking(move || super::reaper::reap_orphans_for_session(id))
            .await
            .map_err(|_| refused("manager_succession_cleanup_unsettled"))?
            .map_err(|error| {
                tracing::warn!(session_id=%id, %error, "manager candidate process settlement remains unproved");
                refused("manager_succession_cleanup_unsettled")
            })?;
        if self
            .store
            .lock()
            .await
            .load_model_invocation_record(root.model_invocation_id)?
            .is_some()
        {
            crate::model_control::complete_invocation_by_id(
                &self.store,
                root.model_invocation_id,
                crate::model_control::InvocationCompletion {
                    error_class: Some("manager_succession_establishment_uncertain".into()),
                    confidence: Some(rsi_common::model_control::ModelUsageConfidence::Unavailable),
                    ..Default::default()
                },
                &self.bus,
            )
            .await?;
        }
        let session = {
            let store = self.store.lock().await;
            if store.get_session(id)?.is_some() {
                store.update_session_status(id, SessionStatus::Failed)?;
            }
            let current = store
                .manager_succession(root.operation_id)?
                .ok_or_else(|| refused("manager_succession_unavailable"))?;
            store.settle_manager_succession(
                &current,
                &ManagerSuccessionCleanupWitness::after_checked_settlement(&current),
            )?;
            store.get_session(id)?
        };
        self.active.write().await.remove(&id);
        if let Some(session) = session {
            self.completed
                .write()
                .await
                .insert(id, completed_row(session));
        }
        super::revoke_agent_tokens_for_session(&self.tokens, id).await;
        match self
            .store
            .lock()
            .await
            .reconcile_manager_action_notice(root.operation_id)
        {
            Ok(Some(job_id)) => self
                .bus
                .publish(DaemonEvent::ManagerNoticeQueued { job_id }),
            Ok(None) => {}
            Err(error) => tracing::warn!(operation_id=%root.operation_id, %error,
                "manager succession action notice reconciliation deferred"),
        }
        Ok(())
    }
}

fn launch_config(root: &ManagerRootSuccession, content: String) -> Result<LaunchConfig> {
    let source = &root.frozen.predecessor;
    Ok(LaunchConfig {
        query: content,
        title: source.title.clone(),
        agent_role: source.agent_role.clone(),
        epic_spawn_ordinal: source.epic_spawn_ordinal,
        working_dir: Some(source.working_dir.clone()),
        provider: Some(root.frozen.launch.provider),
        model: Some(root.frozen.launch.model.clone()),
        configured_context_window: None,
        max_turns: None,
        system_prompt: None,
        resume_session_id: None,
        session_kind: Some(SessionKind::Standard),
        project_id: Some(root.project_id),
        rsi_session_id: Some(root.candidate_session_id),
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: Some(root.predecessor_session_id),
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: source.workflow_id,
        workflow_id_override: source.workflow_id_override,
        max_retries: source.max_retries,
        group_id: source.group_id,
        parent_id: None,
        effort: root.frozen.launch.effort.clone(),
        issue_identifier: source.issue_identifier.clone(),
        issue_url: source.issue_url.clone(),
        issue_tracker_id: source.issue_tracker_id.clone(),
        scheduled_job_id: None,
        model_invocation_owner: None,
        model_invocation_dedup_key: Some(root.invocation_dedup_key()),
        model_invocation_request_fingerprint: Some(root.invocation_fingerprint()?),
        skip_project_model_default: true,
        model_invocation_purpose:
            rsi_common::model_control::ModelInvocationPurpose::SessionRotateChild,
        sandbox: Some(rsi_common::types::SandboxSpec {
            kind: Some(rsi_common::types::SandboxKind::GitWorktree),
            branch: None,
        }),
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: source.is_eval,
        skip_context_pipeline: source.is_eval,
        capability_class: source.capability_class,
        tags: source.tags.clone(),
        topology_node_id: source.topology_node_id.clone(),
        topology_iteration: source.topology_iteration,
        closure_selector: None,
    })
}

fn completed_row(session: rsi_common::types::Session) -> CompletedSession {
    CompletedSession {
        session,
        events: Vec::new(),
        turn_metrics: Vec::new(),
        retry_cancel: None,
        retry_fired_at: None,
        superseded_by_retry: None,
        events_hydrated: false,
    }
}
