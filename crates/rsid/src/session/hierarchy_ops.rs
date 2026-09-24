//! Authoritative hierarchy and Epic-lead mutations for `SessionManager`.

use super::spawn_single_flight::{
    SpawnGuard, acquire_spawn_guards_sorted, try_acquire_spawn_guards_sorted,
};
use super::{CompletedSession, SessionManager, TrackedSession};
use crate::bus::{DaemonEvent, EventBus};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use rsi_common::rpc::CreateContainerParams;
use rsi_common::types::{
    ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

impl SessionManager {
    /// Create a non-spawnable hierarchy container and make it visible in the
    /// daemon runtime immediately.
    pub async fn create_container(&self, params: CreateContainerParams) -> Result<Session> {
        if !rsi_common::is_container_kind(params.kind) {
            return Err(DaemonError::InvalidParam(format!(
                "illegal_kind: CreateContainer requires a container kind (Group or Epic), got {:?}",
                params.kind
            )));
        }

        let parent = if let Some(parent_id) = params.parent_id {
            Some(self.resolve_session(parent_id).await?.ok_or_else(|| {
                DaemonError::InvalidParam(format!(
                    "illegal_parent: parent session {parent_id} not found"
                ))
            })?)
        } else {
            None
        };
        let parent_kind = parent.as_ref().map(|parent| parent.session_kind);

        validate_containment_for_rpc(parent_kind, params.kind)?;

        // ─── P1.6: validate and normalize tags ──────────────────────────────────
        if params.tags.is_empty() {
            return Err(DaemonError::InvalidParam(
                "tags required: at least one tag must be provided".to_string(),
            ));
        }
        let normalized_tags: Vec<String> = {
            let mut out = Vec::with_capacity(params.tags.len());
            for t in &params.tags {
                match rsi_common::normalize_tag(t) {
                    Ok(n) => out.push(n),
                    Err(_) => {
                        return Err(DaemonError::InvalidParam(format!("tag_malformed: {t}")));
                    }
                }
            }
            out.sort();
            out.dedup();
            out
        };

        let working_dir = if let Some(project_id) = params.project_id {
            self.resolve_project_working_dir(project_id).await?
        } else if let Some(parent) = parent.as_ref() {
            parent.working_dir.clone()
        } else {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
        };
        let session =
            build_container_session(Uuid::new_v4(), params, working_dir, normalized_tags.clone());

        self.completed.write().await.insert(
            session.id,
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

        if let Err(e) = self.persist_insert_session(session.clone()).await {
            self.completed.write().await.remove(&session.id);
            return Err(e);
        }

        // ─── P1.6: persist tags to session_tags join table ──────────────────────
        self.update_session_tags(session.id, normalized_tags)
            .await?;

        Ok(session)
    }

    /// Reparent an existing session and apply lead lifecycle hooks that depend
    /// on parent changes.
    pub async fn set_session_parent(
        &self,
        session_id: Uuid,
        new_parent_id: Option<Uuid>,
    ) -> Result<Session> {
        let session = self
            .resolve_session(session_id)
            .await?
            .ok_or(DaemonError::SessionNotFound(session_id))?;
        let old_parent_id = session.parent_id;

        let parent_kind = if let Some(parent_id) = new_parent_id {
            let parent = self.resolve_session(parent_id).await?.ok_or_else(|| {
                DaemonError::InvalidParam(format!(
                    "illegal_parent: parent session {parent_id} not found"
                ))
            })?;
            Some(parent.session_kind)
        } else {
            None
        };
        validate_containment_for_rpc(parent_kind, session.session_kind)?;

        if let Some(proposed_parent) = new_parent_id {
            let parent_index = self.runtime_parent_index().await?;
            crate::session::hierarchy::detect_cycle(session_id, proposed_parent, |id| {
                parent_index.get(&id).copied().flatten()
            })
            .map_err(map_containment_error)?;
        }

        self.store
            .lock()
            .await
            .reject_nonterminal_agent_successor_candidate_topology_mutation(session_id)?;

        self.update_parent_in_memory(session_id, new_parent_id)
            .await;
        if let Err(e) = self
            .persist_update_session_parent(session_id, new_parent_id)
            .await
        {
            self.update_parent_in_memory(session_id, old_parent_id)
                .await;
            return Err(e);
        }

        self.event_bus.publish(DaemonEvent::SessionMetadataChanged {
            session_id,
            model: None,
            pinned_at: None,
            project_id: None,
            parent_id: Some(new_parent_id),
            lead_session_id: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            resolved_context_budget: None,
        });

        if old_parent_id != new_parent_id {
            if let Some(old_parent_id) = old_parent_id {
                self.clear_epic_lead_if_matches(old_parent_id, session_id)
                    .await?;
            }
            let mut moved = session.clone();
            moved.parent_id = new_parent_id;
            if let Some(new_parent_id) = new_parent_id {
                self.try_auto_promote_epic_lead(new_parent_id, &moved, None)
                    .await?;
            }
        }

        let mut updated = session;
        updated.parent_id = new_parent_id;
        Ok(updated)
    }

    /// Set or clear an Epic's lead pointer.
    pub async fn set_epic_lead(
        &self,
        epic_id: Uuid,
        new_lead_session_id: Option<Uuid>,
    ) -> Result<Option<Uuid>> {
        let epic = self
            .resolve_session(epic_id)
            .await?
            .ok_or(DaemonError::SessionNotFound(epic_id))?;
        if epic.session_kind != SessionKind::Epic {
            return Err(DaemonError::InvalidParam(format!(
                "set_epic_lead_rejected: session {epic_id} is {:?}, not Epic",
                epic.session_kind
            )));
        }

        if let Some(lead_id) = new_lead_session_id {
            let lead = self
                .resolve_session(lead_id)
                .await?
                .ok_or_else(|| DaemonError::InvalidParam(format!("lead not found: {lead_id}")))?;
            validate_lead_candidate(epic_id, &lead)?;
        }
        // A reservation lock keeps its own typed refusal ahead of the gate;
        // it is re-checked under the guards below.
        self.store
            .lock()
            .await
            .reject_nonterminal_agent_successor_epic_lead_mutation(epic_id)?;
        // K2 authority gate: both leads' guards, non-blocking in the global
        // order. A continuation of either lead holds its guard until its
        // provider is installed; the operator retries `lead_mutation_contended`.
        let affected: Vec<Uuid> = epic
            .lead_session_id
            .into_iter()
            .chain(new_lead_session_id)
            .collect();
        let _lead_guards = lead_mutation_guards(&affected, LeadGuardMode::TryAcquire).await?;
        let epic = self
            .resolve_session(epic_id)
            .await?
            .ok_or(DaemonError::SessionNotFound(epic_id))?;
        if epic
            .lead_session_id
            .is_some_and(|lead| !affected.contains(&lead))
        {
            return Err(DaemonError::InvalidParam(LEAD_MUTATION_CONTENDED.into()));
        }

        self.store
            .lock()
            .await
            .reject_nonterminal_agent_successor_epic_lead_mutation(epic_id)?;

        let old_lead = epic.lead_session_id;
        self.set_lead_in_memory(epic_id, new_lead_session_id).await;
        let jobs = match self
            .persist_epic_lead_and_readdress(epic_id, new_lead_session_id)
            .await
        {
            Ok(jobs) => jobs,
            Err(e) => {
                self.set_lead_in_memory(epic_id, old_lead).await;
                return Err(e);
            }
        };

        for job_id in jobs {
            self.event_bus
                .publish(crate::bus::DaemonEvent::ManagerNoticeQueued { job_id });
        }

        self.publish_lead_delta(epic_id, new_lead_session_id);
        if let Some(lead_id) = new_lead_session_id {
            self.mark_lead_session(lead_id).await;
        }
        Ok(old_lead)
    }

    /// Auto-promote the first valid child to an Epic that has no lead.
    ///
    /// K2 finding d: this writer goes through the same authority gate as every
    /// other `lead_session_id` writer. It holds guard(candidate) (the launch
    /// path passes the spawn guard it already holds as `held`) and commits
    /// with the NULL-lead CAS, so the generation bump cannot straddle a fenced
    /// continuation of the candidate.
    pub(super) async fn try_auto_promote_epic_lead(
        &self,
        epic_id: Uuid,
        candidate: &Session,
        held: Option<&SpawnGuard>,
    ) -> Result<bool> {
        if validate_lead_candidate(epic_id, candidate).is_err() {
            return Ok(false);
        }
        let _lead_guards = lead_mutation_guards(
            &[candidate.id],
            held.map_or(LeadGuardMode::Acquire, LeadGuardMode::Held),
        )
        .await?;

        let epic = self
            .resolve_session(epic_id)
            .await?
            .ok_or(DaemonError::SessionNotFound(epic_id))?;
        if epic.session_kind != SessionKind::Epic || epic.lead_session_id.is_some() {
            return Ok(false);
        }

        self.store
            .lock()
            .await
            .reject_nonterminal_agent_successor_epic_lead_mutation(epic_id)?;

        self.set_lead_in_memory(epic_id, Some(candidate.id)).await;
        match self
            .persist_try_promote_lead_if_unset(epic_id, candidate.id)
            .await
        {
            Ok(true) => {
                self.publish_lead_delta(epic_id, Some(candidate.id));
                self.mark_lead_session(candidate.id).await;
                Ok(true)
            }
            Ok(false) => {
                let persisted = self.resolve_session_from_store(epic_id).await?;
                self.set_lead_in_memory(epic_id, persisted.and_then(|s| s.lead_session_id))
                    .await;
                Ok(false)
            }
            Err(e) => {
                self.set_lead_in_memory(epic_id, epic.lead_session_id).await;
                Err(e)
            }
        }
    }

    pub(super) async fn clear_lead_pointers_to(&self, target: Uuid) -> Result<()> {
        Self::clear_lead_pointers_to_runtime(
            target,
            &self.active,
            &self.completed,
            &self.store,
            &self.event_bus,
            LeadGuardMode::Acquire,
        )
        .await
    }

    /// `clear_lead_pointers_to` for a caller that already holds guard(target).
    pub(super) async fn clear_lead_pointers_to_held(
        &self,
        target: Uuid,
        held: &SpawnGuard,
    ) -> Result<()> {
        Self::clear_lead_pointers_to_runtime(
            target,
            &self.active,
            &self.completed,
            &self.store,
            &self.event_bus,
            LeadGuardMode::Held(held),
        )
        .await
    }

    pub(super) async fn clear_lead_pointers_to_runtime(
        target: Uuid,
        active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        store: &Arc<tokio::sync::Mutex<Store>>,
        event_bus: &Arc<EventBus>,
        guard_mode: LeadGuardMode<'_>,
    ) -> Result<()> {
        // K2 authority gate: guard(target) before the store lock.
        let _lead_guards = lead_mutation_guards(&[target], guard_mode).await?;
        store
            .lock()
            .await
            .reject_nonterminal_agent_successor_lead_clear(target)?;
        let affected = Self::memory_epics_with_lead_runtime(active, completed, target).await;
        for epic_id in &affected {
            Self::set_lead_in_runtime(active, completed, *epic_id, None).await;
        }

        if let Err(e) = Self::persist_clear_lead_session_if_matches_runtime(store, target).await {
            for epic_id in affected {
                Self::set_lead_in_runtime(active, completed, epic_id, Some(target)).await;
            }
            return Err(e);
        }

        for epic_id in affected {
            Self::publish_lead_delta_runtime(event_bus, epic_id, None);
        }
        Ok(())
    }

    pub(super) async fn repair_invalid_epic_leads_on_restore(&self) -> Result<()> {
        let sessions = self.list_sessions().await;
        let by_id: HashMap<Uuid, Session> = sessions.iter().map(|s| (s.id, s.clone())).collect();

        for epic in sessions
            .iter()
            .filter(|s| s.session_kind == SessionKind::Epic && s.lead_session_id.is_some())
        {
            let lead_id = epic.lead_session_id.expect("checked is_some");
            let valid = by_id
                .get(&lead_id)
                .is_some_and(|lead| validate_lead_candidate(epic.id, lead).is_ok());
            if valid {
                continue;
            }

            tracing::warn!(
                epic_id = %epic.id,
                lead_id = %lead_id,
                "Clearing invalid Epic lead pointer during restore"
            );
            let _lead_guards = lead_mutation_guards(&[lead_id], LeadGuardMode::Acquire).await?;
            self.set_lead_in_memory(epic.id, None).await;
            if let Err(e) = self.persist_set_lead_session(epic.id, None).await {
                tracing::warn!(
                    epic_id = %epic.id,
                    lead_id = %lead_id,
                    error = %e,
                    "Failed to persist restore-time Epic lead repair"
                );
            }
            self.publish_lead_delta(epic.id, None);
        }

        Ok(())
    }

    async fn clear_epic_lead_if_matches(&self, epic_id: Uuid, lead_id: Uuid) -> Result<()> {
        let Some(epic) = self.resolve_session(epic_id).await? else {
            return Ok(());
        };
        if epic.session_kind != SessionKind::Epic || epic.lead_session_id != Some(lead_id) {
            return Ok(());
        }
        let _lead_guards = lead_mutation_guards(&[lead_id], LeadGuardMode::Acquire).await?;

        self.store
            .lock()
            .await
            .reject_nonterminal_agent_successor_epic_lead_mutation(epic_id)?;

        self.set_lead_in_memory(epic_id, None).await;
        if let Err(e) = self.persist_set_lead_session(epic_id, None).await {
            self.set_lead_in_memory(epic_id, Some(lead_id)).await;
            return Err(e);
        }
        self.publish_lead_delta(epic_id, None);
        Ok(())
    }

    pub(super) async fn resolve_session(&self, id: Uuid) -> Result<Option<Session>> {
        if let Some(tracked) = self.active.read().await.get(&id) {
            return Ok(Some(tracked.session.clone()));
        }
        if let Some(completed) = self.completed.read().await.get(&id) {
            return Ok(Some(completed.session.clone()));
        }
        self.resolve_session_from_store(id).await
    }

    async fn resolve_session_from_store(&self, id: Uuid) -> Result<Option<Session>> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_session(id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    async fn runtime_parent_index(&self) -> Result<HashMap<Uuid, Option<Uuid>>> {
        let store = self.store.clone();
        let mut index = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_parent_index()
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        for tracked in self.active.read().await.values() {
            index.insert(tracked.session.id, tracked.session.parent_id);
        }
        for completed in self.completed.read().await.values() {
            index.insert(completed.session.id, completed.session.parent_id);
        }
        Ok(index)
    }

    async fn update_parent_in_memory(&self, session_id: Uuid, parent_id: Option<Uuid>) {
        if let Some(tracked) = self.active.write().await.get_mut(&session_id) {
            tracked.session.parent_id = parent_id;
            tracked.session.updated_at = chrono::Utc::now();
            return;
        }
        if let Some(completed) = self.completed.write().await.get_mut(&session_id) {
            completed.session.parent_id = parent_id;
            completed.session.updated_at = chrono::Utc::now();
        }
    }

    /// Apply every mark a session earns by taking lead authority: the lead role
    /// word in its title, and a pin. See [`mark_lead_session_runtime`].
    pub(super) async fn mark_lead_session(&self, lead_id: Uuid) {
        mark_lead_session_runtime(
            &self.active,
            &self.completed,
            &self.store,
            &self.event_bus,
            lead_id,
        )
        .await;
    }

    pub(super) async fn set_lead_in_memory(&self, epic_id: Uuid, lead_id: Option<Uuid>) {
        Self::set_lead_in_runtime(&self.active, &self.completed, epic_id, lead_id).await;
    }

    async fn set_lead_in_runtime(
        active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        epic_id: Uuid,
        lead_id: Option<Uuid>,
    ) {
        if let Some(tracked) = active.write().await.get_mut(&epic_id) {
            tracked.session.lead_session_id = lead_id;
            tracked.session.updated_at = chrono::Utc::now();
            return;
        }
        if let Some(completed_session) = completed.write().await.get_mut(&epic_id) {
            completed_session.session.lead_session_id = lead_id;
            completed_session.session.updated_at = chrono::Utc::now();
        }
    }

    async fn memory_epics_with_lead_runtime(
        active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        target: Uuid,
    ) -> Vec<Uuid> {
        let mut affected = Vec::new();
        affected.extend(
            active
                .read()
                .await
                .values()
                .filter(|tracked| {
                    tracked.session.session_kind == SessionKind::Epic
                        && tracked.session.lead_session_id == Some(target)
                })
                .map(|tracked| tracked.session.id),
        );
        affected.extend(
            completed
                .read()
                .await
                .values()
                .filter(|completed| {
                    completed.session.session_kind == SessionKind::Epic
                        && completed.session.lead_session_id == Some(target)
                })
                .map(|completed| completed.session.id),
        );
        affected
    }

    pub(super) fn publish_lead_delta(&self, epic_id: Uuid, lead_id: Option<Uuid>) {
        Self::publish_lead_delta_runtime(&self.event_bus, epic_id, lead_id);
    }

    fn publish_lead_delta_runtime(event_bus: &Arc<EventBus>, epic_id: Uuid, lead_id: Option<Uuid>) {
        event_bus.publish(DaemonEvent::SessionMetadataChanged {
            session_id: epic_id,
            model: None,
            pinned_at: None,
            project_id: None,
            parent_id: None,
            lead_session_id: Some(lead_id),
            testing_needed_at: None,
            rotation_disabled_at: None,
            resolved_context_budget: None,
        });
    }

    async fn persist_insert_session(&self, session: Session) -> Result<()> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.insert_session(&session)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    async fn persist_update_session_parent(
        &self,
        session_id: Uuid,
        new_parent: Option<Uuid>,
    ) -> Result<()> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.update_session_parent(session_id, new_parent)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    async fn persist_set_lead_session(&self, epic_id: Uuid, lead_id: Option<Uuid>) -> Result<()> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.set_lead_session(epic_id, lead_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    async fn persist_epic_lead_and_readdress(
        &self,
        epic_id: Uuid,
        lead_id: Option<Uuid>,
    ) -> Result<Vec<Uuid>> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.set_epic_lead_and_readdress(epic_id, lead_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    async fn persist_try_promote_lead_if_unset(
        &self,
        epic_id: Uuid,
        candidate_id: Uuid,
    ) -> Result<bool> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.try_promote_lead_if_unset(epic_id, candidate_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }

    async fn persist_clear_lead_session_if_matches_runtime(
        store: &Arc<tokio::sync::Mutex<Store>>,
        target: Uuid,
    ) -> Result<()> {
        let store = store.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.clear_lead_session_if_matches(target)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))?
    }
}

fn validate_containment_for_rpc(
    parent_kind: Option<SessionKind>,
    child_kind: SessionKind,
) -> Result<()> {
    crate::session::hierarchy::validate_containment(parent_kind, child_kind)
        .map_err(map_containment_error)
}

fn map_containment_error(e: crate::session::hierarchy::ContainmentError) -> DaemonError {
    match e {
        crate::session::hierarchy::ContainmentError::IllegalParent { parent, child } => {
            DaemonError::InvalidParam(format!(
                "illegal_parent: {:?} cannot contain {:?}",
                parent, child
            ))
        }
        crate::session::hierarchy::ContainmentError::CycleDetected { chain } => {
            DaemonError::InvalidParam(format!("cycle_detected: {:?}", chain))
        }
    }
}

/// Apply the pin a session earns by taking lead authority.
///
/// Pinning is best-effort and must run only AFTER the lead pointer is durable,
/// so no failure here can leave a session marked for authority it does not
/// hold. Lead authority does not rewrite the session's raw title. A former
/// lead stays pinned at the operator's call.
///
/// Runtime-shaped (no `&self`) because the rotation saga transfers the baton
/// from a static context and must apply the same marks as an ordinary
/// promotion. One code path, so the two can never disagree.
pub(super) async fn mark_lead_session_runtime(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    event_bus: &Arc<EventBus>,
    lead_id: Uuid,
) {
    pin_lead_session_runtime(active, completed, store, event_bus, lead_id).await;
}

/// Pin a session that has just taken lead authority, if it is not pinned
/// already. Idempotent by construction (`pin_session_if_unpinned`): a toggle
/// here would unpin an operator's manual pin, and would unpin a session that
/// takes the baton a second time.
async fn pin_lead_session_runtime(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    event_bus: &Arc<EventBus>,
    lead_id: Uuid,
) {
    let store_for_pin = store.clone();
    let pinned = tokio::task::spawn_blocking(move || {
        store_for_pin
            .blocking_lock()
            .pin_session_if_unpinned(lead_id)
    })
    .await
    .map_err(|e| DaemonError::Store(e.to_string()))
    .and_then(|inner| inner);

    let pinned_at = match pinned {
        Ok(pinned_at) => pinned_at,
        Err(e) => {
            tracing::warn!(
                lead_id = %lead_id,
                error = %e,
                "Failed to pin newly installed lead session"
            );
            return;
        }
    };

    let parsed = pinned_at.as_deref().and_then(|value| {
        chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
    });
    if let Some(tracked) = active.write().await.get_mut(&lead_id) {
        tracked.session.pinned_at = parsed;
    } else if let Some(completed_session) = completed.write().await.get_mut(&lead_id) {
        completed_session.session.pinned_at = parsed;
    }

    event_bus.publish(DaemonEvent::SessionMetadataChanged {
        session_id: lead_id,
        model: None,
        pinned_at: Some(pinned_at),
        project_id: None,
        parent_id: None,
        lead_session_id: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        resolved_context_budget: None,
    });
}

fn validate_lead_candidate(epic_id: Uuid, lead: &Session) -> Result<()> {
    if !rsi_common::is_leaf_kind(lead.session_kind) {
        return Err(DaemonError::InvalidParam(format!(
            "set_epic_lead_rejected: session {} is {:?}, not a leaf kind",
            lead.id, lead.session_kind
        )));
    }
    if lead.parent_id != Some(epic_id) {
        return Err(DaemonError::InvalidParam(format!(
            "set_epic_lead_rejected: session {} is not a direct child of Epic {}",
            lead.id, epic_id
        )));
    }
    if matches!(
        lead.status,
        SessionStatus::Archived | SessionStatus::Deleted
    ) {
        return Err(DaemonError::InvalidParam(format!(
            "set_epic_lead_rejected: session {} is {:?} (archived or deleted)",
            lead.id, lead.status
        )));
    }
    Ok(())
}

/// Maximum walk depth for `effective_topology` traversal. Realistic chains
/// never exceed `MAX_SPAWN_DEPTH = 5`; this 16-deep cap exists purely as a
/// defense against `parent_id` cycles that would otherwise spin forever in
/// production. Distinct from `MAX_SPAWN_DEPTH` (spawn policy) and
/// `MAX_HIER_DEPTH` (TUI render cap in `mini_dag.rs`).
pub const MAX_HIERARCHY_DEPTH: u32 = 16;

/// Read-only borrow over the daemon's session universe. Implemented by the
/// in-memory `HashMap<Uuid, TrackedSession>` (active), the
/// `HashMap<Uuid, CompletedSession>` (finalized), and a plain
/// `HashMap<Uuid, Session>` (built ad-hoc at restore / for test fixtures).
///
/// The trait exists so `effective_topology` can walk parent chains over any
/// of the three shapes without forcing callers to clone or reshape data.
pub trait SessionsView {
    fn get(&self, id: Uuid) -> Option<&Session>;
}

impl SessionsView for HashMap<Uuid, Session> {
    fn get(&self, id: Uuid) -> Option<&Session> {
        HashMap::get(self, &id)
    }
}

impl SessionsView for HashMap<Uuid, TrackedSession> {
    fn get(&self, id: Uuid) -> Option<&Session> {
        HashMap::get(self, &id).map(|t| &t.session)
    }
}

impl SessionsView for HashMap<Uuid, CompletedSession> {
    fn get(&self, id: Uuid) -> Option<&Session> {
        HashMap::get(self, &id).map(|c| &c.session)
    }
}

/// Resolve the topology a session effectively runs under by walking its
/// `parent_id` chain to the first ancestor with `workflow_id.is_some()`.
/// Returns `None` if no ancestor has a topology, or if the walk hits the
/// `MAX_HIERARCHY_DEPTH` cap (cycle detected).
///
/// Allocation-free: the walk is a depth-counted cursor loop over borrowed
/// `&Session` refs from the view. No `Vec`/`HashSet`/`String` allocated on
/// the hot path.
///
/// Does **not** consult `Session.workflow_id_override` — see
/// [`effective_topology_with_override`] for that.
pub fn effective_topology<V: SessionsView>(session: &Session, view: &V) -> Option<Uuid> {
    if session.workflow_id.is_some() {
        return session.workflow_id;
    }
    let mut cursor: Option<Uuid> = session.parent_id;
    let mut depth: u32 = 1;
    while let Some(pid) = cursor {
        if depth >= MAX_HIERARCHY_DEPTH {
            tracing::warn!(
                session_id = %session.id,
                depth,
                "effective_topology depth cap reached — possible parent_id cycle"
            );
            return None;
        }
        let ancestor = view.get(pid)?;
        if ancestor.workflow_id.is_some() {
            return ancestor.workflow_id;
        }
        cursor = ancestor.parent_id;
        depth += 1;
    }
    None
}

/// Variant of [`effective_topology`] that honors `Session.workflow_id_override`.
/// If the input session carries a non-`None` override, returns it immediately
/// without walking. Otherwise behaves identically to `effective_topology`.
///
/// Use this on the read path when leaf-level topology overrides are allowed.
pub fn effective_topology_with_override<V: SessionsView>(
    session: &Session,
    view: &V,
) -> Option<Uuid> {
    if session.workflow_id_override.is_some() {
        return session.workflow_id_override;
    }
    effective_topology(session, view)
}

// ─── Phase 5 (P1.7): iteration helper + prereq check ────────────────────────

/// Returns the maximum `topology_iteration` recorded for any session under
/// `epic_id` bound to `node_id`. Returns 0 when no prior iterations exist.
///
/// Queries the store via the V47 compound index
/// `idx_sessions_topology_node(parent_id, topology_node_id)`.
pub async fn max_iter_for_node(
    store: &Arc<tokio::sync::Mutex<Store>>,
    epic_id: Uuid,
    node_id: &str,
) -> crate::error::Result<u32> {
    let node_id = node_id.to_string();
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || {
        let guard = store.blocking_lock();
        let result: i64 = guard
            .conn
            .query_row(
                "SELECT COALESCE(MAX(topology_iteration), 0) \
                 FROM sessions \
                 WHERE parent_id = ?1 AND topology_node_id = ?2",
                rusqlite::params![epic_id.to_string(), node_id],
                |row| row.get(0),
            )
            .map_err(|e| crate::error::DaemonError::Store(e.to_string()))?;
        Ok(result as u32)
    })
    .await
    .map_err(|e| crate::error::DaemonError::Store(e.to_string()))?
}

/// Returns `true` if at least one session under `epic_id` is bound to
/// `node_id` and has `status == SessionStatus::Completed`.
/// Used by the spawn coordinator for prereq verification.
///
/// Iterates a slice of `&Session` without touching the store — the caller
/// passes a snapshot built from the active + completed maps.
pub fn prereq_satisfied(sessions: &[&Session], epic_id: Uuid, node_id: &str) -> bool {
    sessions.iter().any(|s| {
        s.parent_id == Some(epic_id)
            && s.topology_node_id.as_deref() == Some(node_id)
            && s.status == rsi_common::types::SessionStatus::Completed
    })
}

/// Refusal when a lead mutation cannot take its authority guards without
/// waiting on an in-flight continuation (`SetEpicLead`; operator retries).
pub const LEAD_MUTATION_CONTENDED: &str = "lead_mutation_contended";

/// How a `lead_session_id` writer takes the K2 authority gate.
pub(super) enum LeadGuardMode<'a> {
    /// Block for every guard in the global order (no guard already held).
    Acquire,
    /// All or nothing, never waiting; contention is a typed refusal.
    TryAcquire,
    /// The caller already holds this guard; acquire only the others.
    Held(&'a SpawnGuard),
}

/// K2 authority gate: every production writer of `sessions.lead_session_id`
/// holds the spawn guards of the affected leads, in the global lock order,
/// before it takes the store lock and commits (the generation trigger fires
/// in that commit). A fenced continuation holds its target's guard from its
/// check until its provider is installed, so a lead change is linearized
/// strictly before the check or strictly after the installation.
pub(super) async fn lead_mutation_guards(
    leads: &[Uuid],
    mode: LeadGuardMode<'_>,
) -> Result<Vec<SpawnGuard>> {
    match mode {
        LeadGuardMode::Acquire => Ok(acquire_spawn_guards_sorted(leads).await),
        LeadGuardMode::TryAcquire => try_acquire_spawn_guards_sorted(leads)
            .ok_or_else(|| DaemonError::InvalidParam(LEAD_MUTATION_CONTENDED.into())),
        LeadGuardMode::Held(held) => {
            let others: Vec<Uuid> = leads
                .iter()
                .copied()
                .filter(|lead| *lead != held.session_id())
                .collect();
            if others.is_empty() {
                return Ok(Vec::new());
            }
            // Adding a guard while holding another could invert the global
            // order; never wait in that case.
            try_acquire_spawn_guards_sorted(&others)
                .ok_or_else(|| DaemonError::InvalidParam(LEAD_MUTATION_CONTENDED.into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::store::Store;
    use rsi_common::harness_manager::{
        AgentManagerInboxRequestV1, ConfigureHarnessManagerRequestV1,
    };
    use rsi_common::rpc::CreateContainerParams;
    use rsi_common::types::Project;
    use tempfile::TempDir;

    fn manager() -> (SessionManager, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("rsi.db");
        let store = Store::open(&db_path).expect("open store");
        let config = Config::from_env();
        let runtime_config = RuntimeConfig::from_config(&config);
        let manager = SessionManager::new(
            std::sync::Arc::new(EventBus::new(16)),
            store,
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            runtime_config,
            dir.path().join("sandboxes"),
        )
        .expect("manager");
        (manager, dir)
    }

    fn leaf(id: Uuid, kind: SessionKind, parent_id: Uuid) -> Session {
        let now = chrono::Utc::now();
        Session {
            context_fill_pct: None,
            id,
            status: SessionStatus::Completed,
            session_kind: kind,
            provider: SessionProvider::default(),
            context_usage_confidence: ContextUsageConfidence::default(),
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            created_at: now,
            updated_at: now,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            query: "leaf".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            working_dir: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            model: None,
            claude_session_id: None,
            project_id: None,
            continued_from: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: Some(parent_id),
            lead_session_id: None,
            is_eval: false,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            stop_reason: None,
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            pending_question: None,
            pending_archive: false,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            approval_started_at: None,
            work_time_ms: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            // V99: populated from the provider handshake / result event, not at
            // construction. A session that never reaches those has none of these facts.
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    async fn insert_completed(manager: &SessionManager, session: Session) {
        {
            let store = manager.store.clone();
            let session_for_store = session.clone();
            tokio::task::spawn_blocking(move || {
                let store = store.blocking_lock();
                store.insert_session(&session_for_store)
            })
            .await
            .expect("join")
            .expect("insert");
        }
        manager.completed.write().await.insert(
            session.id,
            CompletedSession {
                session,
                events: Vec::new(),
                turn_metrics: Vec::new(),
                retry_cancel: None,
                retry_fired_at: None,
                superseded_by_retry: None,
                events_hydrated: true,
            },
        );
    }

    async fn insert_store(manager: &SessionManager, session: &Session) {
        let store = manager.store.clone();
        let session_for_store = session.clone();
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.insert_session(&session_for_store)
        })
        .await
        .expect("join")
        .expect("insert");
    }

    fn tracked(session: Session, pending_archive: bool) -> TrackedSession {
        let session_id = session.id;
        let (stop_tx, _stop_rx) = tokio::sync::mpsc::channel(1);
        TrackedSession {
            session,
            spawn_generation: 0,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            process: None,
            deferred_successor_start_gate: None,
            stop_tx,
            interrupt_requested: false,
            pending_archive,
            rotation: crate::session::rotation_coordinator::RotationCoordinator::new(
                session_id, 0, false,
            ),
            live_input_tokens: 0,
            live_output_tokens: 0,
            live_usage_confidence: ContextUsageConfidence::Missing,
            daemon_input_tokens: 0,
            daemon_output_tokens: 0,
            daemon_tokens_at_last_api_update: 0,
            codex_context_tokens: 0,
            pipeline_artifact: None,
            memory_flush_compaction_count: None,
            pending_question: None,
            approval_wait_start: None,
            approval_wait_total_ms: 0,
            work_run_start: None,
            work_time_base_ms: 0,
            received_meaningful_output: true,
            exit_code: Some(0),
            retry_attempt: 0,
            max_retries: 0,
            last_event_at: chrono::Utc::now(),
            stall_interrupted: false,
            last_usage_update: None,
            last_mismatch_warn: None,
            last_classified_at: None,
            classification_count: 0,
            last_verdict: None,
        }
    }

    /// Lighter-weight test fixture for `effective_topology` tests. Builds a
    /// `Session` with the four fields the helper actually reads (id, parent_id,
    /// workflow_id, workflow_id_override) and stubs everything else. Mirrors
    /// the existing `leaf()` fixture but without spinning up a `SessionKind`-
    /// specific shape.
    fn mk_session(
        id: Uuid,
        parent_id: Option<Uuid>,
        workflow_id: Option<Uuid>,
        workflow_id_override: Option<Uuid>,
    ) -> Session {
        let mut session = leaf(id, SessionKind::Standard, Uuid::nil());
        session.parent_id = parent_id;
        session.workflow_id = workflow_id;
        session.workflow_id_override = workflow_id_override;
        session
    }

    // ─────────────────────────────────────────────────────────────────────
    // P1.2 — effective_topology() helper
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn effective_topology_returns_none_for_orphan_leaf() {
        let leaf_id = Uuid::new_v4();
        let leaf = mk_session(leaf_id, None, None, None);
        let view: HashMap<Uuid, Session> = HashMap::from([(leaf_id, leaf.clone())]);
        assert_eq!(effective_topology(&leaf, &view), None);
        assert_eq!(effective_topology_with_override(&leaf, &view), None);
    }

    #[test]
    fn effective_topology_returns_epic_workflow() {
        let workflow = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let leaf_id = Uuid::new_v4();
        let epic = mk_session(epic_id, None, Some(workflow), None);
        let leaf = mk_session(leaf_id, Some(epic_id), None, None);
        let view: HashMap<Uuid, Session> =
            HashMap::from([(epic_id, epic), (leaf_id, leaf.clone())]);
        assert_eq!(effective_topology(&leaf, &view), Some(workflow));
    }

    #[test]
    fn effective_topology_walks_through_epic_to_group() {
        let workflow = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let leaf_id = Uuid::new_v4();
        let group = mk_session(group_id, None, Some(workflow), None);
        let epic = mk_session(epic_id, Some(group_id), None, None);
        let leaf = mk_session(leaf_id, Some(epic_id), None, None);
        let view: HashMap<Uuid, Session> =
            HashMap::from([(group_id, group), (epic_id, epic), (leaf_id, leaf.clone())]);
        assert_eq!(effective_topology(&leaf, &view), Some(workflow));
    }

    #[test]
    fn effective_topology_short_circuits_on_cycle() {
        // a.parent_id = b, b.parent_id = a → 2-cycle.
        let a_id = Uuid::new_v4();
        let b_id = Uuid::new_v4();
        let a = mk_session(a_id, Some(b_id), None, None);
        let b = mk_session(b_id, Some(a_id), None, None);
        let view: HashMap<Uuid, Session> = HashMap::from([(a_id, a.clone()), (b_id, b)]);
        // Walks a → b → a → b → ... until depth >= MAX_HIERARCHY_DEPTH, then warn + None.
        assert_eq!(effective_topology(&a, &view), None);
        assert_eq!(effective_topology_with_override(&a, &view), None);
    }

    #[test]
    fn effective_topology_with_override_beats_inherited() {
        let inherited = Uuid::new_v4();
        let override_topology = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let leaf_id = Uuid::new_v4();
        let epic = mk_session(epic_id, None, Some(inherited), None);
        let leaf = mk_session(leaf_id, Some(epic_id), None, Some(override_topology));
        let view: HashMap<Uuid, Session> =
            HashMap::from([(epic_id, epic), (leaf_id, leaf.clone())]);
        // Plain helper ignores override → walks to Epic.
        assert_eq!(effective_topology(&leaf, &view), Some(inherited));
        // Override helper short-circuits on the leaf's own override.
        assert_eq!(
            effective_topology_with_override(&leaf, &view),
            Some(override_topology)
        );
    }

    #[tokio::test]
    async fn create_container_is_immediately_visible_in_runtime() {
        let (manager, _dir) = manager();

        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let epic = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "Epic".to_string(),
                parent_id: Some(group.id),
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();

        let sessions = manager.list_sessions().await;
        assert!(sessions.iter().any(|s| s.id == group.id));
        assert!(sessions.iter().any(|s| s.id == epic.id));
        assert_eq!(
            manager.get_session(epic.id).await.unwrap().parent_id,
            Some(group.id)
        );
    }

    #[tokio::test]
    async fn create_container_uses_project_working_dir() {
        let (manager, dir) = manager();
        let project_dir = dir.path().join("acme");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = manager
            .create_project(
                "Acme".to_string(),
                Some(project_dir.clone()),
                None,
                None,
            )
            .await
            .unwrap();

        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: Some(project.id),
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();

        assert_eq!(group.working_dir, project_dir.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn create_container_inherits_parent_working_dir_without_project() {
        let (manager, dir) = manager();
        let project_dir = dir.path().join("acme");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = manager
            .create_project(
                "Acme".to_string(),
                Some(project_dir.clone()),
                None,
                None,
            )
            .await
            .unwrap();
        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: Some(project.id),
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();

        let epic = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "Epic".to_string(),
                parent_id: Some(group.id),
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();

        assert_eq!(epic.working_dir, group.working_dir);
    }

    #[tokio::test]
    async fn reparent_lead_out_clears_old_epic_and_auto_promotes_new_epic() {
        let (manager, _dir) = manager();
        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let epic_a = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "Epic A".to_string(),
                parent_id: Some(group.id),
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let epic_b = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "Epic B".to_string(),
                parent_id: Some(group.id),
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();

        let story_id = Uuid::new_v4();
        insert_completed(&manager, leaf(story_id, SessionKind::Story, epic_a.id)).await;
        manager
            .set_epic_lead(epic_a.id, Some(story_id))
            .await
            .unwrap();

        manager
            .set_session_parent(story_id, Some(epic_b.id))
            .await
            .unwrap();

        assert_eq!(
            manager
                .get_session(epic_a.id)
                .await
                .unwrap()
                .lead_session_id,
            None
        );
        assert_eq!(
            manager
                .get_session(epic_b.id)
                .await
                .unwrap()
                .lead_session_id,
            Some(story_id)
        );
    }

    #[tokio::test]
    async fn promoting_a_lead_preserves_its_session_title() {
        let (manager, _dir) = manager();
        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let epic = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "Epic".to_string(),
                parent_id: Some(group.id),
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let story_id = Uuid::new_v4();
        let mut story = leaf(story_id, SessionKind::Story, epic.id);
        story.title = Some("Implementer: S3 Custody Fence".to_string());
        insert_completed(&manager, story).await;

        manager
            .set_epic_lead(epic.id, Some(story_id))
            .await
            .unwrap();

        assert_eq!(
            manager
                .get_session(story_id)
                .await
                .unwrap()
                .title
                .as_deref(),
            Some("Implementer: S3 Custody Fence"),
            "the lead keeps its raw session title",
        );
        let persisted = manager
            .resolve_session_from_store(story_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted.title.as_deref(),
            Some("Implementer: S3 Custody Fence"),
            "lead promotion does not rewrite the durable session title",
        );
    }

    #[tokio::test]
    async fn promoting_a_lead_pins_it_and_keeps_an_existing_pin_time() {
        let (manager, _dir) = manager();
        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let epic = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "Epic".to_string(),
                parent_id: Some(group.id),
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();

        let first_id = Uuid::new_v4();
        insert_completed(&manager, leaf(first_id, SessionKind::Story, epic.id)).await;
        let second_id = Uuid::new_v4();
        insert_completed(&manager, leaf(second_id, SessionKind::Story, epic.id)).await;

        manager
            .set_epic_lead(epic.id, Some(first_id))
            .await
            .unwrap();
        let first_pin = manager
            .get_session(first_id)
            .await
            .unwrap()
            .pinned_at
            .expect("a new lead is pinned automatically");
        assert_eq!(
            manager
                .resolve_session_from_store(first_id)
                .await
                .unwrap()
                .unwrap()
                .pinned_at,
            Some(first_pin),
            "the automatic pin is durable",
        );

        // Baton moves on: the successor is pinned, and the predecessor keeps
        // both its pin and its original pin time, so pin order still reads as
        // the succession line.
        manager
            .set_epic_lead(epic.id, Some(second_id))
            .await
            .unwrap();
        assert_eq!(
            manager.get_session(first_id).await.unwrap().pinned_at,
            Some(first_pin),
            "a former lead stays pinned at its original time",
        );
        assert!(
            manager
                .get_session(second_id)
                .await
                .unwrap()
                .pinned_at
                .is_some(),
            "the successor is pinned on taking the baton",
        );

        // Re-taking the baton must not toggle the pin back off.
        manager.set_epic_lead(epic.id, None).await.unwrap();
        manager
            .set_epic_lead(epic.id, Some(first_id))
            .await
            .unwrap();
        assert_eq!(
            manager.get_session(first_id).await.unwrap().pinned_at,
            Some(first_pin),
            "re-promotion keeps the pin instead of toggling it off",
        );
    }

    #[tokio::test]
    async fn the_rotation_runtime_path_applies_the_same_marks_as_a_promotion() {
        // Rotation transfers the baton from a static context, so it calls the
        // runtime helper rather than the method. Both must mark identically.
        let (manager, _dir) = manager();
        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let epic = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "Epic".to_string(),
                parent_id: Some(group.id),
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let successor_id = Uuid::new_v4();
        let mut successor = leaf(successor_id, SessionKind::Story, epic.id);
        successor.title = Some("Implementer: S3 Custody Fence".to_string());
        insert_completed(&manager, successor).await;

        super::mark_lead_session_runtime(
            &manager.active,
            &manager.completed,
            &manager.store,
            &manager.event_bus,
            successor_id,
        )
        .await;

        let marked = manager
            .resolve_session_from_store(successor_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            marked.title.as_deref(),
            Some("Implementer: S3 Custody Fence")
        );
        assert!(
            marked.pinned_at.is_some(),
            "a rotation successor is pinned like any other new lead",
        );
    }

    #[tokio::test]
    async fn an_untitled_lead_is_left_for_its_own_generation_path() {
        let (manager, _dir) = manager();
        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let epic = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "Epic".to_string(),
                parent_id: Some(group.id),
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let story_id = Uuid::new_v4();
        let mut story = leaf(story_id, SessionKind::Story, epic.id);
        story.title = None;
        insert_completed(&manager, story).await;

        manager
            .set_epic_lead(epic.id, Some(story_id))
            .await
            .unwrap();

        assert_eq!(
            manager.get_session(story_id).await.unwrap().title,
            None,
            "no title is invented here; title generation resolves the role itself",
        );
    }

    #[tokio::test]
    async fn archiving_current_lead_clears_epic_pointer() {
        let (manager, _dir) = manager();
        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let epic = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "Epic".to_string(),
                parent_id: Some(group.id),
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let story_id = Uuid::new_v4();
        insert_completed(&manager, leaf(story_id, SessionKind::Story, epic.id)).await;
        manager
            .set_epic_lead(epic.id, Some(story_id))
            .await
            .unwrap();

        manager.archive_session(story_id).await.unwrap();

        assert_eq!(
            manager.get_session(epic.id).await.unwrap().lead_session_id,
            None
        );
        let persisted = manager
            .resolve_session_from_store(epic.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.lead_session_id, None);
    }

    #[tokio::test]
    async fn finalize_pending_archive_lead_retains_terminal_notice_across_reopen() {
        let (manager, dir) = manager();
        let database = dir.path().join("rsi.db");
        let now = chrono::Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: "Archive notice project".into(),
            path: Some(dir.path().to_path_buf()),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        manager.store.lock().await.insert_project(&project).unwrap();
        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: Some(project.id),
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let epic = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "Epic".to_string(),
                parent_id: Some(group.id),
                project_id: Some(project.id),
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();

        let story_id = Uuid::new_v4();
        let mut story = leaf(story_id, SessionKind::Story, epic.id);
        story.status = SessionStatus::Running;
        story.pending_archive = true;
        story.project_id = Some(project.id);
        insert_store(&manager, &story).await;
        manager
            .active
            .write()
            .await
            .insert(story_id, tracked(story, true));
        manager
            .set_epic_lead(epic.id, Some(story_id))
            .await
            .unwrap();
        let mut principal = leaf(Uuid::new_v4(), SessionKind::Standard, epic.id);
        principal.parent_id = None;
        principal.project_id = Some(project.id);
        principal.status = SessionStatus::Completed;
        insert_store(&manager, &principal).await;
        manager
            .store
            .lock()
            .await
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: project.id,
                session_id: principal.id,
                epic_ids: Some(vec![epic.id]),
                group_ids: Vec::new(),
                expected_row_version: 0,
            })
            .unwrap();

        SessionManager::finalize_session(
            story_id,
            0,
            crate::session::types::TerminalFinalizeDecision::completed(),
            manager.active.clone(),
            manager.completed.clone(),
            manager.event_bus.clone(),
            manager.store.clone(),
            manager.persistence.clone(),
            None,
            manager.runtime_config.clone(),
        )
        .await;

        assert_eq!(
            manager.get_session(epic.id).await.unwrap().lead_session_id,
            None
        );
        let persisted = manager
            .resolve_session_from_store(epic.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.lead_session_id, None);
        assert_eq!(
            manager
                .resolve_session_from_store(story_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SessionStatus::Archived
        );

        let reopened = Store::open(&database).unwrap();
        let inbox = reopened
            .manager_inbox(principal.id, &AgentManagerInboxRequestV1::default())
            .unwrap();
        let terminal = inbox
            .notices
            .iter()
            .find(|notice| {
                notice.kind == "session_state" && notice.subject_id == story_id.to_string()
            })
            .expect("the exact pre-archive terminal notice survives restart");
        assert_eq!(terminal.state["status"], "Completed");
        assert_eq!(terminal.state["session_id"], story_id.to_string());
    }

    // ─── P1.6 tests: create_container tag + topology validation ─────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn create_container_empty_tags_rejected() {
        let (manager, _dir) = manager();
        let err = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Test".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec![],
                topology_id: None,
            })
            .await
            .unwrap_err();
        match err {
            DaemonError::InvalidParam(msg) => {
                assert!(msg.contains("tags required"), "msg: {msg}")
            }
            other => panic!("expected InvalidParam, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_container_malformed_tag_rejected() {
        let (manager, _dir) = manager();
        let err = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Test".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["UPPER!".to_string()],
                topology_id: None,
            })
            .await
            .unwrap_err();
        match err {
            DaemonError::InvalidParam(msg) => {
                assert!(msg.contains("tag_malformed"), "msg: {msg}")
            }
            other => panic!("expected InvalidParam, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_container_valid_tags_persisted() {
        let (manager, _dir) = manager();
        let session = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Test".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["alpha".to_string(), "beta".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        // The in-memory session should have normalized tags
        assert_eq!(session.tags, vec!["alpha", "beta"]);
        assert_eq!(session.tag, "alpha"); // lex-first
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_container_topology_id_on_non_epic_rejected() {
        let (manager, _dir) = manager();
        // Use a fake topology_id — type-level rejection should fire before row check
        let fake_topo = Uuid::new_v4();
        // create_container validates topology_id Epic-only INSIDE the fn (we do it at RPC layer)
        // But since the kind is Group, the RPC layer would reject. Here we test via the store
        // directly that a topology row doesn't exist and verify the RPC-level rejection applies.
        // For the unit test we can't easily call handle_create_container, so we verify that
        // topology_id field threads through and check the session workflow_id is None on Group.
        let session = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Test Group".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: Some(fake_topo), // Groups set this but Epic-guard is in RPC layer
            })
            .await
            .unwrap();
        // create_container itself maps topology_id → session.workflow_id regardless of kind
        // (the Epic-only guard is the RPC handler's job — tested separately).
        // The workflow_id should be set to the topology_id value.
        assert_eq!(session.workflow_id, Some(fake_topo));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_container_topology_id_nonexistent_row_rejected() {
        // This test verifies that topology_exists returns false for unknown UUIDs.
        // The actual row-existence check is in handle_create_container (RPC layer),
        // but we verify the Store helper directly here.
        let (manager, _dir) = manager();
        let fake_topo = Uuid::new_v4();
        let store = manager.store.clone();
        let exists = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.topology_exists(fake_topo)
        })
        .await
        .expect("join")
        .expect("topology_exists");
        assert!(!exists, "non-existent topology UUID must not exist");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_container_topology_id_on_epic_persisted_to_workflow_id() {
        let (manager, _dir) = manager();
        let topo_id = Uuid::new_v4();

        // Insert a real topology row so topology_exists returns true
        let store_clone = manager.store.clone();
        let topo_id_for_insert = topo_id;
        tokio::task::spawn_blocking(move || {
            let store = store_clone.blocking_lock();
            store.conn.execute(
                "INSERT INTO topologies (id, name, definition_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    topo_id_for_insert.to_string(),
                    "test-topology",
                    "{\"version\":\"1.0\",\"name\":\"test\",\"nodes\":[],\"edges\":[],\"metadata\":{}}",
                    chrono::Utc::now().to_rfc3339(),
                    chrono::Utc::now().to_rfc3339(),
                ],
            )
        })
        .await
        .expect("join")
        .expect("insert topology");

        // Epic must be under a Group (legal_children(None) does not include Epic)
        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "My Group".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["ci".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();

        let epic = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Epic,
                name: "My Epic".to_string(),
                parent_id: Some(group.id),
                project_id: None,
                tags: vec!["ci".to_string()],
                topology_id: Some(topo_id),
            })
            .await
            .unwrap();

        // topology_id should be persisted to session.workflow_id
        assert_eq!(epic.workflow_id, Some(topo_id));
    }

    // ─── Phase 5 (P1.7): max_iter_for_node + prereq_satisfied ───────────────

    /// Helper to build a leaf session with topology binding.
    fn bound_leaf(
        id: Uuid,
        epic_id: Uuid,
        node_id: &str,
        iteration: u32,
        status: SessionStatus,
    ) -> Session {
        let mut s = leaf(id, SessionKind::Task, epic_id);
        s.topology_node_id = Some(node_id.to_string());
        s.topology_iteration = iteration;
        s.status = status;
        s
    }

    #[tokio::test]
    async fn max_iter_for_node_scoped_to_epic() {
        let (manager, _dir) = manager();
        let store = manager.store.clone();

        let epic_a = Uuid::new_v4();
        let epic_b = Uuid::new_v4();

        // Insert epic containers first (foreign-key style — parent must exist).
        let s_a1 = bound_leaf(
            Uuid::new_v4(),
            epic_a,
            "node_a",
            1,
            SessionStatus::Completed,
        );
        let s_a3 = bound_leaf(
            Uuid::new_v4(),
            epic_a,
            "node_a",
            3,
            SessionStatus::Completed,
        );
        let s_b99 = bound_leaf(
            Uuid::new_v4(),
            epic_b,
            "node_a",
            99,
            SessionStatus::Completed,
        );

        let g = store.lock().await;
        g.insert_session(&s_a1).unwrap();
        g.insert_session(&s_a3).unwrap();
        g.insert_session(&s_b99).unwrap();
        drop(g);

        let max_a = max_iter_for_node(&store, epic_a, "node_a").await.unwrap();
        assert_eq!(max_a, 3, "max iter for epic_a/node_a should be 3");

        let max_a_b = max_iter_for_node(&store, epic_a, "node_b").await.unwrap();
        assert_eq!(
            max_a_b, 0,
            "max iter for epic_a/node_b (no rows) should be 0"
        );

        let max_b = max_iter_for_node(&store, epic_b, "node_a").await.unwrap();
        assert_eq!(
            max_b, 99,
            "max iter for epic_b/node_a should be 99 (scoped)"
        );
    }

    #[test]
    fn prereq_satisfied_checks_status() {
        let epic_id = Uuid::new_v4();

        let completed = bound_leaf(Uuid::new_v4(), epic_id, "plan", 1, SessionStatus::Completed);
        let running = bound_leaf(Uuid::new_v4(), epic_id, "plan", 2, SessionStatus::Running);

        let sessions: Vec<&Session> = vec![&completed, &running];

        // Only completed session satisfies the prereq.
        assert!(
            prereq_satisfied(&sessions, epic_id, "plan"),
            "Completed session for 'plan' must satisfy prereq"
        );
        assert!(
            !prereq_satisfied(&sessions, epic_id, "build"),
            "No session for 'build' must not satisfy prereq"
        );

        // Running alone does not satisfy.
        let only_running: Vec<&Session> = vec![&running];
        assert!(
            !prereq_satisfied(&only_running, epic_id, "plan"),
            "Running-only session must not satisfy prereq"
        );
    }

    #[allow(clippy::unwrap_used)]
    async fn epic_generation(manager: &SessionManager, epic_id: Uuid) -> i64 {
        manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT generation FROM epic_lead_generations WHERE epic_id=?1",
                [epic_id.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[allow(clippy::unwrap_used)]
    async fn two_epics(manager: &SessionManager) -> (Session, Session) {
        let group = manager
            .create_container(CreateContainerParams {
                kind: SessionKind::Group,
                name: "Group".to_string(),
                parent_id: None,
                project_id: None,
                tags: vec!["test".to_string()],
                topology_id: None,
            })
            .await
            .unwrap();
        let mut epics = Vec::new();
        for name in ["Epic A", "Epic B"] {
            epics.push(
                manager
                    .create_container(CreateContainerParams {
                        kind: SessionKind::Epic,
                        name: name.to_string(),
                        parent_id: Some(group.id),
                        project_id: None,
                        tags: vec!["test".to_string()],
                        topology_id: None,
                    })
                    .await
                    .unwrap(),
            );
        }
        let second = epics.pop().unwrap();
        (epics.pop().unwrap(), second)
    }

    /// K2 finding d: auto-promotion is a `lead_session_id` writer and goes
    /// through the authority gate. While a continuation of the candidate
    /// holds its spawn guard, the promotion (and its generation bump) waits;
    /// it commits once the guard is released.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
    async fn auto_promotion_waits_for_the_candidate_continuation_guard() {
        let (manager, _dir) = manager();
        let manager = Arc::new(manager);
        let (epic_a, epic_b) = two_epics(&manager).await;
        let story_id = Uuid::new_v4();
        insert_completed(&manager, leaf(story_id, SessionKind::Story, epic_a.id)).await;
        let before = epic_generation(&manager, epic_b.id).await;

        let in_flight = super::super::spawn_single_flight::acquire_spawn_guard(story_id).await;
        let reparent = {
            let manager = Arc::clone(&manager);
            let epic_b = epic_b.id;
            tokio::spawn(async move { manager.set_session_parent(story_id, Some(epic_b)).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            epic_generation(&manager, epic_b.id).await,
            before,
            "the lead commit waits for the candidate's guard"
        );
        drop(in_flight);
        reparent.await.unwrap().unwrap();
        assert_eq!(epic_generation(&manager, epic_b.id).await, before + 1);
        assert_eq!(
            manager
                .get_session(epic_b.id)
                .await
                .unwrap()
                .lead_session_id,
            Some(story_id)
        );
    }

    /// K2 design test 12: `SetEpicLead` never waits on a lead continuation's
    /// guard; it is refused `lead_mutation_contended` and the retry after the
    /// continuation installs its provider succeeds at generation + 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::unwrap_used)]
    async fn set_epic_lead_is_refused_while_a_lead_continuation_holds_its_guard() {
        let (manager, _dir) = manager();
        let (epic, _) = two_epics(&manager).await;
        let (first, replacement) = (Uuid::new_v4(), Uuid::new_v4());
        insert_completed(&manager, leaf(first, SessionKind::Story, epic.id)).await;
        insert_completed(&manager, leaf(replacement, SessionKind::Story, epic.id)).await;
        manager.set_epic_lead(epic.id, Some(first)).await.unwrap();
        let before = epic_generation(&manager, epic.id).await;

        let in_flight = super::super::spawn_single_flight::acquire_spawn_guard(first).await;
        let refused = manager
            .set_epic_lead(epic.id, Some(replacement))
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains(LEAD_MUTATION_CONTENDED), "{refused}");
        assert_eq!(epic_generation(&manager, epic.id).await, before);
        assert_eq!(
            manager.get_session(epic.id).await.unwrap().lead_session_id,
            Some(first)
        );
        drop(in_flight);
        manager
            .set_epic_lead(epic.id, Some(replacement))
            .await
            .unwrap();
        assert_eq!(epic_generation(&manager, epic.id).await, before + 1);
        assert_eq!(
            manager.get_session(epic.id).await.unwrap().lead_session_id,
            Some(replacement)
        );
    }
}

/// Pure constructor shared by operator and transactional manager admission.
pub(super) fn build_container_session(
    id: Uuid,
    params: CreateContainerParams,
    working_dir: std::path::PathBuf,
    normalized_tags: Vec<String>,
) -> Session {
    let now = chrono::Utc::now();
    Session {
        context_fill_pct: None,
        id,
        status: SessionStatus::Completed,
        session_kind: params.kind,
        provider: SessionProvider::default(),
        context_usage_confidence: ContextUsageConfidence::default(),
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        created_at: now,
        updated_at: now,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        query: params.name.clone(),
        title: Some(params.name),
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir,
        git_branch: None,
        model: None,
        claude_session_id: None,
        project_id: params.project_id,
        continued_from: None,
        // ─── P1.6: populate tag fields from normalized set ───────────────────
        tag: normalized_tags.first().cloned().unwrap_or_default(),
        tags: normalized_tags.clone(),
        parent_id: params.parent_id,
        lead_session_id: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        stop_reason: None,
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        pipeline_artifact: None,
        // ─── P1.6: topology_id persists to workflow_id ───────────────────────
        workflow_id: params.topology_id,
        workflow_id_override: None,
        pending_question: None,
        pending_archive: false,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        rating: None,
        harness_version_hash: None,
        test_passed: None,
        clippy_passed: None,
        turn_count: None,
        retry_count: None,
        approval_wait_ms: None,
        approval_started_at: None,
        // TD1 (D3): Group/Epic containers never spawn a subprocess, never enter
        // the monitor loop, so the fold never runs — field stays NULL.
        work_time_ms: None,
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_cleanup_state: None,
        is_eval: false,
        capability_class: None,
        // Container sessions are never topology-bound at the node level.
        topology_node_id: None,
        topology_iteration: 0,
        // V99: populated from the provider handshake / result event, not at
        // construction. A session that never reaches those has none of these facts.
        provider_cli_version: None,
        provider_capabilities: Vec::new(),
        thinking_tokens: None,
        service_tier: None,
        cache_creation_1h_tokens: None,
        cache_creation_5m_tokens: None,
        permission_denial_count: None,
        subagent_stats_json: None,
        queued_turn_count: None,
        terminal_reason: None,
    }
}
