//! Native approval ingress and exact operator dispatch. A monitor lease owns
//! the actual writer; neither persisted UUIDs nor generic provider traits can
//! recreate one after restart.
use super::{
    SessionManager,
    types::{PersistenceHandle, TrackedSession},
};
use crate::{
    codex_app_server::AppServerWriter,
    error::Result,
    provider::ApprovalDecision,
    store::{harness_manager_v2::refused, manager_coordinator::ManagerDecisionDeliveryV2},
};
use rsi_common::types::{ConversationEvent, EventType, Role, SessionProvider, SessionStatus};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use uuid::Uuid;

static WRITERS: LazyLock<Mutex<HashMap<Uuid, Weak<ApprovalRuntime>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(test)]
type PublicationPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
static PUBLICATION_PAUSES: LazyLock<Mutex<HashMap<Uuid, PublicationPause>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
fn pause_next_publication_for_test(
    session: Uuid,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    PUBLICATION_PAUSES
        .lock()
        .unwrap()
        .insert(session, (entered_tx, release_rx));
    (entered_rx, release_tx)
}
pub(super) struct ApprovalRuntime {
    incarnation: Uuid,
    generation: u64,
    live: AtomicBool,
    writer: AppServerWriter,
    pending: AsyncMutex<HashMap<String, PendingApproval>>,
    capacity_sealed: AtomicBool,
}
struct PendingApproval {
    target: Value,
    resolution: Option<(ConversationEvent, Value)>,
    resolution_persisted: bool,
}

fn request_key(target: &Value) -> String {
    if target["request_id"].is_i64() || target["request_id"].is_string() {
        target["request_id"].to_string()
    } else {
        format!("unresolved:{}", target["publication_id"])
    }
}

pub(super) struct ApprovalLease {
    session: Uuid,
    runtime: Arc<ApprovalRuntime>,
}
impl Drop for ApprovalLease {
    fn drop(&mut self) {
        self.runtime.live.store(false, Ordering::SeqCst);
        let mut registry = WRITERS.lock().unwrap();
        if registry
            .get(&self.session)
            .is_some_and(|w| w.ptr_eq(&Arc::downgrade(&self.runtime)))
        {
            registry.remove(&self.session);
        }
    }
}
pub(super) fn register_writer(
    session: Uuid,
    generation: u64,
    writer: AppServerWriter,
) -> Option<ApprovalLease> {
    let mut registry = WRITERS.lock().unwrap();
    registry.retain(|_, w| w.strong_count() > 0);
    if registry
        .get(&session)
        .and_then(Weak::upgrade)
        .is_some_and(|r| r.generation > generation)
    {
        return None;
    }
    if registry.len() >= 4096 && !registry.contains_key(&session) {
        return None;
    }
    let runtime = Arc::new(ApprovalRuntime {
        incarnation: Uuid::new_v4(),
        generation,
        live: AtomicBool::new(true),
        writer,
        pending: AsyncMutex::new(HashMap::new()),
        capacity_sealed: AtomicBool::new(false),
    });
    if let Some(old) = registry
        .insert(session, Arc::downgrade(&runtime))
        .and_then(|w| w.upgrade())
    {
        old.live.store(false, Ordering::SeqCst);
    }
    Some(ApprovalLease { session, runtime })
}
pub(super) fn capacity_sealed(lease: &ApprovalLease) -> bool {
    lease.runtime.capacity_sealed.load(Ordering::SeqCst)
}

pub(super) fn live_incarnations() -> Vec<Uuid> {
    WRITERS
        .lock()
        .unwrap()
        .values()
        .filter_map(Weak::upgrade)
        .filter(|r| r.live.load(Ordering::SeqCst))
        .map(|r| r.incarnation)
        .collect()
}

fn remote_preview(
    raw: Option<&str>,
    cap: usize,
) -> crate::remote_read::Result<Option<crate::remote_read::BoundedText>> {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let mut end = raw.len().min(cap);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    Ok(Some(crate::remote_read::BoundedText {
        text: raw[..end].to_owned(),
        observed_bytes: u64::try_from(raw.len())
            .map_err(|_| crate::remote_read::ReadError::ResourceLimit)?,
        truncated: end < raw.len(),
    }))
}

/// Inspect one exact native publication witness. Upgrade only the selected
/// session's registry entry, release that mutex, then try-lock its bounded
/// pending set. No target JSON or writer handle leaves this read-only helper.
/// Missing and replaced observations never establish a tombstone or complete
/// coverage; the caller separately checks project ownership and Store state.
pub(super) fn remote_selected_native_approval(
    session: Uuid,
    selected: Uuid,
) -> crate::remote_read::Result<crate::remote_read::NativeRuntimeApprovalSnapshot> {
    use crate::remote_read::{
        NativeRuntimeApprovalRow, NativeRuntimeApprovalSnapshot, NativeRuntimeApprovalState,
        ReadError,
    };
    let started = std::time::Instant::now();
    let runtime = {
        let registry = WRITERS.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => ReadError::Busy,
            std::sync::TryLockError::Poisoned(_) => ReadError::SourceUnavailable,
        })?;
        registry.get(&session).and_then(Weak::upgrade)
    };
    let Some(runtime) = runtime else {
        return Ok(NativeRuntimeApprovalSnapshot {
            session,
            selected,
            observed_at: chrono::Utc::now(),
            state: NativeRuntimeApprovalState::Missing,
        });
    };
    if !runtime.live.load(Ordering::SeqCst) {
        return Ok(NativeRuntimeApprovalSnapshot {
            session,
            selected,
            observed_at: chrono::Utc::now(),
            state: NativeRuntimeApprovalState::SourceChanged,
        });
    }
    let pending = runtime.pending.try_lock().map_err(|_| ReadError::Busy)?;
    if pending.len() > crate::store::pending_approvals::MAX_PENDING_APPROVALS {
        return Err(ReadError::ResourceLimit);
    }
    let capacity = if runtime.capacity_sealed.load(Ordering::SeqCst) {
        0
    } else {
        u32::try_from(crate::store::pending_approvals::MAX_PENDING_APPROVALS - pending.len())
            .map_err(|_| ReadError::ResourceLimit)?
    };
    let mut found = None;
    for approval in pending.values() {
        let raw_id = approval.target["publication_id"]
            .as_str()
            .ok_or(ReadError::InvalidSource)?;
        let id = crate::remote_read::source_uuid(raw_id)?;
        if id != selected {
            continue;
        }
        if found.is_some() {
            return Err(ReadError::InvalidSource);
        }
        found = Some(NativeRuntimeApprovalRow {
            id,
            incarnation_id: runtime.incarnation,
            spawn_generation: runtime.generation,
            method: remote_preview(approval.target["method"].as_str(), 512)?,
            description: remote_preview(approval.target["description"].as_str(), 2048)?,
            resolution_observed: approval.resolution.is_some(),
            resolution_persisted: approval.resolution_persisted,
            writer_live: true,
            writer_capacity: capacity,
        });
    }
    drop(pending);
    let observed_at = chrono::Utc::now();
    let current = {
        let registry = WRITERS.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => ReadError::Busy,
            std::sync::TryLockError::Poisoned(_) => ReadError::SourceUnavailable,
        })?;
        registry.get(&session).and_then(Weak::upgrade)
    };
    let state = if !runtime.live.load(Ordering::SeqCst)
        || current
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, &runtime))
    {
        NativeRuntimeApprovalState::SourceChanged
    } else {
        found.map_or(
            NativeRuntimeApprovalState::Missing,
            NativeRuntimeApprovalState::Present,
        )
    };
    if started.elapsed() > Duration::from_millis(5) {
        return Err(ReadError::ResourceLimit);
    }
    Ok(NativeRuntimeApprovalSnapshot {
        session,
        selected,
        observed_at,
        state,
    })
}

/// Reobserve after Store work, with no overlapping lock. Equality reports
/// only no bounded change; a missing or replaced witness is always changed.
pub(super) fn remote_selected_native_approval_recheck(
    before: &crate::remote_read::NativeRuntimeApprovalSnapshot,
) -> crate::remote_read::Result<crate::remote_read::NativeRuntimeApprovalRecheckObservation> {
    use crate::remote_read::{
        NativeRuntimeApprovalRecheck, NativeRuntimeApprovalRecheckObservation,
        NativeRuntimeApprovalState,
    };
    let after = remote_selected_native_approval(before.session, before.selected)?;
    Ok(NativeRuntimeApprovalRecheckObservation {
        state: if matches!(&before.state, NativeRuntimeApprovalState::Present(_))
            && before.state == after.state
        {
            NativeRuntimeApprovalRecheck::NoObservedChange
        } else {
            NativeRuntimeApprovalRecheck::Changed
        },
        observed_at: after.observed_at,
    })
}

/// Capture the bounded native runtime source for one session without cloning
/// target JSON or retaining a writer. A missing/replaced registry entry is
/// unavailable source evidence, not an empty complete list. Project scope and
/// Store ownership must be established separately by the read caller.
pub(super) fn remote_native_approval_list(
    session: Uuid,
) -> crate::remote_read::Result<crate::remote_read::NativeRuntimeApprovalListSnapshot> {
    use crate::remote_read::{
        NativeRuntimeApprovalListSnapshot, NativeRuntimeApprovalListState,
        NativeRuntimeApprovalRow, ReadError,
    };
    let started = std::time::Instant::now();
    let runtime = {
        let registry = WRITERS.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => ReadError::Busy,
            std::sync::TryLockError::Poisoned(_) => ReadError::SourceUnavailable,
        })?;
        registry.get(&session).and_then(Weak::upgrade)
    };
    let Some(runtime) = runtime else {
        return Ok(NativeRuntimeApprovalListSnapshot {
            session,
            observed_at: chrono::Utc::now(),
            state: NativeRuntimeApprovalListState::Missing,
        });
    };
    if !runtime.live.load(Ordering::SeqCst) {
        return Ok(NativeRuntimeApprovalListSnapshot {
            session,
            observed_at: chrono::Utc::now(),
            state: NativeRuntimeApprovalListState::SourceChanged,
        });
    }
    let pending = runtime.pending.try_lock().map_err(|_| ReadError::Busy)?;
    if pending.len() > crate::store::pending_approvals::MAX_PENDING_APPROVALS {
        return Err(ReadError::ResourceLimit);
    }
    let capacity = if runtime.capacity_sealed.load(Ordering::SeqCst) {
        0
    } else {
        u32::try_from(crate::store::pending_approvals::MAX_PENDING_APPROVALS - pending.len())
            .map_err(|_| ReadError::ResourceLimit)?
    };
    let mut rows = Vec::with_capacity(pending.len());
    for approval in pending.values() {
        let raw_id = approval.target["publication_id"]
            .as_str()
            .ok_or(ReadError::InvalidSource)?;
        rows.push(NativeRuntimeApprovalRow {
            id: crate::remote_read::source_uuid(raw_id)?,
            incarnation_id: runtime.incarnation,
            spawn_generation: runtime.generation,
            method: remote_preview(approval.target["method"].as_str(), 512)?,
            description: remote_preview(approval.target["description"].as_str(), 2048)?,
            resolution_observed: approval.resolution.is_some(),
            resolution_persisted: approval.resolution_persisted,
            writer_live: true,
            writer_capacity: capacity,
        });
    }
    drop(pending);
    rows.sort_unstable_by_key(|row| row.id);
    if rows.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err(ReadError::InvalidSource);
    }
    let observed_at = chrono::Utc::now();
    let current = {
        let registry = WRITERS.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => ReadError::Busy,
            std::sync::TryLockError::Poisoned(_) => ReadError::SourceUnavailable,
        })?;
        registry.get(&session).and_then(Weak::upgrade)
    };
    let state = if !runtime.live.load(Ordering::SeqCst)
        || current
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, &runtime))
    {
        NativeRuntimeApprovalListState::SourceChanged
    } else {
        NativeRuntimeApprovalListState::Present(rows)
    };
    if started.elapsed() > Duration::from_millis(5) {
        return Err(ReadError::ResourceLimit);
    }
    Ok(NativeRuntimeApprovalListSnapshot {
        session,
        observed_at,
        state,
    })
}

/// A matching post-Store reread reports no bounded change only. It is not a
/// transaction or proof that the runtime source stayed stable between reads.
pub(super) fn remote_native_approval_list_recheck(
    before: &crate::remote_read::NativeRuntimeApprovalListSnapshot,
) -> crate::remote_read::Result<crate::remote_read::NativeRuntimeApprovalListRecheckObservation> {
    use crate::remote_read::{
        NativeRuntimeApprovalListRecheckObservation, NativeRuntimeApprovalListState,
        NativeRuntimeApprovalRecheck,
    };
    let after = remote_native_approval_list(before.session)?;
    Ok(NativeRuntimeApprovalListRecheckObservation {
        state: if matches!(&before.state, NativeRuntimeApprovalListState::Present(_))
            && before.state == after.state
        {
            NativeRuntimeApprovalRecheck::NoObservedChange
        } else {
            NativeRuntimeApprovalRecheck::Changed
        },
        observed_at: after.observed_at,
    })
}

/// Called directly by the production monitor on the normalized provider frame.
/// Update the runtime witness before awaiting FIFO persistence, preventing an
/// old answer from passing while a reused ID/new publication is not yet durable.
#[allow(clippy::too_many_arguments)]
pub(super) async fn publish_monitor_approval(
    lease: Option<&ApprovalLease>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    persistence: &PersistenceHandle,
    session: Uuid,
    generation: u64,
    launch_invocation: Option<Uuid>,
    invocation: Option<Uuid>,
    sequence: &mut i32,
    data: &Value,
) -> Result<ConversationEvent> {
    let mut witness = match lease {
        Some(l) => Some(l.runtime.pending.lock().await),
        None => None,
    };
    if lease.is_some_and(|l| l.runtime.capacity_sealed.load(Ordering::SeqCst)) {
        return Err(refused("approval_pending_capacity_exceeded"));
    }
    let publication = Uuid::new_v4();
    let complete = serde_json::to_vec(data)?.len() <= 16384;
    *sequence = sequence
        .checked_add(1)
        .ok_or_else(|| refused("approval_sequence_exhausted"))?;
    let mut target = json!({"kind":"appserver_approval","session_id":session,"publication_id":publication,
        "incarnation_id":lease.map(|l|l.runtime.incarnation).unwrap_or_else(Uuid::new_v4),"spawn_generation":generation,
        "launch_invocation_id":launch_invocation,"model_invocation_id":invocation,
        "event_id":0,"event_sequence":*sequence,"request_id":data["request_id"],
        "method":data["method"].as_str().unwrap_or("unrecognized approval method").chars().take(256).collect::<String>(),
        "description":data["description"].as_str().unwrap_or("").chars().take(2048).collect::<String>(),
        "params":if complete {data["params"].clone()} else {json!({"state":"oversized_unresolved"})},
        "payload_complete":complete && lease.is_some()});
    // Never turn an oversized JSON ID into a reply to a truncated identity.
    if serde_json::to_vec(&target["request_id"])?.len() > 4096 {
        target["request_identity_digest"] = json!(crate::store::harness_manager_v2::fingerprint(
            &target["request_id"]
        )?);
        target["request_id"] = Value::Null;
        target["payload_complete"] = json!(false);
    }
    let key = request_key(&target);
    let overflow = witness.as_ref().is_some_and(|w| {
        !w.contains_key(&key) && w.len() >= crate::store::pending_approvals::MAX_PENDING_APPROVALS
    });
    target["overflow"] = json!(overflow);
    if overflow {
        target["payload_complete"] = json!(false);
        if let Some(lease) = lease {
            lease.runtime.capacity_sealed.store(true, Ordering::SeqCst);
        }
    }
    let mut event = ConversationEvent {
        id: 0,
        session_id: session,
        sequence: *sequence,
        event_type: EventType::ToolUse,
        role: Some(Role::Assistant),
        content: format!(
            "Pending AppServer approval: {} (request {})",
            target["description"], target["request_id"]
        ),
        tool_name: target["method"].as_str().map(str::to_owned),
        tool_input: Some(Box::new(target.clone())),
        tool_use_id: Some(publication.to_string()),
        created_at: chrono::Utc::now(),
        metadata: None,
        offload_id: None,
    };
    {
        let mut guard = active.write().await;
        let tracked = guard
            .get_mut(&session)
            .filter(|t| t.spawn_generation == generation)
            .ok_or_else(|| refused("approval_runtime_changed"))?;
        if tracked.session.provider != SessionProvider::CodexAppServer {
            target["payload_complete"] = json!(false);
        }
        tracked.session.status = SessionStatus::WaitingApproval;
        tracked.last_event_at = chrono::Utc::now();
        tracked
            .approval_wait_start
            .get_or_insert_with(std::time::Instant::now);
        tracked.events.push(event.clone());
        if let Some(w) = witness.as_mut() {
            if !overflow {
                w.insert(
                    key.clone(),
                    PendingApproval {
                        target: target.clone(),
                        resolution: None,
                        resolution_persisted: false,
                    },
                );
            }
        }
    }
    #[cfg(test)]
    if let Some((entered, release)) = { PUBLICATION_PAUSES.lock().unwrap().remove(&session) } {
        let _ = entered.send(());
        let _ = release.await;
    }
    event.id = persistence
        .publish_appserver_approval(event.clone(), target.clone())
        .await?;
    target["event_id"] = json!(event.id);
    if let Some(w) = witness.as_mut() {
        if let Some(entry) = w.get_mut(&key) {
            entry.target = target;
        }
    }
    let mut guard = active.write().await;
    if let Some(t) = guard
        .get_mut(&session)
        .filter(|t| t.spawn_generation == generation)
    {
        if let Some(e) = t
            .events
            .iter_mut()
            .rev()
            .find(|e| e.tool_use_id == event.tool_use_id)
        {
            e.id = event.id;
        }
    }
    if overflow {
        return Err(refused("approval_pending_capacity_exceeded"));
    }
    Ok(event)
}

fn apply_approval_waiting(tracked: &mut TrackedSession, waiting: bool) {
    if waiting {
        return;
    }
    if tracked.session.status == SessionStatus::WaitingApproval {
        tracked.session.status = SessionStatus::Running;
    }
    if let Some(start) = tracked.approval_wait_start.take() {
        tracked.approval_wait_total_ms = tracked
            .approval_wait_total_ms
            .saturating_add(start.elapsed().as_millis() as u64);
    }
}

/// Publication and resolution arrive on the same ordered provider event queue.
/// Capture the occurrence under its writer witness before awaiting persistence.
/// A failed persistence leaves this witness non-answerable and retry-owned.
pub(super) async fn resolve_monitor_approval(
    lease: Option<&ApprovalLease>,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    persistence: &PersistenceHandle,
    session: Uuid,
    generation: u64,
    sequence: &mut i32,
    resolution: &Value,
) -> Result<()> {
    let Some(lease) = lease else {
        return Err(refused("approval_resolution_writer_unavailable"));
    };
    if !(resolution["requestId"].is_i64() || resolution["requestId"].is_string())
        || !resolution["threadId"].is_string()
    {
        return Err(refused("approval_resolution_identity_mismatch"));
    }
    if !lease.runtime.live.load(Ordering::SeqCst) {
        return Err(refused("approval_resolution_writer_changed"));
    }
    let mut pending = lease.runtime.pending.lock().await;
    let key = resolution["requestId"].to_string();
    let Some(entry) = pending.get_mut(&key) else {
        return Ok(());
    };
    if resolution["threadId"].as_str().is_none()
        || resolution["threadId"] != entry.target["params"]["threadId"]
    {
        return Err(refused("approval_resolution_identity_mismatch"));
    }
    if entry.resolution_persisted {
        return Ok(());
    }
    if entry.resolution.is_none() {
        *sequence = sequence
            .checked_add(1)
            .ok_or_else(|| refused("approval_sequence_exhausted"))?;
        let event = ConversationEvent {
            id: 0,
            session_id: session,
            sequence: *sequence,
            event_type: EventType::ToolResult,
            role: None,
            content: format!(
                "AppServer request {} closed or answered by provider; operator answer consumption unconfirmed",
                resolution["requestId"]
            ),
            tool_name: Some("serverRequest/resolved".into()),
            tool_input: Some(Box::new(resolution.clone())),
            tool_use_id: entry.target["publication_id"].as_str().map(str::to_owned),
            created_at: chrono::Utc::now(),
            metadata: None,
            offload_id: None,
        };
        let mut guard = active.write().await;
        let tracked = guard
            .get_mut(&session)
            .filter(|t| t.spawn_generation == generation)
            .ok_or_else(|| refused("approval_resolution_writer_changed"))?;
        if !lease.runtime.live.load(Ordering::SeqCst) {
            return Err(refused("approval_resolution_writer_changed"));
        }
        tracked.events.push(event.clone());
        entry.resolution = Some((event, resolution.clone()));
    }
    let (event, resolution) = entry.resolution.as_ref().unwrap();
    let (closed, waiting) = persistence
        .resolve_appserver_approval(event.clone(), entry.target.clone(), resolution.clone())
        .await?;
    entry.resolution_persisted = true;
    if closed {
        pending.remove(&key);
    }
    let mut guard = active.write().await;
    if let Some(tracked) = guard
        .get_mut(&session)
        .filter(|t| t.spawn_generation == generation)
    {
        apply_approval_waiting(tracked, waiting);
    }
    Ok(())
}

impl SessionManager {
    /// Retry only persistence of already-observed source closure, never an
    /// operator response. Registry and each pending set have fixed bounds.
    pub(super) async fn retry_appserver_approval_closures(&self) -> Result<usize> {
        let writers: Vec<_> = WRITERS
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(id, w)| w.upgrade().map(|w| (*id, w)))
            .collect();
        let mut changed = 0;
        for (session, runtime) in writers {
            if !runtime.live.load(Ordering::SeqCst) {
                continue;
            }
            let mut pending = runtime.pending.lock().await;
            let keys: Vec<_> = pending
                .iter()
                .filter(|(_, p)| p.resolution.is_some() && !p.resolution_persisted)
                .map(|(k, _)| k.clone())
                .collect();
            if keys.is_empty() {
                continue;
            }
            let mut active = self.active.write().await;
            let Some(tracked) = active
                .get_mut(&session)
                .filter(|t| t.spawn_generation == runtime.generation)
            else {
                continue;
            };
            if !runtime.live.load(Ordering::SeqCst) {
                continue;
            }
            let store = self.store.lock().await;
            for key in keys {
                let entry = pending.get_mut(&key).unwrap();
                let (event, resolution) = entry.resolution.as_ref().unwrap();
                match store.resolve_appserver_approval(event, &entry.target, resolution) {
                    Ok(closed) => {
                        entry.resolution_persisted = true;
                        if closed {
                            pending.remove(&key);
                        }
                        changed += 1;
                    }
                    Err(error) => {
                        tracing::warn!(%session,%error,"provider closure persistence remains owned; no answer resend")
                    }
                }
            }
            apply_approval_waiting(tracked, store.appserver_approval_waiting(session)?);
        }
        Ok(changed)
    }

    /// Separate native transport route. The Claude continuation implementation
    /// and its post-establishment cleanup remain unchanged.
    pub(super) async fn deliver_manager_decision(
        &self,
        delivery: ManagerDecisionDeliveryV2,
    ) -> Result<()> {
        if delivery.target["kind"] == "appserver_approval" {
            self.deliver_appserver_approval(delivery).await
        } else {
            self.continue_manager_decision(delivery).await
        }
    }

    async fn deliver_appserver_approval(&self, delivery: ManagerDecisionDeliveryV2) -> Result<()> {
        let session = super::manager_coordinator::decision_session(&delivery)?;
        let _spawn = super::spawn_single_flight::acquire_spawn_guard(session).await;
        let runtime = WRITERS
            .lock()
            .unwrap()
            .get(&session)
            .and_then(Weak::upgrade)
            .ok_or_else(|| refused("manager_v2_approval_writer_unavailable"))?;
        let pending = runtime.pending.lock().await;
        if !runtime.live.load(Ordering::SeqCst)
            || runtime.capacity_sealed.load(Ordering::SeqCst)
            || !pending
                .get(&request_key(&delivery.target))
                .is_some_and(|p| p.target == delivery.target && p.resolution.is_none())
            || delivery.target["incarnation_id"] != runtime.incarnation.to_string()
        {
            return Err(refused("manager_v2_approval_incarnation_changed"));
        }
        let decision = match delivery.answer.trim() {
            "approve" => ApprovalDecision::Approve,
            "deny" => ApprovalDecision::Deny,
            _ => {
                return Err(refused(
                    "manager_v2_approval_answer_must_be_approve_or_deny",
                ));
            }
        };
        let request_id = &delivery.target["request_id"];
        let method = delivery.target["method"]
            .as_str()
            .ok_or_else(|| refused("manager_v2_approval_method_missing"))?;
        let prepared = tokio::time::timeout(
            Duration::from_secs(2),
            runtime.writer.prepare_approval(
                request_id,
                method,
                &delivery.target["params"],
                decision,
            ),
        )
        .await
        .map_err(|_| refused("manager_v2_approval_writer_capacity"))??;
        // Lock order matches ingress: witness -> active -> Store. Capacity is
        // reserved before these locks; enqueue itself has no await. Hold the
        // actual incarnation and current authority through the provider effect.
        let mut active = self.active.write().await;
        let _tracked = active
            .get_mut(&session)
            .filter(|t| {
                t.spawn_generation == runtime.generation
                    && t.session.provider == SessionProvider::CodexAppServer
                    && !t.interrupt_requested
            })
            .ok_or_else(|| refused("manager_v2_approval_runtime_changed"))?;
        if !runtime.live.load(Ordering::SeqCst) {
            return Err(refused("manager_v2_approval_writer_unavailable"));
        }
        let store = self.store.lock().await;
        let started = store.manager_v2_set_decision_delivery(
            &delivery,
            "running",
            true,
            Some("native approval write intent committed before enqueue".into()),
        )?;
        prepared.enqueue();
        // Failure here leaves durable effect_started and the human gate intact;
        // recovery exposes uncertainty and never repeats the enqueue.
        store.finish_appserver_approval_enqueue(&started)?;
        // Keep this occurrence until exact provider closure; other requests
        // and the response's unconfirmed consumption remain independently owned.
        Ok(())
    }
}

#[cfg(test)]
mod tests;
