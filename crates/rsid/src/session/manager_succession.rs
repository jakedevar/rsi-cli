//! One root succession occurrence, using the ordinary confirmed provider engine.
//! Runtime guards and filesystem proofs never come from caller JSON or receipts.
use super::agent_verbs::AgentControlHandle;
use super::spawn_single_flight::{SpawnGuard, acquire_provider_cwd_admission, acquire_spawn_guard};
use super::types::{LaunchPurpose, ProspectiveAgentTransportWitness};
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
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
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
    pub(super) prospective_a6: ProspectiveAgentTransportWitness,
    pub(super) generation: Arc<AtomicU64>,
}
impl std::fmt::Debug for ManagerSuccessionLaunchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerSuccessionLaunchContext")
            .field("candidate", &self.candidate)
            .finish_non_exhaustive()
    }
}

/// #1166: how long the succession publication keeps retrying a busy custody
/// stripe (releasing every lock between attempts) before surrendering with the
/// typed retryable `root_busy`.
const PUBLICATION_STRIPE_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
const PUBLICATION_STRIPE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(25);

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
        // Same live value the effect recheck uses: a persisted-absent setting
        // with the live choice off must refuse a new occurrence here, before any
        // reservation. An identical-key retry of an already-queued succession
        // still gets its stored receipt (as the Store gives it with the setting
        // persisted off): refusing it would hide an occurrence that will run.
        if self
            .runtime_config
            .as_ref()
            .is_some_and(|config| !config.context_rotation_enabled.load(Ordering::Relaxed))
        {
            return match crate::store_lock::lock(
                &self.store,
                "manager_succession::enqueue_root_manager_succession",
            )
            .await
            .replay_manager_succession(caller, &request)?
            {
                Some(receipt) => Ok(receipt),
                None => Err(refused("manager_succession_rotation_disabled")),
            };
        }
        let predecessor = {
            let store = crate::store_lock::lock(
                &self.store,
                "manager_succession::enqueue_root_manager_succession",
            )
            .await;
            // Hub #3315: a home seat is a portfolio seat with no project
            // manager configuration; it hands its seat on with
            // `AgentManagerAppointChild` on its own node, and no git ever runs
            // in `$HOME` for it. Say so instead of `manager_not_configured`.
            let caller_session = store.get_session(caller)?;
            if caller_session
                .as_ref()
                .and_then(|session| session.project_id)
                .is_some_and(rsi_common::types::Project::is_home_id)
            {
                return Err(refused(rsi_common::types::Project::HOME_SUCCESSION_REFUSED));
            }
            store.manager_succession_observation(caller)?;
            caller_session.ok_or_else(|| refused("manager_session_unavailable"))?
        };
        let ManagerActionV2::SucceedManager { handoff, .. } = &request.operation else {
            return Err(refused("manager_succession_action_required"));
        };
        let (verified, _) = runtime
            .prepare_manager_handoff(&predecessor, handoff)
            .await?;
        crate::store_lock::lock(
            &self.store,
            "manager_succession::enqueue_root_manager_succession",
        )
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
        let mut claim = crate::store_lock::lock(
            &self.store,
            "manager_succession::execute_manager_succession",
        )
        .await
        .claim_manager_succession(action)?;
        let predecessor = claim.reservation.predecessor_session_id;
        let guard = acquire_spawn_guard(predecessor).await;
        // A terminal DB row is not a physical drain proof. Never interrupt the
        // predecessor from this tool/reconciler, including a late monitor drain.
        if self.active.read().await.contains_key(&predecessor) {
            return crate::store_lock::lock(
                &self.store,
                "manager_succession::execute_manager_succession",
            )
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
        // UpdateDaemonConfig publishes to RuntimeConfig before persisting the
        // setting. Match the live value used by launch_session; the Store still
        // rechecks the persisted gate before effects and authority publication.
        if !self
            .runtime_config
            .context_rotation_enabled
            .load(Ordering::Relaxed)
        {
            return Err(refused("manager_succession_rotation_disabled"));
        }
        let orphan_store = std::sync::Arc::clone(&self.store);
        let turns = crate::store_lock::lock(
            &orphan_store,
            "manager_succession::execute_manager_succession",
        )
        .await
        .list_active_provider_turn_custody()?;
        tokio::task::spawn_blocking(move || super::reaper::reap_orphans_for_session(predecessor, turns))
            .await
            .map_err(|_| refused("manager_succession_predecessor_unsettled"))?
            .map_err(|error| {
                tracing::warn!(session_id=%predecessor, %error, "manager predecessor drain remains unproved");
                refused("manager_succession_predecessor_unsettled")
            })?;
        let (source, observed_invocation) = {
            let store = crate::store_lock::lock(
                &self.store,
                "manager_succession::execute_manager_succession",
            )
            .await;
            let source = store
                .get_session(predecessor)?
                .ok_or_else(|| refused("manager_session_unavailable"))?;
            (source, store.session_model_invocation_id(predecessor)?)
        };
        // #2667: witness the invocation the drained predecessor holds NOW. A
        // turn it took after queueing the handoff (a decision-class notice is
        // never held back) is accepted by the store within its bound; any
        // other change is still refused there against this observation.
        let proof = ManagerPredecessorSettledWitness::after_checked_drain(
            &source,
            observed_invocation,
            self.program_run_boot_id,
        )?;
        claim = crate::store_lock::lock(
            &self.store,
            "manager_succession::execute_manager_succession",
        )
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
            prospective_a6: ProspectiveAgentTransportWitness::default(),
            generation: Arc::new(AtomicU64::new(0)),
        };
        let disabled = source.rotation_disabled_at.is_some();
        if let Err(error) = self
            .boxed_launch_session_with_retry_admission(
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
        let rows = crate::store_lock::lock(
            &self.store,
            "manager_succession::reconcile_manager_succession_cleanup",
        )
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
        *claim = crate::store_lock::lock(&self.runtime.store, "manager_succession::refresh")
            .await
            .claim_manager_succession(&claim.action)?;
        Ok(())
    }
    pub(super) async fn source_gate(&self) -> Result<()> {
        let claim = self.claim().await;
        // #289: the store gate judges a Claude effort by the CLI's live
        // ladder, so read it once if this daemon has not yet, as a launch
        // does. Unit tests never probe the ambient CLI.
        #[cfg(not(test))]
        ensure_successor_effort_ladders(&claim.reservation.frozen.launch).await;
        self.runtime
            .custody
            .prepare_manager_handoff(
                &claim.reservation.frozen.predecessor,
                &claim.reservation.frozen.handoff,
            )
            .await?;
        if let Some(custody) = claim.reservation.candidate_custody.as_ref() {
            let candidate =
                crate::store_lock::lock(&self.runtime.store, "manager_succession::source_gate")
                    .await
                    .get_session(self.candidate)?
                    .ok_or_else(|| refused("manager_succession_candidate_missing"))?;
            self.runtime
                .custody
                .authenticate_manager_candidate(&candidate, custody)
                .await?;
        }
        crate::store_lock::lock(&self.runtime.store, "manager_succession::source_gate")
            .await
            .manager_succession_effect_gate(&claim)
    }
    pub(super) async fn claim_effect(&self) -> Result<()> {
        self.source_gate().await?;
        let mut claim = self.claim.lock().await;
        *claim = crate::store_lock::lock(&self.runtime.store, "manager_succession::claim_effect")
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
        let binding = self
            .prospective_a6
            .current_binding()
            .await
            .ok_or_else(|| refused("manager_succession_token_changed"))?;
        // The commit authenticates and writes under the active map, the Store and
        // the token registry, so it must never wait for a custody stripe there: a
        // busy stripe returns the typed retryable `root_busy` with everything
        // released and the whole (pre-write) critical section is retried after an
        // asynchronous back-off (#1166).
        let deadline = tokio::time::Instant::now() + PUBLICATION_STRIPE_BUDGET;
        let (predecessor, receipt) = loop {
            match self.commit_publication(&claim, generation, binding).await {
                Err(error)
                    if crate::store::custody_lock_order::is_lock_order_busy_error(&error)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(PUBLICATION_STRIPE_BACKOFF).await;
                }
                result => break result?,
            }
        };
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
        // #185: the successor is published and its handoff was verified at
        // the source gate above; prune the lineage's aged-out handoff refs off
        // this path, so publication never waits on that git.
        let store = self.runtime.store.clone();
        let live = self.candidate;
        tokio::spawn(async move {
            match prune_manager_handoff_refs(&store, live).await {
                Ok(pruned) if !pruned.is_empty() => {
                    tracing::info!(%live, ?pruned, "pruned aged-out manager handoff refs")
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%live, %error, "manager handoff ref prune failed"),
            }
        });
        Ok(())
    }

    /// The publication's critical section: candidate liveness, token and the
    /// durable commit, under the active map, the Store and the token registry.
    /// Returns the archived predecessor row and the receipt.
    async fn commit_publication(
        &self,
        claim: &ManagerSuccessionClaim,
        generation: u64,
        binding: super::AgentTransportWitness,
    ) -> Result<(rsi_common::types::Session, ManagerActionReceiptV2)> {
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
        let store = crate::store_lock::lock(
            &self.runtime.store,
            "manager_succession::commit_publication",
        )
        .await;
        let mut tokens = self.runtime.tokens.write().await;
        if !tokens.binding_matches(self.candidate, binding) {
            return Err(refused("manager_succession_token_changed"));
        }
        let proof = ManagerSuccessionPublicationWitness::after_provider_established(
            self.candidate,
            claim.reservation.model_invocation_id,
            claim.reservation.launch_attempt_id,
            claim.action.boot_id,
        )?;
        // Store takes the distinct sorted custody shards itself, without waiting.
        // Runtime retains only spawn/cwd/process/token witnesses here, never
        // those shard locks.
        let mut predecessor = store
            .get_session(claim.reservation.predecessor_session_id)?
            .ok_or_else(|| refused("manager_session_unavailable"))?;
        let receipt = store.commit_manager_succession(claim, &proof)?;
        // No fallible post-commit read may abandon the established writer before
        // its monitor takes ownership. This is the exact metadata-only mutation
        // committed above; candidate runtime counters remain intact.
        predecessor.status = SessionStatus::Archived;
        predecessor.updated_at = chrono::Utc::now();
        tokens.revoke_session(predecessor.id);
        drop(tokens);
        drop(store);
        drop(active);
        Ok((predecessor, receipt))
    }
    /// Also used by the deferred task's exit guard. An unknown process keeps a
    /// CleanupRequired record and admitted capacity; it never triggers resend.
    pub(super) async fn fail_and_settle(&self) -> Result<()> {
        let claim = self.claim().await;
        let root = {
            let store =
                crate::store_lock::lock(&self.runtime.store, "manager_succession::fail_and_settle")
                    .await;
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
            let store = crate::store_lock::lock(&self.store, "manager_succession::cleanup").await;
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
                tracked
                    .interrupt_source
                    .get_or_insert(crate::terminal_cause::InterruptSource::ManagerAction);
                if let Some(process) = tracked.process.as_mut() {
                    process.kill().await?;
                    if process.is_alive() {
                        return Err(refused("manager_succession_cleanup_unsettled"));
                    }
                }
            }
        }
        let orphan_store = std::sync::Arc::clone(&self.store);
        let turns = crate::store_lock::lock(&orphan_store, "manager_succession::cleanup")
            .await
            .list_active_provider_turn_custody()?;
        tokio::task::spawn_blocking(move || super::reaper::reap_orphans_for_session(id, turns))
            .await
            .map_err(|_| refused("manager_succession_cleanup_unsettled"))?
            .map_err(|error| {
                tracing::warn!(session_id=%id, %error, "manager candidate process settlement remains unproved");
                refused("manager_succession_cleanup_unsettled")
            })?;
        if crate::store_lock::lock(&self.store, "manager_succession::cleanup")
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
            let store = crate::store_lock::lock(&self.store, "manager_succession::cleanup").await;
            if store.get_session(id)?.is_some() {
                store.set_session_terminal_status(
                    id,
                    SessionStatus::Failed,
                    "manager_succession_establishment_uncertain",
                )?;
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
        match crate::store_lock::lock(&self.store, "manager_succession::cleanup")
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
        completion_gates: None,
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
        tool_policy: None,
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
        structured_handoff: false,
        exec_target: rsi_common::exec_target::ExecTarget::Host,
        bench_profile: None,
        loop_knobs: rsi_common::bench_profile::LoopKnobs::default(),
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
        store_writes: Default::default(),
    }
}

/// #289: one unbilled `initialize` probe before a Claude successor seat's
/// off-list effort (`ultracode`) meets the store gate; free otherwise
/// (`ClaudeClient::ensure_effort_ladders_for`).
#[cfg(not(test))]
async fn ensure_successor_effort_ladders(launch: &ManagerLaunchChoiceV2) {
    if launch.provider != SessionProvider::Claude {
        return;
    }
    let Ok(client) = crate::claude::ClaudeClient::for_discovery() else {
        return;
    };
    Box::pin(client.ensure_effort_ladders_for(launch.effort.as_deref())).await;
}

/// #185: how many published predecessors of a seat lineage keep their private
/// handoff ref (`refs/rsi/manager-handoff/<id>`, #172). Two: the handoff the
/// live seat was just admitted from (it, a reviewer or a recovery may re-read
/// it) and the one before it, for one hop of audit; older handoffs stay in the
/// transcripts the predecessor-seat read serves (#118), so their refs go.
pub(crate) const MANAGER_HANDOFF_REFS_KEPT: usize = 2;

/// #185: after `live` was published, delete the handoff refs of its lineage's
/// predecessors older than [`MANAGER_HANDOFF_REFS_KEPT`]. Only a sandboxless
/// hop (the one kind whose handoff the daemon read at the ref) is considered,
/// matched by its session id and recorded commit from the store; the ref is
/// deleted only while it still names that commit, in the hop's own recorded
/// working directory, never in `$HOME` or the home project. Best effort:
/// a repository that cannot be read keeps its refs. Returns the pruned ids.
pub(crate) async fn prune_manager_handoff_refs(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    live: Uuid,
) -> Result<Vec<Uuid>> {
    let mut cursor = ManagerHandoffLineageCursor::new(live);
    let mut kept = MANAGER_HANDOFF_REFS_KEPT;
    let mut pruned = Vec::new();
    let user_home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    loop {
        // #505: continue behind the oldest hop of each batch. Bound each lock
        // hold to 256 hops, then release it before Git and the next batch.
        let (lineage, home) = {
            let store =
                crate::store_lock::lock(store, "manager_succession::prune_handoff_refs").await;
            let lineage =
                store.manager_handoff_lineage_page(&mut cursor, MANAGER_HANDOFF_LINEAGE_BATCH)?;
            let home = store
                .get_project(rsi_common::types::Project::HOME_ID)?
                .and_then(|home| home.path);
            (lineage, home)
        };
        if lineage.is_empty() {
            return Ok(pruned);
        }
        let skip = kept.min(lineage.len());
        kept -= skip;
        let mut by_repo: HashMap<std::path::PathBuf, Vec<(Uuid, String)>> = HashMap::new();
        for hop in lineage.into_iter().skip(skip) {
            let (Some(dir), Some(commit), true, false) = (
                hop.working_dir,
                hop.source_commit,
                hop.sandboxless,
                hop.protected,
            ) else {
                continue;
            };
            if !dir.is_absolute() || home.as_ref() == Some(&dir) || user_home.as_ref() == Some(&dir)
            {
                continue;
            }
            by_repo
                .entry(dir)
                .or_default()
                .push((hop.session_id, commit));
        }
        if !by_repo.is_empty() {
            let batch = tokio::task::spawn_blocking(move || {
                let mut pruned = Vec::new();
                for (dir, recorded) in by_repo {
                    match crate::sandbox::git_worktree::prune_manager_handoff_refs(&dir, &recorded)
                    {
                        Ok(ids) => pruned.extend(ids),
                        Err(error) => {
                            tracing::warn!(dir=%dir.display(), %error, "manager handoff refs kept")
                        }
                    }
                }
                pruned
            })
            .await
            .map_err(|error| {
                crate::error::DaemonError::Process(format!("handoff ref prune: {error}"))
            })?;
            pruned.extend(batch);
        }
        // Even a page with no eligible refs yields before reacquiring Store.
        tokio::task::yield_now().await;
    }
}
