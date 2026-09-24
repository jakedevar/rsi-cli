//! Scoped manager action coordinator. Admission is synchronous and durable;
//! only this bounded reconciler owns physical effects. It never invokes a lead
//! Agent verb with a substituted identity.
use super::agent_verbs::AgentControlHandle;
use super::types::ManagerActionLaunchContext;
use super::{CompletedSession, SessionManager};
use crate::bus::DaemonEvent;
use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use crate::store::harness_manager_v2::refused;
use crate::store::manager_actions::{
    ManagerActionClaimV2, ManagerActionOriginV2, action_epic, action_fence,
};
use rsi_common::harness_manager_v2::*;
use rsi_common::types::{Session, SessionProvider, SessionStatus};
use std::sync::{LazyLock, atomic::Ordering};
use uuid::Uuid;

mod recovery;

static RECONCILE: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));
const BATCH: usize = 4;

impl AgentControlHandle {
    pub async fn agent_manager_prepare_control(
        &self,
        caller: Uuid,
        request: AgentManagerPrepareControlRequestV2,
    ) -> Result<ManagerPreparedActionReceiptV2> {
        self.store
            .lock()
            .await
            .prepare_manager_action(caller, request)
    }

    pub async fn agent_manager_commit_prepared_control(
        &self,
        caller: Uuid,
        request: AgentManagerCommitPreparedControlRequestV2,
    ) -> Result<ManagerPreparedActionCommitResultV2> {
        self.store
            .lock()
            .await
            .commit_prepared_manager_action(caller, request)
    }

    pub async fn agent_manager_get_action(
        &self,
        caller: Uuid,
        request: AgentManagerGetActionRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        self.store
            .lock()
            .await
            .manager_action_receipt_for_caller(caller, request)
    }

    pub async fn agent_manager_control(
        &self,
        caller: Uuid,
        request: AgentManagerControlRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        if matches!(request.operation, ManagerActionV2::SucceedManager { .. }) {
            return self.enqueue_root_manager_succession(caller, request).await;
        }
        if matches!(request.operation, ManagerActionV2::Integrate { .. }) {
            return self
                .store
                .lock()
                .await
                .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller }, request);
        }
        self.store
            .lock()
            .await
            .enqueue_manager_action(ManagerActionOriginV2::Agent { caller }, request)
    }
}

impl SessionManager {
    /// Must be called explicitly after runtime restoration and before normal
    /// reconciliation. A lost running claim is surfaced as uncertain, even if
    /// its durable provider row is missing. Missing evidence is not permission.
    pub async fn reconcile_manager_actions_startup(&self) -> Result<usize> {
        self.reconcile_manager_actions_once().await
    }

    pub async fn reconcile_manager_actions_once(&self) -> Result<usize> {
        let Ok(_flight) = RECONCILE.try_lock() else {
            return Ok(0);
        };
        let mut count = self
            .store
            .lock()
            .await
            .recover_abandoned_manager_action_claims()?;
        // RME-S2A-003: Reconcile uncertain integrate claims against actual
        // refs so a crash after publish can be settled and the singleton
        // unblocked without duplicate publication.
        count += self.reconcile_uncertain_integrate_claims().await?;
        // K2 (#380): settle uncertain pause/assign actions whose exact
        // predecessor is terminal and process-absent before claiming, so an
        // exact queued resume/repair is no longer fenced by a visible effect.
        count += self.reconcile_uncertain_lead_actions().await?;
        for _ in 0..BATCH {
            let claim = self
                .store
                .lock()
                .await
                .claim_manager_action(self.program_run_boot_id)?;
            let Some(claim) = claim else { break };
            if let Err(error) = self.execute_manager_action(&claim).await {
                let code = safe_action_error(&error);
                let is_integrate = matches!(claim.action(), ManagerActionV2::Integrate { .. });
                // RME-S2A-003: For integrate actions with effect_started,
                // reconcile against actual refs after dropping the store
                // lock (git_ref is async). For all other actions, finish
                // immediately inside the store lock.
                let needs_integrate_reconcile = {
                    let store = self.store.lock().await;
                    let op = store.manager_action_operation(claim.id())?;
                    let Some(op) = op else {
                        tracing::warn!(operation_id=%claim.id(),error=%error,
                            "manager action vanished during error handling");
                        continue;
                    };
                    let effect_started = op.effect_started;
                    let needs_reconcile = is_integrate
                        && effect_started
                        && op.receipt.state == ManagerActionStateV2::Running;
                    if !needs_reconcile {
                        if op.receipt.state == ManagerActionStateV2::Running {
                            let state = if effect_started {
                                ManagerActionStateV2::Uncertain
                            } else if matches!(
                                code,
                                "manager_v2_scope_changed"
                                    | "manager_v2_policy_changed"
                                    | "manager_v2_capability_denied"
                                    | "manager_v2_epic_out_of_scope"
                                    | "manager_v2_container_out_of_scope"
                            ) {
                                ManagerActionStateV2::Revoked
                            } else {
                                ManagerActionStateV2::Blocked
                            };
                            store.finish_manager_action(&claim, state, code)?;
                        }
                    }
                    needs_reconcile
                };
                if needs_integrate_reconcile {
                    // Same-boot path: the claim is still Running with the
                    // original boot_id, so finish_manager_action works.
                    // Use settle_integrate_claim only for lost-boot
                    // recovery where the row is already terminal Uncertain.
                    let target_ref: String = match claim.action() {
                        ManagerActionV2::Integrate { target_ref, .. } => target_ref.clone(),
                        _ => unreachable!("checked above"),
                    };
                    let repo = self
                        .resolve_project_working_dir(claim.operation.project_id)
                        .await
                        .ok();
                    let resolved = if let Some(r) = repo {
                        git_ref(&r, &target_ref).await
                    } else {
                        None
                    };
                    let store = self.store.lock().await;
                    let state = if let Some(ref_oid) = resolved {
                        match store.reconcile_integrate_claim(&claim, &ref_oid)? {
                            ManagerActionStateV2::Succeeded => ManagerActionStateV2::Succeeded,
                            ManagerActionStateV2::Failed => ManagerActionStateV2::Failed,
                            _ => ManagerActionStateV2::Uncertain,
                        }
                    } else {
                        ManagerActionStateV2::Uncertain
                    };
                    let outcome = match state {
                        ManagerActionStateV2::Succeeded => "integrated_crash_reconciled",
                        ManagerActionStateV2::Failed => "not_integrated_crash_reconciled",
                        _ => "execution_owner_lost_unconfirmed",
                    };
                    store.finish_manager_action(&claim, state, outcome)?;
                }
                tracing::warn!(operation_id=%claim.id(),error=%error,"manager lifecycle action did not establish a confirmed result");
            }
            match self
                .store
                .lock()
                .await
                .reconcile_manager_action_notice(claim.id())
            {
                Ok(Some(job_id)) => self
                    .event_bus
                    .publish(DaemonEvent::ManagerNoticeQueued { job_id }),
                Ok(None) => {}
                Err(error) => tracing::warn!(operation_id=%claim.id(),%error,
                    "manager action notice reconciliation deferred"),
            }
            count += 1;
        }
        let (reviews_changed, review_notice_jobs) = self
            .store
            .lock()
            .await
            .reconcile_manager_review_assignments_once()?;
        count += reviews_changed;
        for job_id in review_notice_jobs {
            self.event_bus
                .publish(DaemonEvent::ManagerNoticeQueued { job_id });
        }
        count += self.reconcile_manager_succession_cleanup().await?;
        Ok(count)
    }

    pub(super) async fn execute_manager_action(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        use ManagerActionV2::*;
        if matches!(claim.action(), SucceedManager { .. }) {
            return self.execute_manager_succession(claim).await;
        }
        if matches!(claim.action(), Integrate { .. } | OperatorCall { .. }) {
            // One shared await of a heap future built by a non-async helper:
            // K14's delegated executor adds no await site, state or poll-frame
            // temporaries here (debug test threads run on a 2 MiB stack).
            return self.boxed_dispatched_manager_action(claim).await;
        }
        self.check_manager_action_runtime(claim, false).await?;
        match claim.action() {
            SucceedManager { .. } => unreachable!("root succession dispatched above"),
            Integrate { .. } => unreachable!("integrate dispatched above"),
            OperatorCall { .. } => unreachable!("operator call dispatched above"),
            SettleUncertainAction { operation_id, .. } => {
                self.execute_manager_settle(claim, *operation_id).await?;
            }
            RetireLeadContinuations { .. } => {
                self.execute_manager_retire(claim).await?;
            }
            CreateContainer {
                parent_id,
                kind,
                name,
                tags,
            } => {
                let working_dir = if let Some(parent) = parent_id {
                    self.store
                        .lock()
                        .await
                        .get_session(*parent)?
                        .ok_or_else(|| refused("manager_v2_target_unavailable"))?
                        .working_dir
                } else {
                    self.resolve_project_working_dir(claim.operation.project_id)
                        .await?
                };
                let mut normalized = tags
                    .iter()
                    .map(|t| {
                        rsi_common::normalize_tag(t).map_err(|_| refused("manager_v2_invalid_tags"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                normalized.sort();
                normalized.dedup();
                let params = rsi_common::rpc::CreateContainerParams {
                    kind: *kind,
                    name: name.clone(),
                    parent_id: *parent_id,
                    project_id: Some(claim.operation.project_id),
                    tags: normalized.clone(),
                    topology_id: None,
                };
                let session = super::hierarchy_ops::build_container_session(
                    claim
                        .operation
                        .context
                        .target_session_id
                        .expect("reserved container"),
                    params,
                    working_dir,
                    normalized,
                );
                let (session, _) = self
                    .store
                    .lock()
                    .await
                    .apply_manager_container_action(claim, Some(&session))?;
                self.project_manager_container(session, claim.action())
                    .await;
            }
            ArchiveContainer { .. } | RestoreContainer { .. } => {
                self.execute_manager_container_cascade(claim).await?;
            }
            ArchiveSession { .. } | RestoreSession { .. } | UpdateSession { .. } => {
                self.execute_manager_session_action(claim).await?;
            }
            UpdateContainer { container_id, .. } | DeleteContainer { container_id, .. } => {
                // K2 authority gate: Delete clears the container's lead.
                let _lead_guards = self.container_lead_guards(*container_id).await?;
                let (session, _) = self
                    .store
                    .lock()
                    .await
                    .apply_manager_container_action(claim, None)?;
                self.project_manager_container(session, claim.action())
                    .await;
            }
            ResumeLead { message, .. } => {
                self.continue_manager_action(claim.clone(), message.clone())
                    .await?;
                // The continuation holds the guard through installation. Its
                // success is a confirmed start, not proof of task completion.
                self.store.lock().await.finish_manager_action(
                    claim,
                    ManagerActionStateV2::Succeeded,
                    "lead_resumed",
                )?;
            }
            PauseLead { .. } => {
                let predecessor = action_fence(claim.action())
                    .and_then(|f| f.lead_session_id)
                    .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
                let _guard = super::spawn_single_flight::acquire_spawn_guard(predecessor).await;
                self.check_manager_action_runtime(claim, false).await?;
                self.settle_manager_predecessor(claim, predecessor).await?;
                self.check_manager_action_runtime(claim, false).await?;
                self.store.lock().await.finish_manager_action(
                    claim,
                    ManagerActionStateV2::Succeeded,
                    "lead_paused",
                )?;
            }
            CreateSession { .. } | ReplaceLead { .. } | RetryLead { .. } => {
                let predecessor = action_fence(claim.action()).and_then(|f| f.lead_session_id);
                // Held until CAS: an operator/worker continuation cannot restart
                // the old process between quiescence and assignment.
                let _predecessor_guard = if let Some(id) = predecessor {
                    Some(super::spawn_single_flight::acquire_spawn_guard(id).await)
                } else {
                    None
                };
                self.check_manager_action_runtime(claim, false).await?;
                if let Some(id) = predecessor {
                    self.settle_manager_predecessor(claim, id).await?;
                }
                let frozen = claim
                    .operation
                    .context
                    .source
                    .as_ref()
                    .ok_or_else(|| refused("manager_v2_source_unavailable"))?;
                let source = self
                    .store
                    .lock()
                    .await
                    .get_session(frozen.session_id)?
                    .ok_or_else(|| refused("manager_v2_source_unavailable"))?;
                let runtime = self.custody_execution_runtime();
                let fork = if frozen.historical_commit {
                    runtime
                        .prepare_manager_action_fork_at(&source, &frozen.commit)
                        .await?
                } else {
                    runtime.prepare_manager_action_fork(&source).await?
                };
                if source.working_dir != frozen.working_dir
                    || source.sandbox_root != frozen.sandbox_root
                    || fork.fork_commit() != frozen.commit
                {
                    return Err(refused("manager_v2_source_changed"));
                }
                let config = manager_launch_config(claim, &source)?;
                let target = claim
                    .operation
                    .context
                    .target_session_id
                    .expect("reserved candidate");
                let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                self.launch_manager_action_candidate(
                    config,
                    ManagerActionLaunchContext {
                        claim: claim.clone(),
                        fork,
                        generation: generation.clone(),
                    },
                )
                .await?;
                let generation = generation.load(Ordering::Acquire);
                let result = async {
                    self.confirm_manager_candidate(claim, target, generation)
                        .await?;
                    if predecessor.is_some() {
                        self.commit_manager_assignment(claim, Some(generation))
                            .await
                    } else {
                        self.store
                            .lock()
                            .await
                            .commit_manager_created_session(claim)
                    }
                }
                .await;
                if result.is_err() {
                    // Keep the allocated source and durable uncertain result;
                    // stop a losing candidate instead of leaving an unowned writer.
                    self.stop_manager_candidate(target, generation).await?;
                }
                result?;
            }
            AssignLead { session_id, .. } => {
                let predecessor = action_fence(claim.action()).and_then(|f| f.lead_session_id);
                let mut ids = predecessor
                    .into_iter()
                    .chain(*session_id)
                    .collect::<Vec<_>>();
                ids.sort();
                ids.dedup();
                let mut guards = Vec::new();
                for id in ids {
                    guards.push(super::spawn_single_flight::acquire_spawn_guard(id).await);
                }
                self.check_manager_action_runtime(claim, false).await?;
                if predecessor != *session_id {
                    if let Some(old) = predecessor {
                        self.settle_manager_predecessor(claim, old).await?;
                    }
                }
                self.commit_manager_assignment(claim, None).await?;
            }
        }
        Ok(())
    }

    /// Guard of the container's current lead (K2 authority gate).
    async fn container_lead_guards(
        &self,
        container_id: Uuid,
    ) -> Result<Vec<super::spawn_single_flight::SpawnGuard>> {
        let lead = self
            .store
            .lock()
            .await
            .get_session(container_id)?
            .and_then(|container| container.lead_session_id);
        super::hierarchy_ops::lead_mutation_guards(
            &lead.into_iter().collect::<Vec<_>>(),
            super::hierarchy_ops::LeadGuardMode::Acquire,
        )
        .await
    }

    async fn execute_manager_container_cascade(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        match claim.action() {
            ManagerActionV2::ArchiveContainer { container_id, .. } => {
                let mut tree = self
                    .store
                    .lock()
                    .await
                    .manager_action_container_tree_ids(*container_id)?;
                tree.sort();
                let mut spawn_guards = Vec::with_capacity(tree.len());
                for id in &tree {
                    spawn_guards.push(super::spawn_single_flight::acquire_spawn_guard(*id).await);
                }
                {
                    let active = self.active.read().await;
                    if tree.iter().any(|id| active.contains_key(id)) {
                        return Err(refused("container_not_terminal"));
                    }
                }
                {
                    let completed = self.completed.read().await;
                    if tree.iter().any(|id| {
                        completed.get(id).is_some_and(|cs| {
                            cs.retry_cancel.is_some()
                                || cs.retry_fired_at.is_some()
                                || cs.superseded_by_retry.is_some()
                        })
                    }) {
                        return Err(refused("manager_v2_human_or_recovery_owner"));
                    }
                }
                let (_, ids) = self
                    .store
                    .lock()
                    .await
                    .apply_manager_container_action(claim, None)?;
                self.project_manager_cascade(ids, false).await?;
                drop(spawn_guards);
            }
            ManagerActionV2::RestoreContainer { container_id, .. } => {
                let recorded = self
                    .store
                    .lock()
                    .await
                    .manager_action_cascade_archive_ids(*container_id)?;
                let Some(record) = recorded else {
                    // K2 authority gate: a restore clears the container lead.
                    let _lead_guards = self.container_lead_guards(*container_id).await?;
                    let (session, _) = self
                        .store
                        .lock()
                        .await
                        .apply_manager_container_action(claim, None)?;
                    self.project_manager_container(session, claim.action())
                        .await;
                    return Ok(());
                };
                let mut ids = record.ids;
                if !ids.contains(container_id) {
                    ids.push(*container_id);
                }
                ids.sort();
                ids.dedup();
                let mut spawn_guards = Vec::with_capacity(ids.len());
                for id in &ids {
                    spawn_guards.push(super::spawn_single_flight::acquire_spawn_guard(*id).await);
                }
                {
                    let active = self.active.read().await;
                    if ids.iter().any(|id| active.contains_key(id)) {
                        return Err(refused("manager_v2_target_active"));
                    }
                }
                let (_, transitioned) = self
                    .store
                    .lock()
                    .await
                    .apply_manager_container_action(claim, None)?;
                self.project_manager_cascade(transitioned, true).await?;
                drop(spawn_guards);
            }
            _ => return Err(refused("manager_v2_not_container_action")),
        }
        Ok(())
    }

    /// Single-leaf housekeeping. The spawn guard keeps an operator/worker
    /// continuation from starting the target between these checks and the
    /// store transaction; in-memory effects mirror the operator handlers.
    async fn execute_manager_session_action(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        let id = claim
            .action()
            .housekeeping_session()
            .ok_or_else(|| refused("manager_v2_not_session_action"))?;
        let _spawn_guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
        // Archive/restore require a quiescent row; metadata edits may target a
        // running leaf exactly as the operator handlers do.
        if !matches!(claim.action(), ManagerActionV2::UpdateSession { .. })
            && self.active.read().await.contains_key(&id)
        {
            return Err(refused("manager_v2_session_active"));
        }
        match claim.action() {
            ManagerActionV2::ArchiveSession { .. } => {
                if self.completed.read().await.get(&id).is_some_and(|cs| {
                    cs.retry_cancel.is_some()
                        || cs.retry_fired_at.is_some()
                        || cs.superseded_by_retry.is_some()
                }) {
                    return Err(refused("manager_v2_human_or_recovery_owner"));
                }
                self.store
                    .lock()
                    .await
                    .apply_manager_session_action(claim)?;
                self.project_manager_cascade(vec![id], false).await?;
            }
            ManagerActionV2::RestoreSession { .. } => {
                self.store
                    .lock()
                    .await
                    .apply_manager_session_action(claim)?;
                self.project_manager_cascade(vec![id], true).await?;
            }
            ManagerActionV2::UpdateSession { patch, .. } => {
                let session = self
                    .store
                    .lock()
                    .await
                    .apply_manager_session_action(claim)?;
                let project = |target: &mut Session| {
                    if patch.title.is_some() {
                        target.title.clone_from(&session.title);
                    }
                    if patch.description.is_some() {
                        target.description.clone_from(&session.description);
                    }
                    if patch.rating.is_some() {
                        target.rating = session.rating;
                    }
                    if patch.active_task.is_some() {
                        target.active_task.clone_from(&session.active_task);
                    }
                    if patch.label.is_some() {
                        target.group_id = session.group_id;
                    }
                    if patch.tags.is_some() {
                        target.tags.clone_from(&session.tags);
                        target.tag.clone_from(&session.tag);
                    }
                    target.updated_at = session.updated_at;
                };
                if let Some(tracked) = self.active.write().await.get_mut(&id) {
                    project(&mut tracked.session);
                }
                if let Some(completed) = self.completed.write().await.get_mut(&id) {
                    project(&mut completed.session);
                }
                self.event_bus.publish(DaemonEvent::SessionMetadataChanged {
                    session_id: id,
                    model: None,
                    pinned_at: None,
                    project_id: None,
                    parent_id: None,
                    lead_session_id: None,
                    testing_needed_at: None,
                    rotation_disabled_at: None,
                    resolved_context_budget: None,
                });
            }
            _ => return Err(refused("manager_v2_not_session_action")),
        }
        Ok(())
    }

    pub(super) async fn project_manager_cascade(
        &self,
        ids: Vec<Uuid>,
        restore: bool,
    ) -> Result<()> {
        for id in ids {
            if restore {
                let store = self.store.clone();
                let (mut session, events, turn_metrics) = tokio::task::spawn_blocking(move || {
                    let store = store.blocking_lock();
                    let session = store
                        .get_session(id)?
                        .ok_or(DaemonError::SessionNotFound(id))?;
                    let events = store.load_events(id)?;
                    let turn_metrics = store.load_turn_metrics(id)?;
                    drop(store);
                    Ok::<_, DaemonError>((session, events, turn_metrics))
                })
                .await
                .map_err(|error| DaemonError::Store(error.to_string()))??;
                session.context_fill_pct =
                    super::monitor::context_fill_pct_from_persisted(&session);
                self.completed.write().await.insert(
                    id,
                    CompletedSession {
                        session,
                        events,
                        turn_metrics,
                        retry_cancel: None,
                        retry_fired_at: None,
                        superseded_by_retry: None,
                        events_hydrated: true,
                    },
                );
                self.event_bus
                    .publish(DaemonEvent::SessionUnarchived { session_id: id });
            } else {
                let mut completed = { self.completed.write().await.remove(&id) };
                if let Some(cancel) = completed.as_mut().and_then(|cs| cs.retry_cancel.take()) {
                    let _ = cancel.send(());
                }
                self.event_bus.publish(DaemonEvent::SessionArchived {
                    session_id: id,
                    projection_id: None,
                });
            }
        }
        Ok(())
    }

    async fn project_manager_container(&self, session: Session, action: &ManagerActionV2) {
        let id = session.id;
        if matches!(
            session.status,
            SessionStatus::Archived | SessionStatus::Deleted
        ) {
            self.completed.write().await.remove(&id);
            self.event_bus
                .publish(if session.status == SessionStatus::Archived {
                    DaemonEvent::SessionArchived {
                        session_id: id,
                        projection_id: None,
                    }
                } else {
                    DaemonEvent::SessionDeleted { session_id: id }
                });
        } else {
            self.completed.write().await.insert(
                id,
                CompletedSession {
                    session: session.clone(),
                    events: Vec::new(),
                    turn_metrics: Vec::new(),
                    retry_cancel: None,
                    retry_fired_at: None,
                    superseded_by_retry: None,
                    events_hydrated: true,
                },
            );
            self.event_bus
                .publish(DaemonEvent::SessionCreated { session });
            if matches!(action, ManagerActionV2::RestoreContainer { .. }) {
                self.event_bus
                    .publish(DaemonEvent::SessionUnarchived { session_id: id });
            }
        }
    }

    /// Called while the actual target spawn guard is held; launch.rs calls it
    /// again just before the provider effect, including deferred AppServer.
    pub(super) async fn check_manager_action_runtime(
        &self,
        claim: &ManagerActionClaimV2,
        effect: bool,
    ) -> Result<()> {
        if matches!(claim.action(), ManagerActionV2::RetryLead { .. })
            && !self.runtime_config.retry_enabled.load(Ordering::Relaxed)
        {
            return Err(refused("manager_v2_retry_disabled"));
        }
        if let Some(lead) = action_fence(claim.action()).and_then(|f| f.lead_session_id) {
            if let Some(cs) = self.completed.read().await.get(&lead) {
                if cs.retry_cancel.is_some()
                    || cs.retry_fired_at.is_some()
                    || cs.superseded_by_retry.is_some()
                    || cs.session.pending_question.is_some()
                    || cs.session.pending_archive
                {
                    return Err(refused("manager_v2_human_or_recovery_owner"));
                }
            }
        }
        if effect {
            manager_action_source_gate(&self.custody_execution_runtime(), &self.store, claim)
                .await?;
        }
        self.store
            .lock()
            .await
            .manager_action_runtime_gate(claim, effect)
    }

    async fn settle_manager_predecessor(
        &self,
        claim: &ManagerActionClaimV2,
        predecessor: Uuid,
    ) -> Result<()> {
        let generation = self
            .active
            .read()
            .await
            .get(&predecessor)
            .map(|s| s.spawn_generation);
        if generation.is_some() {
            self.check_manager_action_runtime(claim, true).await?;
            super::lifecycle::interrupt_active_in_maps(&self.active, predecessor).await?;
            let deadline = tokio::time::Instant::now() + Self::CONTINUE_INTERRUPT_WAIT;
            loop {
                let active = self.active.read().await;
                match active.get(&predecessor) {
                    None => break,
                    Some(s) if Some(s.spawn_generation) != generation => {
                        return Err(refused("manager_v2_predecessor_changed"));
                    }
                    _ => {}
                }
                drop(active);
                if tokio::time::Instant::now() >= deadline {
                    return Err(refused("manager_v2_predecessor_unsettled"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
        // Map removal alone is not an OS-process settlement witness. Reprove
        // the exact daemon-owned cohort to a bounded two-empty-pass fixed point.
        tokio::task::spawn_blocking(move || super::reaper::reap_orphans_for_session(predecessor))
            .await
            .map_err(|_| refused("manager_v2_predecessor_unsettled"))??;
        self.store
            .lock()
            .await
            .manager_action_accept_settled_fence(claim)
    }

    async fn stop_manager_candidate(&self, target: Uuid, generation: u64) -> Result<()> {
        let _guard = super::spawn_single_flight::acquire_spawn_guard(target).await;
        if !self
            .active
            .read()
            .await
            .get(&target)
            .is_some_and(|s| s.spawn_generation == generation)
        {
            return Ok(());
        }
        super::lifecycle::interrupt_active_in_maps(&self.active, target).await?;
        let deadline = tokio::time::Instant::now() + Self::CONTINUE_INTERRUPT_WAIT;
        loop {
            if !self
                .active
                .read()
                .await
                .get(&target)
                .is_some_and(|s| s.spawn_generation == generation)
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(refused("manager_v2_candidate_unconfirmed"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        tokio::task::spawn_blocking(move || super::reaper::reap_orphans_for_session(target))
            .await
            .map_err(|_| refused("manager_v2_candidate_unconfirmed"))??;
        Ok(())
    }

    /// RME-S2A-003: Reconcile uncertain integrate actions by checking
    /// actual refs. If the target matches the stored candidate, the publish
    /// succeeded; if it still matches expected_tip, it did not. This unblocks
    /// the per-target singleton without duplicate publication.
    async fn reconcile_uncertain_integrate_claims(&self) -> Result<usize> {
        // RME-S2A-003: Reconcile uncertain integrate actions by checking
        // actual refs. Uses settle_integrate_claim (CAS) instead of
        // finish_manager_action, which requires a Running claim with the
        // original boot_id — the crashed boot's id is gone.
        let uncertain_ops: Vec<crate::store::manager_actions::ManagerActionOperationV2> = {
            let store = self.store.lock().await;
            let mut stmt = store.conn.prepare(
                "SELECT id FROM harness_manager_v2_operations
                 WHERE kind='lifecycle_action' AND state='uncertain'
                 AND json_extract(payload_json,'$.request.operation.action')='integrate'
                 ORDER BY id LIMIT 16",
            )?;
            let ids: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            ids.into_iter()
                .filter_map(|id| {
                    let oid = uuid::Uuid::parse_str(&id).ok()?;
                    store.manager_action_operation(oid).ok().flatten()
                })
                .collect()
        };
        let mut count = 0;
        for op in uncertain_ops {
            let (target_ref, project_id, manager_session_id, scope_version, actor, operation_id) = {
                let target_ref = match &op.context.request.operation {
                    ManagerActionV2::Integrate { target_ref, .. } => target_ref.clone(),
                    _ => continue,
                };
                let actor = match &op.context.origin {
                    crate::store::manager_actions::ManagerActionOriginV2::Agent { caller } => {
                        Some(*caller)
                    }
                    _ => None,
                };
                (
                    target_ref,
                    op.project_id,
                    op.manager_session_id,
                    op.scope_version,
                    actor,
                    op.receipt.operation_id,
                )
            };
            let repo = self.resolve_project_working_dir(project_id).await?;
            let resolved = git_ref(&repo, &target_ref).await;
            let store = self.store.lock().await;
            if let Some(ref_oid) = resolved {
                match store.reconcile_integrate_claim(
                    &crate::store::manager_actions::ManagerActionClaimV2 {
                        operation: op,
                        boot_id: self.program_run_boot_id,
                    },
                    &ref_oid,
                )? {
                    ManagerActionStateV2::Succeeded => {
                        store.settle_integrate_claim(
                            operation_id,
                            project_id,
                            manager_session_id,
                            scope_version,
                            actor,
                            ManagerActionStateV2::Succeeded,
                            "integrated_crash_reconciled",
                        )?;
                    }
                    ManagerActionStateV2::Failed => {
                        store.settle_integrate_claim(
                            operation_id,
                            project_id,
                            manager_session_id,
                            scope_version,
                            actor,
                            ManagerActionStateV2::Failed,
                            "not_integrated_crash_reconciled",
                        )?;
                    }
                    _ => {} // Still genuinely uncertain; leave it
                }
            }
            count += 1;
        }
        Ok(count)
    }

    /// Heap future for the actions dispatched ahead of the lifecycle match:
    /// `Integrate` (unchanged executor) and K14 `OperatorCall` (runtime gate,
    /// then the delegated executor).
    #[inline(never)]
    fn boxed_dispatched_manager_action<'a>(
        &'a self,
        claim: &'a ManagerActionClaimV2,
    ) -> super::delegated_operator::DelegatedOperatorFuture<'a> {
        if matches!(claim.action(), ManagerActionV2::Integrate { .. }) {
            Box::pin(self.execute_integrate_action(claim))
        } else {
            self.boxed_delegated_operator_call(claim)
        }
    }

    async fn execute_integrate_action(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        use crate::integration::{self, IntegrationConfig, Prepared};
        let (target_ref, expected_tip, source_commit) = match claim.action() {
            ManagerActionV2::Integrate {
                work_key: _,
                target_ref,
                expected_tip,
                source_commit,
            } => (
                target_ref.clone(),
                expected_tip.clone(),
                source_commit.clone(),
            ),
            _ => return Err(refused("manager_v2_not_integrate_action")),
        };
        // Pre-effect gate: verify claim, authority, scope, policy.
        self.store
            .lock()
            .await
            .manager_v2_integrate_runtime_gate(claim, false)?;
        let repo = self
            .resolve_project_working_dir(claim.operation.project_id)
            .await?;
        let config = IntegrationConfig {
            allowed_targets: vec![target_ref.clone()],
            identity: integration::CommitIdentity {
                name: "rsi integration".into(),
                email: "integration@rsi.local".into(),
            },
            git_timeout: std::time::Duration::from_secs(120),
        };
        let scratch_dir = self
            .sandbox_allocator
            .base_dir()
            .join("integration-scratch");
        tokio::fs::create_dir_all(&scratch_dir)
            .await
            .map_err(|e| DaemonError::Process(format!("scratch dir: {e}")))?;
        let prepared = integration::prepare_candidate(
            &config,
            &repo,
            &target_ref,
            &expected_tip,
            &source_commit,
            &scratch_dir,
        )
        .await?;
        match prepared {
            Prepared::AlreadyIntegrated => {
                self.store.lock().await.finish_manager_action(
                    claim,
                    ManagerActionStateV2::Succeeded,
                    "already_integrated",
                )?;
                Ok(())
            }
            Prepared::Candidate(candidate) => {
                // D20: Recheck acceptance at the effect boundary. A crash after
                // effect_started is Uncertain; a failed acceptance recheck
                // never marks effect_started and must clean up the candidate.
                //
                // The guard from `manager_v2_integrate_runtime_gate` must drop
                // before `discard_candidate` awaits so the MutexGuard does not
                // live across the async cleanup.
                let gate_result = {
                    let store = self.store.lock().await;
                    store.manager_v2_integrate_runtime_gate(claim, true)
                };
                if let Err(error) = gate_result {
                    let _ = integration::discard_candidate(&config, &repo, &candidate.handle).await;
                    return Err(error);
                }
                // RME-S2A-002: Persist the candidate OID so crash recovery can
                // determine whether publish succeeded.
                {
                    let store = self.store.lock().await;
                    store.persist_integrate_candidate(claim, &candidate.oid)?;
                }
                // RME-S2A-002: Verify target ref hasn't moved between prepare
                // and publish. The engine's publish also checks this, but the
                // durable custody record provides evidence for crash recovery.
                let resolved_ref = git_ref(&repo, &target_ref).await;
                if let Some(ref_oid) = resolved_ref {
                    let custody_result = {
                        let store = self.store.lock().await;
                        store.verify_integrate_custody(claim, &ref_oid)
                    };
                    if let Err(error) = custody_result {
                        let _ =
                            integration::discard_candidate(&config, &repo, &candidate.handle).await;
                        return Err(error);
                    }
                }
                let publish_result = integration::publish(
                    &config,
                    &repo,
                    &target_ref,
                    &expected_tip,
                    &candidate.oid,
                )
                .await;
                // Best-effort cleanup of the candidate worktree.
                let _ = integration::discard_candidate(&config, &repo, &candidate.handle).await;
                publish_result?;
                self.store.lock().await.finish_manager_action(
                    claim,
                    ManagerActionStateV2::Succeeded,
                    "source_integrated",
                )?;
                Ok(())
            }
        }
    }

    async fn confirm_manager_candidate(
        &self,
        claim: &ManagerActionClaimV2,
        target: Uuid,
        generation: u64,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if let Some(tracked) = self.active.write().await.get_mut(&target) {
                if tracked.spawn_generation != generation || generation == 0 {
                    return Err(refused("manager_v2_candidate_changed"));
                }
                if tracked.interrupt_requested {
                    return Err(refused("manager_v2_candidate_cancelled"));
                }
                if tracked.process.as_mut().is_some_and(|process| {
                    manager_provider_established(tracked.session.provider, process)
                }) {
                    return Ok(());
                }
            } else {
                return Err(refused("manager_v2_candidate_unconfirmed"));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(refused("manager_v2_candidate_unconfirmed"));
            }
            // Scope revocation stops the candidate; it never wins the lead CAS.
            let scope_gate = {
                self.store
                    .lock()
                    .await
                    .manager_action_runtime_gate(claim, false)
            };
            if let Err(error) = scope_gate {
                return Err(error);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    async fn commit_manager_assignment(
        &self,
        claim: &ManagerActionClaimV2,
        generation: Option<u64>,
    ) -> Result<()> {
        let require_established = generation.is_some();
        #[cfg(test)]
        if require_established {
            super::launch::pause_controller_candidate_test(
                claim
                    .operation
                    .context
                    .target_session_id
                    .expect("candidate"),
                super::launch::ControllerCandidateTestPhase::BeforeAssignment,
            )
            .await;
        }
        let target = claim.operation.context.target_session_id;
        // Match custody and model invocation while the active incarnation is
        // locked; a stale or merely allocated Starting row cannot win a lead.
        let mut active = self.active.write().await;
        let mut store = self.store.lock().await;
        if let Some(target) = target {
            let candidate = store
                .get_session(target)?
                .ok_or_else(|| refused("manager_v2_candidate_unavailable"))?;
            store.manager_action_human_gate(target)?;
            if candidate.pending_question.is_some()
                || candidate.pending_archive
                || candidate.status == SessionStatus::WaitingApproval
            {
                return Err(refused("manager_v2_human_or_recovery_owner"));
            }
            if require_established {
                let invocation = store
                    .session_model_invocation_id(target)?
                    .ok_or_else(|| refused("manager_v2_candidate_unconfirmed"))?;
                let bound:bool=store.conn.query_row("SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1 AND session_id=?2 AND project_id=?3 AND dedup_key=?4)",rusqlite::params![invocation.to_string(),target.to_string(),claim.operation.project_id.to_string(),format!("manager.action:{}",claim.id())],|r|r.get(0))?;
                if !bound {
                    return Err(refused("manager_v2_candidate_unconfirmed"));
                }
            }
            if let Some(tracked) = active.get_mut(&target) {
                if generation
                    .is_some_and(|expected| expected == 0 || expected != tracked.spawn_generation)
                {
                    return Err(refused("manager_v2_candidate_changed"));
                }
                if tracked.interrupt_requested
                    || !tracked.process.as_mut().is_some_and(|process| {
                        manager_provider_established(tracked.session.provider, process)
                    })
                {
                    return Err(refused("manager_v2_candidate_unconfirmed"));
                }
                let tokens = self.agent_tokens.read().await;
                if tokens.token_for_session(target).is_none() {
                    return Err(refused("manager_v2_candidate_unconfirmed"));
                }
            } else if require_established
                || matches!(
                    candidate.status,
                    SessionStatus::Starting
                        | SessionStatus::Running
                        | SessionStatus::WaitingApproval
                )
            {
                return Err(refused("manager_v2_candidate_unconfirmed"));
            }
            match crate::sandbox::custody::CustodyService::classify(&candidate)? {
                crate::sandbox::custody::CustodyClassification::OrdinaryUnsandboxed => {
                    crate::sandbox::custody::CustodyService::authorize_ordinary(&candidate)?;
                }
                crate::sandbox::custody::CustodyClassification::RequiresPersistedAuthentication => {
                    crate::sandbox::custody::CustodyService::authorize_live(
                        &candidate,
                        &mut store,
                        self.sandbox_allocator.base_dir(),
                        rsi_common::types::SandboxCustodyTransitionV1::Continue,
                    )?;
                }
            }
        }
        let jobs = store.commit_manager_lead_action(claim)?;
        drop(store);
        drop(active);
        for job_id in jobs {
            self.event_bus
                .publish(DaemonEvent::ManagerNoticeQueued { job_id });
        }
        let epic = action_epic(claim.action()).expect("lead action");
        self.set_lead_in_memory(epic, target).await;
        if let Some(target) = target {
            self.mark_lead_session(target).await;
        }
        self.publish_lead_delta(epic, target);
        if let Some(old) = action_fence(claim.action())
            .and_then(|f| f.lead_session_id)
            .filter(|id| Some(*id) != target)
        {
            self.revoke_agent_token_for_session(old).await;
        }
        Ok(())
    }
}

/// Fresh manager launches reauthenticate the predecessor immediately before
/// the effect. Same-session resume has no fork source and keeps dirty custody.
pub(super) async fn manager_action_source_gate(
    runtime: &crate::sandbox::custody::CustodyExecutionRuntime,
    store: &std::sync::Arc<tokio::sync::Mutex<crate::store::Store>>,
    claim: &ManagerActionClaimV2,
) -> Result<()> {
    let Some(frozen) = claim.operation.context.source.as_ref() else {
        return Ok(());
    };
    let source = store
        .lock()
        .await
        .get_session(frozen.session_id)?
        .ok_or_else(|| refused("manager_v2_source_unavailable"))?;
    let fork = if frozen.historical_commit {
        runtime
            .prepare_manager_action_fork_at(&source, &frozen.commit)
            .await?
    } else {
        runtime.prepare_manager_action_fork(&source).await?
    };
    if source.working_dir != frozen.working_dir
        || source.sandbox_root != frozen.sandbox_root
        || fork.fork_commit() != frozen.commit
    {
        return Err(refused("manager_v2_source_changed"));
    }
    if let Some(generation) = frozen.custody_generation {
        if store
            .lock()
            .await
            .live_custody_for_session(source.id)?
            .generation
            != generation as u64
        {
            return Err(refused("manager_v2_source_changed"));
        }
    }
    Ok(())
}

fn manager_provider_established(
    provider: SessionProvider,
    process: &mut super::types::ProviderProcess,
) -> bool {
    if provider == SessionProvider::CodexAppServer {
        // The process is installed only after initialize and thread/start.
        matches!(process, super::types::ProviderProcess::CodexAppServer(_)) && process.is_alive()
    } else {
        super::provider_spawn::installed_provider_confirmation(provider, process).is_some()
    }
}

/// Error-code list extracted so `safe_action_error` stays under the
/// `too_many_lines` threshold. Order matters only for matching priority.
const MANAGER_ACTION_ERROR_CODES: &[&str] = &[
    "manager_succession_action_required",
    "manager_succession_admission_already_exists",
    "manager_succession_admission_required",
    "manager_succession_already_committed",
    "manager_succession_appointment_missing",
    "manager_succession_authority_changed",
    "manager_succession_candidate_changed",
    "manager_succession_candidate_custody_changed",
    "manager_succession_candidate_history",
    "manager_succession_candidate_missing",
    "manager_succession_candidate_unconfirmed",
    "manager_succession_claim_changed",
    "manager_succession_cleanup_changed",
    "manager_succession_cleanup_required",
    "manager_succession_cleanup_unsettled",
    "manager_succession_custody_runtime_required",
    "manager_succession_depth_overflow",
    "manager_succession_distinct_custody_required",
    "manager_succession_effect_unavailable",
    "manager_succession_establishment_changed",
    "manager_succession_establishment_invalid",
    "manager_succession_establishment_uncertain",
    "manager_succession_handoff_blob_changed",
    "manager_succession_handoff_blob_invalid",
    "manager_succession_handoff_head_changed",
    "manager_succession_handoff_size",
    "manager_succession_handoff_unavailable",
    "manager_succession_identity_corrupt",
    "manager_succession_invalid_limit",
    "manager_succession_invalid_spend",
    "manager_succession_invocation_changed",
    "manager_succession_launch_invalid",
    "manager_succession_origin_required",
    "manager_succession_predecessor_changed",
    "manager_succession_predecessor_invocation_unproven",
    "manager_succession_predecessor_ledger_live",
    "manager_succession_predecessor_unsettled",
    "manager_succession_publication_changed",
    "manager_succession_publication_required",
    "manager_succession_resource_origin_missing",
    "manager_succession_resource_identity_changed",
    "manager_succession_resource_project_changed",
    "manager_succession_root_required",
    "manager_succession_rotation_disabled",
    "manager_succession_settlement_changed",
    "manager_succession_source_changed",
    "manager_succession_source_custody_changed",
    "manager_succession_token_changed",
    "manager_succession_transaction_required",
    "manager_succession_unavailable",
    "manager_succession_unresolved",
    "manager_succession_unsettled_predecessor",
    "manager_v2_scope_changed",
    "manager_v2_policy_changed",
    "manager_v2_capability_denied",
    "manager_v2_epic_out_of_scope",
    "manager_v2_container_out_of_scope",
    "manager_v2_lead_changed",
    "container_not_empty",
    "manager_v2_container_changed",
    "manager_v2_container_state_changed",
    "manager_v2_policy_paused",
    "manager_v2_manager_paused",
    "manager_v2_pause_malformed",
    "manager_v2_pending_operator_decision",
    "manager_v2_decision_hold",
    "manager_v2_no_ready_work",
    "manager_v2_human_or_recovery_owner",
    "manager_v2_program_evidence_unknown",
    "manager_v2_retry_disabled",
    "manager_v2_resource_limit",
    "manager_v2_concurrency_capacity",
    "manager_v2_provider_capacity",
    "manager_v2_spend_exhausted",
    "manager_v2_provider_usage_limit",
    "manager_v2_spend_unknown",
    "manager_v2_launch_not_granted",
    "manager_v2_source_changed",
    "manager_v2_source_worktree_dirty",
    "manager_v2_predecessor_unsettled",
    "manager_v2_candidate_unconfirmed",
    "manager_v2_candidate_cancelled",
    "manager_v2_lead_not_resumable",
    "manager_v2_resume_unavailable",
    "manager_v2_retry_lead_resumable",
    "manager_v2_retry_requires_terminal",
    "manager_v2_candidate_already_exists",
    "manager_v2_integrate_in_progress",
    "manager_v2_source_acceptance_required",
    "manager_v2_not_integrate_action",
    // K14 (#672) delegated operator calls.
    "manager_v2_operator_method_not_delegable",
    "manager_v2_operator_params_invalid",
    "manager_v2_operator_fence_required",
    "manager_v2_operator_result_too_large",
    "manager_v2_target_out_of_project",
    "manager_v2_retention_pinned",
    "manager_v2_retention_enabled_wake",
    "manager_v2_retention_live_review",
    "manager_v2_retention_sealed_source",
    "manager_v2_retention_recent_activity",
    "manager_v2_retention_live_worktree",
    "manager_v2_effect_unobservable",
    "manager_v2_action_not_uncertain",
    "manager_v2_action_changed",
    "manager_v2_action_out_of_scope",
    "manager_v2_action_unavailable",
    "manager_v2_lead_active",
];

pub(super) fn safe_action_error(error: &DaemonError) -> &'static str {
    let message = error.to_string();
    if message.contains("manager_notice_deferred") {
        return "manager_v2_human_or_recovery_owner";
    }
    if message.contains("source_worktree_dirty") {
        return "manager_v2_source_worktree_dirty";
    }
    for &code in MANAGER_ACTION_ERROR_CODES {
        if message.contains(code) {
            return code;
        }
    }
    "manager_v2_lifecycle_unconfirmed"
}

/// Resolve a Git ref to its commit OID using the hardened engine git helper.
async fn git_ref(repo: &std::path::Path, ref_name: &str) -> Option<String> {
    use crate::integration::{CommitIdentity, IntegrationConfig};
    let config = IntegrationConfig {
        allowed_targets: vec![ref_name.to_string()],
        identity: CommitIdentity {
            name: "rsi integration".into(),
            email: "integration@rsi.local".into(),
        },
        git_timeout: std::time::Duration::from_secs(30),
    };
    crate::integration::resolve_ref(&config, repo, ref_name).await
}

fn manager_launch_config(claim: &ManagerActionClaimV2, source: &Session) -> Result<LaunchConfig> {
    let choice = claim
        .operation
        .context
        .launch
        .as_ref()
        .ok_or_else(|| refused("manager_v2_launch_unavailable"))?;
    let (query, kind, parent, predecessor) = match claim.action() {
        ManagerActionV2::CreateSession {
            query,
            kind,
            parent_id,
            ..
        } => (query.clone(), *kind, *parent_id, None),
        ManagerActionV2::ReplaceLead { query, epic_id, .. } => (
            query.clone(),
            source.session_kind,
            *epic_id,
            Some(source.id),
        ),
        ManagerActionV2::RetryLead {
            message, epic_id, ..
        } => (
            message.clone(),
            source.session_kind,
            *epic_id,
            Some(source.id),
        ),
        _ => return Err(refused("manager_v2_not_launch_action")),
    };
    let sandbox = Some(rsi_common::types::SandboxSpec {
        kind: Some(rsi_common::types::SandboxKind::GitWorktree),
        branch: None,
    });
    Ok(LaunchConfig {
        query,
        title: None,
        agent_role: predecessor.and_then(|_| source.agent_role.clone()),
        epic_spawn_ordinal: predecessor.and(source.epic_spawn_ordinal),
        working_dir: Some(source.working_dir.clone()),
        provider: Some(choice.provider),
        model: Some(choice.model.clone()),
        configured_context_window: None,
        max_turns: None,
        system_prompt: None,
        resume_session_id: None,
        session_kind: Some(kind),
        project_id: Some(claim.operation.project_id),
        rsi_session_id: claim.operation.context.target_session_id,
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: predecessor,
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: source.workflow_id,
        workflow_id_override: None,
        max_retries: Some(0),
        group_id: None,
        parent_id: Some(parent),
        effort: choice.effort.clone(),
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        model_invocation_owner: None,
        model_invocation_dedup_key: Some(format!("manager.action:{}", claim.id())),
        model_invocation_request_fingerprint: Some(crate::model_control::hash_request_fingerprint(
            &[&claim.id().to_string()],
        )),
        // The reservation freezes the effective provider/model witness.
        // A later project-default edit must not rewrite an exact replay.
        skip_project_model_default: true,
        model_invocation_purpose:
            rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
        sandbox,
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: false,
        skip_context_pipeline: false,
        capability_class: None,
        tags: source.tags.clone(),
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
    })
}
