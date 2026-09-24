//! Private operator-only archive cleanup saga for one exact terminal leaf.

use super::SessionManager;
use crate::bus::DaemonEvent;
use crate::error::{DaemonError, Result};
use crate::sandbox::cleanup::OwnershipObservation;
use crate::sandbox::git_worktree;
use crate::store::archive_cleanup::{
    ArchiveCleanupRecoveryCursor, ArchiveCleanupRun, ArchiveProjectionConsumer,
    NewArchiveCleanupIntent, archive_topology_digest, digest_field,
};
use crate::store::sandbox_custody::PersistedCustody;
use rsi_common::archive_cleanup::{
    ARCHIVE_CLEANUP_SCHEMA_VERSION, ArchiveCleanupErrorV1, ArchiveCleanupPhaseV1,
    ArchiveCleanupReceiptV1, ArchiveCleanupSafeCodeV1, ArchiveCleanupStatusV1,
    ArchivePreservationClassV1, ArchiveSessionResultV1,
};
use rsi_common::cohort_settlement::{
    SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST, SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST,
    SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS,
};
use rsi_common::types::{SandboxCleanupState, SandboxKind, Session, SessionStatus};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

const ARCHIVE_CLEANUP_RPC_ERROR: i32 = -32071;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveProjectionDispatchFaultPoint {
    BeforeApplication,
    AfterApplicationBeforeAcknowledgement,
}

#[cfg(test)]
static ARCHIVE_PROJECTION_DISPATCH_FAULT: std::sync::Mutex<
    Vec<(
        Uuid,
        ArchiveProjectionConsumer,
        ArchiveProjectionDispatchFaultPoint,
    )>,
> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn install_archive_projection_dispatch_fault(
    projection_id: Uuid,
    consumer: ArchiveProjectionConsumer,
    point: ArchiveProjectionDispatchFaultPoint,
) {
    ARCHIVE_PROJECTION_DISPATCH_FAULT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((projection_id, consumer, point));
}

#[cfg(test)]
fn fail_archive_projection_dispatch_if_requested(
    projection_id: Uuid,
    consumer: ArchiveProjectionConsumer,
    point: ArchiveProjectionDispatchFaultPoint,
) -> Result<()> {
    let mut requested = ARCHIVE_PROJECTION_DISPATCH_FAULT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(index) = requested
        .iter()
        .position(|requested| *requested == (projection_id, consumer, point))
    {
        requested.swap_remove(index);
        return Err(DaemonError::Store(format!(
            "injected archive projection dispatch fault at {point:?}"
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct ArchiveProof {
    intent: NewArchiveCleanupIntent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveRemovalAuthorityV1 {
    version: u32,
    run_id: Uuid,
    session_id: Uuid,
    custody_id: Uuid,
    custody_generation: u64,
    preservation_class: ArchivePreservationClassV1,
    source_ref: String,
    source_oid: String,
    target_ref: Option<String>,
    target_oid: Option<String>,
    original_root_digest: String,
    quarantine_root_digest: String,
    repository_identity_digest: String,
    git_admin_dir_digest: String,
    git_admin_id: String,
    clean_state_digest: String,
    stable_tree_digest: String,
    dependency_digest: String,
    holder_digest: String,
    journal_evidence_digest: String,
    branch_preserved: bool,
}

impl SessionManager {
    pub(super) async fn try_archive_cleanup(
        &self,
        session_id: Uuid,
    ) -> Result<Option<ArchiveSessionResultV1>> {
        if let Some(receipt) = self.settled_archive_cleanup_receipt(session_id).await? {
            self.finish_archive_cleanup_projection(session_id).await?;
            return ArchiveSessionResultV1::cleanup_settled(receipt)
                .map(Some)
                .map_err(DaemonError::Store);
        }

        let session = {
            let store = self.store.lock().await;
            store
                .get_session(session_id)?
                .ok_or(DaemonError::SessionNotFound(session_id))?
        };
        if !self.archive_cleanup_route_selected(&session).await? {
            return Ok(None);
        }

        let _spawn_guard = super::spawn_single_flight::acquire_spawn_guard(session_id).await;
        let _cwd_guard = super::spawn_single_flight::acquire_settlement_cwd_exclusion().await;
        if self.active.read().await.contains_key(&session_id) {
            return Err(archive_cleanup_error(
                ArchiveCleanupSafeCodeV1::SessionActive,
                None,
                None,
                true,
            ));
        }
        let completed_matches =
            self.completed
                .read()
                .await
                .get(&session_id)
                .is_some_and(|completed| {
                    completed.session.status == session.status
                        && completed.session.updated_at == session.updated_at
                        && completed.session.sandbox_root == session.sandbox_root
                        && completed.session.sandbox_branch == session.sandbox_branch
                        && completed.retry_cancel.is_none()
                        && completed.retry_fired_at.is_none()
                        && completed.superseded_by_retry.is_none()
                });
        if !completed_matches {
            return Err(archive_cleanup_error(
                ArchiveCleanupSafeCodeV1::SessionTopologyChanged,
                None,
                None,
                true,
            ));
        }
        tokio::task::spawn_blocking(move || {
            super::reaper::prove_archive_cleanup_has_no_provider_processes(&[session_id])
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))?
        .map_err(|_| {
            archive_cleanup_error(
                ArchiveCleanupSafeCodeV1::ProcessHolderPresent,
                None,
                None,
                true,
            )
        })?;

        let store = Arc::clone(&self.store);
        let sandbox_base = self.sandbox_allocator.base_dir().to_path_buf();
        let receipt = tokio::task::spawn_blocking(move || {
            resume_or_start_cleanup_blocking(&store, &sandbox_base, session_id)
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))??;
        drop(_cwd_guard);
        drop(_spawn_guard);
        self.finish_archive_cleanup_projection(session_id).await?;
        ArchiveSessionResultV1::cleanup_settled(receipt)
            .map(Some)
            .map_err(DaemonError::Store)
    }

    async fn archive_cleanup_route_selected(&self, session: &Session) -> Result<bool> {
        if !archive_cleanup_route_applies(session) {
            return Ok(false);
        }
        let (Some(root), Some(branch), Some(working_dir)) = (
            session.sandbox_root.as_deref(),
            session.sandbox_branch.as_deref(),
            session.working_dir.to_str(),
        ) else {
            return Ok(false);
        };
        let Some(root_text) = root.to_str() else {
            return Ok(false);
        };
        if self.active.read().await.contains_key(&session.id) {
            return Ok(false);
        }
        let completed_matches =
            self.completed
                .read()
                .await
                .get(&session.id)
                .is_some_and(|completed| {
                    completed.session.status == session.status
                        && completed.session.updated_at == session.updated_at
                        && completed.session.sandbox_root == session.sandbox_root
                        && completed.session.sandbox_branch == session.sandbox_branch
                        && completed.retry_cancel.is_none()
                        && completed.retry_fired_at.is_none()
                        && completed.superseded_by_retry.is_none()
                });
        if !completed_matches {
            return Ok(false);
        }
        {
            let store = self.store.lock().await;
            if !store.list_descendants(session.id)?.is_empty() {
                return Ok(false);
            }
            let custody = match store.live_custody_for_session(session.id) {
                Ok(custody) => custody,
                Err(_) => return Ok(false),
            };
            if custody.owner_session_id != session.id
                || custody.sandbox_root != root_text
                || custody.sandbox_branch != branch
                || custody.canonical_repo_dir != working_dir
            {
                return Ok(false);
            }
        }
        let initial = super::lifecycle::observe_cleanup_ownership(
            &self.active,
            &self.completed,
            &self.store,
            session.id,
            root,
        )
        .await;
        tokio::task::yield_now().await;
        let final_observation = super::lifecycle::observe_cleanup_ownership(
            &self.active,
            &self.completed,
            &self.store,
            session.id,
            root,
        )
        .await;
        Ok(matches!(initial, OwnershipObservation::Exclusive)
            && matches!(final_observation, OwnershipObservation::Exclusive))
    }

    pub(crate) async fn get_archive_cleanup_status(
        &self,
        session_id: Uuid,
    ) -> Result<ArchiveCleanupStatusV1> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            store.blocking_lock().archive_cleanup_status(session_id)
        })
        .await
        .map_err(|error| DaemonError::Store(error.to_string()))?
    }

    pub(super) async fn recover_archive_cleanups(&self) -> Result<()> {
        let mut cursor: Option<ArchiveCleanupRecoveryCursor> = None;
        loop {
            let store = Arc::clone(&self.store);
            let page_cursor = cursor.clone();
            let page = tokio::task::spawn_blocking(move || {
                store.blocking_lock().archive_cleanup_recovery_page(
                    page_cursor.as_ref(),
                    crate::store::archive_cleanup::ARCHIVE_CLEANUP_RECOVERY_BATCH,
                )
            })
            .await
            .map_err(|error| DaemonError::Store(error.to_string()))??;
            if page.runs.is_empty() {
                break;
            }
            for run in &page.runs {
                let session_id = run.session_id;
                let _spawn_guard =
                    super::spawn_single_flight::acquire_spawn_guard(session_id).await;
                let _cwd_guard =
                    super::spawn_single_flight::acquire_settlement_cwd_exclusion().await;
                if self.active.read().await.contains_key(&session_id) {
                    continue;
                }
                let provider_proof = tokio::task::spawn_blocking(move || {
                    super::reaper::prove_archive_cleanup_has_no_provider_processes(&[session_id])
                })
                .await;
                if !matches!(provider_proof, Ok(Ok(()))) {
                    tracing::warn!(%session_id, "archive cleanup recovery retained a live process candidate");
                    continue;
                }
                let store = Arc::clone(&self.store);
                let sandbox_base = self.sandbox_allocator.base_dir().to_path_buf();
                let settled = match tokio::task::spawn_blocking(move || {
                    resume_or_start_cleanup_blocking(&store, &sandbox_base, session_id)
                })
                .await
                {
                    Ok(Ok(_)) => true,
                    Ok(Err(error)) => {
                        tracing::warn!(%session_id, error=%error, "archive cleanup recovery retained durable evidence");
                        false
                    }
                    Err(error) => {
                        tracing::warn!(%session_id, error=%error, "archive cleanup recovery task failed");
                        false
                    }
                };
                drop(_cwd_guard);
                drop(_spawn_guard);
                if settled
                    && let Err(error) = self.finish_archive_cleanup_projection(session_id).await
                {
                    tracing::warn!(%session_id, %error, "archive cleanup projection remains recoverable");
                }
            }
            let last = page.runs.last().expect("nonempty recovery page");
            cursor = Some(ArchiveCleanupRecoveryCursor {
                updated_at: last.updated_at.clone(),
                run_id: last.run_id,
            });
            if !page.has_more {
                break;
            }
        }
        self.recover_archive_cleanup_projections().await?;
        Ok(())
    }

    async fn settled_archive_cleanup_receipt(
        &self,
        session_id: Uuid,
    ) -> Result<Option<ArchiveCleanupReceiptV1>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.current_settled_archive_cleanup_receipt(session_id)
        })
        .await
        .map_err(|error| DaemonError::Store(error.to_string()))?
    }

    async fn recover_archive_cleanup_projections(&self) -> Result<()> {
        let memory_available = self.memory_handle.is_some();
        loop {
            let store = Arc::clone(&self.store);
            let projections = tokio::task::spawn_blocking(move || {
                store
                    .blocking_lock()
                    .archive_cleanup_projection_recovery_page(
                        memory_available,
                        crate::store::archive_cleanup::ARCHIVE_CLEANUP_RECOVERY_BATCH,
                    )
            })
            .await
            .map_err(|error| DaemonError::Store(error.to_string()))??;
            if projections.is_empty() {
                return Ok(());
            }
            let count = projections.len();
            for projection in projections {
                self.finish_archive_cleanup_projection(projection.session_id)
                    .await?;
            }
            if count < crate::store::archive_cleanup::ARCHIVE_CLEANUP_RECOVERY_BATCH as usize {
                return Ok(());
            }
        }
    }

    async fn finish_archive_cleanup_projection(&self, session_id: Uuid) -> Result<()> {
        let projection_guard = super::spawn_single_flight::acquire_spawn_guard(session_id).await;
        let memory_available = self.memory_handle.is_some();
        loop {
            let store = Arc::clone(&self.store);
            let projections = tokio::task::spawn_blocking(move || {
                store
                    .blocking_lock()
                    .archive_cleanup_projection_session_page(
                        session_id,
                        memory_available,
                        crate::store::archive_cleanup::ARCHIVE_CLEANUP_RECOVERY_BATCH,
                    )
            })
            .await
            .map_err(|error| DaemonError::Store(error.to_string()))??;
            if projections.is_empty() {
                return Ok(());
            }
            let count = projections.len();
            for projection in projections {
                debug_assert_eq!(projection.session_id, session_id);
                for consumer in projection.pending_consumers {
                    let store = Arc::clone(&self.store);
                    let projection_id = projection.projection_id;
                    let claimed = tokio::task::spawn_blocking(move || {
                        store
                            .blocking_lock()
                            .begin_archive_cleanup_projection_consumer(projection_id, consumer)
                    })
                    .await
                    .map_err(|error| DaemonError::Store(error.to_string()))??;
                    if !claimed {
                        continue;
                    }
                    #[cfg(test)]
                    fail_archive_projection_dispatch_if_requested(
                        projection_id,
                        consumer,
                        ArchiveProjectionDispatchFaultPoint::BeforeApplication,
                    )?;
                    // The effect must accept the stable projection identity before
                    // its durable consumer receipt advances. A failure leaves the
                    // consumer in `delivering`, so restart recovery retries the
                    // same projection ID instead of losing the effect.
                    let bus_delivery_guard = match consumer {
                        ArchiveProjectionConsumer::Watch => {
                            self.clear_lead_pointers_to_held(session_id, &projection_guard)
                                .await?;
                            if let Some(mut completed) =
                                self.completed.write().await.remove(&session_id)
                                && let Some(cancel) = completed.retry_cancel.take()
                            {
                                let _ = cancel.send(());
                            }
                            None
                        }
                        ArchiveProjectionConsumer::Bus => {
                            let Some(guard) = self
                                .event_bus
                                .begin_archive_projection_delivery(projection.projection_id)
                            else {
                                // Another delivery owns the effect-to-ack span.
                                // Leave this claim unacknowledged for that owner.
                                continue;
                            };
                            self.event_bus.publish(DaemonEvent::SessionArchived {
                                session_id,
                                projection_id: Some(projection.projection_id),
                            });
                            Some(guard)
                        }
                        ArchiveProjectionConsumer::Memory => {
                            let Some(handle) = self.memory_handle.clone() else {
                                // An unavailable worker has accepted no effect. Keep
                                // the durable claim retryable instead of converting
                                // absence into a false delivery acknowledgement.
                                continue;
                            };
                            let reason = format!(
                                "session_archived:{session_id}:{}",
                                projection.projection_id
                            );
                            handle
                                .sync_archive_projection(projection.projection_id, &reason)
                                .await?;
                            None
                        }
                    };
                    #[cfg(test)]
                    fail_archive_projection_dispatch_if_requested(
                        projection_id,
                        consumer,
                        ArchiveProjectionDispatchFaultPoint::AfterApplicationBeforeAcknowledgement,
                    )?;
                    let store = Arc::clone(&self.store);
                    tokio::task::spawn_blocking(move || {
                        store
                            .blocking_lock()
                            .complete_archive_cleanup_projection_consumer(projection_id, consumer)
                    })
                    .await
                    .map_err(|error| DaemonError::Store(error.to_string()))??;
                    drop(bus_delivery_guard);
                }
            }
            if count < crate::store::archive_cleanup::ARCHIVE_CLEANUP_RECOVERY_BATCH as usize {
                return Ok(());
            }
        }
    }
}

fn archive_cleanup_route_applies(session: &Session) -> bool {
    rsi_common::is_leaf_kind(session.session_kind)
        && matches!(
            session.status,
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted
        )
        && session.sandbox_kind == Some(SandboxKind::GitWorktree)
        && session.sandbox_cleanup_state == Some(SandboxCleanupState::Live)
        && !session.pending_archive
}

fn resume_or_start_cleanup_blocking(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    sandbox_base: &Path,
    session_id: Uuid,
) -> Result<ArchiveCleanupReceiptV1> {
    let settled = {
        let store = store.blocking_lock();
        store.current_settled_archive_cleanup_receipt(session_id)?
    };
    if let Some(receipt) = settled {
        return Ok(receipt);
    }
    let existing = {
        let store = store.blocking_lock();
        store.latest_archive_cleanup_for_session(session_id)?
    };
    if let Some(run) = existing {
        match run.phase {
            ArchiveCleanupPhaseV1::Settled | ArchiveCleanupPhaseV1::Refused => {}
            ArchiveCleanupPhaseV1::RecoveryRequired => {
                return Err(error_for_run(
                    &run,
                    run.safe_code
                        .unwrap_or(ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous),
                    false,
                ));
            }
            _ => return resume_run_with_repository_lock(store, sandbox_base, run),
        }
    }

    let (session, custody) = load_candidate(store, session_id)?;
    let origin = PathBuf::from(&custody.canonical_repo_dir);
    git_worktree::with_repository_mutation(&origin, || {
        let _root_guard = crate::store::sandbox_custody::lock_custody_root(custody.custody_id);
        let run_id = Uuid::new_v4();
        let proof = prove_original_candidate(store, sandbox_base, &session, &custody, run_id)
            .map_err(|error| classify_preintent_error(error, None))?;
        let run = store
            .blocking_lock()
            .insert_archive_cleanup_intent(&proof.intent)?;
        if run.run_id != run_id {
            return resume_run_locked(store, sandbox_base, run);
        }
        resume_run_locked(store, sandbox_base, run)
    })
}

fn resume_run_with_repository_lock(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    sandbox_base: &Path,
    run: ArchiveCleanupRun,
) -> Result<ArchiveCleanupReceiptV1> {
    let origin = PathBuf::from(&run.canonical_repo_dir);
    git_worktree::with_repository_mutation(&origin, || {
        let _root_guard = crate::store::sandbox_custody::lock_custody_root(run.custody_id);
        resume_run_locked(store, sandbox_base, run)
    })
}

fn resume_run_locked(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    sandbox_base: &Path,
    mut run: ArchiveCleanupRun,
) -> Result<ArchiveCleanupReceiptV1> {
    let origin_path = PathBuf::from(&run.canonical_repo_dir);
    let original_path = PathBuf::from(&run.original_root);
    let quarantine_path = PathBuf::from(&run.quarantine_root);
    let origin = origin_path.as_path();
    let original = original_path.as_path();
    let quarantine = quarantine_path.as_path();

    if run.phase == ArchiveCleanupPhaseV1::IntentCommitted {
        let (session, custody) = match load_candidate(store, run.session_id) {
            Ok(candidate) => candidate,
            Err(error) => {
                terminalize_run(
                    store,
                    &run,
                    ArchiveCleanupPhaseV1::RecoveryRequired,
                    ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                    "intent_candidate_unavailable",
                )?;
                return Err(classify_effect_error(error, &run));
            }
        };
        let original_proof =
            prove_original_candidate(store, sandbox_base, &session, &custody, run.run_id);
        if original_proof
            .as_ref()
            .is_ok_and(|proof| intent_matches_run(&proof.intent, &run))
        {
            let path = match git_worktree::prepare_settlement_quarantine_path(
                original,
                run.run_id,
                custody.allocation_id,
            ) {
                Ok(path) => path,
                Err(error) => {
                    terminalize_run(
                        store,
                        &run,
                        ArchiveCleanupPhaseV1::Refused,
                        ArchiveCleanupSafeCodeV1::QuarantineCollision,
                        "quarantine_prepare_refused",
                    )?;
                    return Err(classify_preintent_error(error, Some(&run)));
                }
            };
            if path.quarantine_root != quarantine {
                terminalize_run(
                    store,
                    &run,
                    ArchiveCleanupPhaseV1::RecoveryRequired,
                    ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                    "quarantine_path_mismatch",
                )?;
                return Err(error_for_run(
                    &run,
                    ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                    false,
                ));
            }
            if let Err(error) = git_worktree::move_worktree_to_quarantine_non_force_locked(
                origin,
                &path,
                &run.source_ref,
                &run.source_oid,
            ) {
                terminalize_run(
                    store,
                    &run,
                    ArchiveCleanupPhaseV1::RecoveryRequired,
                    ArchiveCleanupSafeCodeV1::WorktreeChanged,
                    "quarantine_move_ambiguous",
                )?;
                return Err(classify_effect_error(error, &run));
            }
            run = store
                .blocking_lock()
                .advance_archive_cleanup_phase(
                    run.run_id,
                    run.phase,
                    run.row_version,
                    ArchiveCleanupPhaseV1::Quarantined,
                    "quarantined",
                    None,
                )
                .map_err(|_| {
                    error_for_run(
                        &run,
                        ArchiveCleanupSafeCodeV1::DatabaseSettlementFailed,
                        true,
                    )
                })?;
        } else {
            if prove_quarantine_candidate(store, &run).is_ok() {
                run = store
                    .blocking_lock()
                    .advance_archive_cleanup_phase(
                        run.run_id,
                        run.phase,
                        run.row_version,
                        ArchiveCleanupPhaseV1::Quarantined,
                        "quarantine_move_ack_recovered",
                        None,
                    )
                    .map_err(|_| {
                        error_for_run(
                            &run,
                            ArchiveCleanupSafeCodeV1::DatabaseSettlementFailed,
                            true,
                        )
                    })?;
            } else {
                terminalize_run(
                    store,
                    &run,
                    ArchiveCleanupPhaseV1::RecoveryRequired,
                    ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                    "intent_state_ambiguous",
                )?;
                return Err(error_for_run(
                    &run,
                    ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                    false,
                ));
            }
        }
    }

    if run.phase == ArchiveCleanupPhaseV1::Quarantined {
        let marker = match prove_quarantine_candidate(store, &run) {
            Ok(marker) => marker,
            Err(error) => {
                terminalize_run(
                    store,
                    &run,
                    ArchiveCleanupPhaseV1::RecoveryRequired,
                    ArchiveCleanupSafeCodeV1::WorktreeChanged,
                    "quarantine_reproof_failed",
                )?;
                return Err(classify_effect_error(error, &run));
            }
        };
        let marker_json = canonical_marker(&marker).map_err(|error| {
            terminal_effect_error(
                store,
                &run,
                ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                "marker_encode_failed",
                error,
            )
        })?;
        let marker_digest = digest_field("archive-removal-authority-v1", &marker_json);
        run = store
            .blocking_lock()
            .advance_archive_cleanup_phase(
                run.run_id,
                run.phase,
                run.row_version,
                ArchiveCleanupPhaseV1::RemovalAuthorized,
                "removal_authorized",
                Some((&marker_json, &marker_digest)),
            )
            .map_err(|_| {
                error_for_run(
                    &run,
                    ArchiveCleanupSafeCodeV1::DatabaseSettlementFailed,
                    true,
                )
            })?;
    }

    if run.phase == ArchiveCleanupPhaseV1::RemovalAuthorized {
        prove_run_target(origin, &run).map_err(|error| {
            terminal_effect_error(
                store,
                &run,
                ArchiveCleanupSafeCodeV1::TargetUnavailable,
                "authorized_target_reproof_failed",
                error,
            )
        })?;
        let already_removed = post_removal_is_exact(
            origin,
            original,
            quarantine,
            &run.source_ref,
            &run.source_oid,
        )
        .map_err(|error| {
            terminal_effect_error(
                store,
                &run,
                ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                "authorized_postcondition_unreadable",
                error,
            )
        })?;
        if already_removed {
            run = store
                .blocking_lock()
                .advance_archive_cleanup_phase(
                    run.run_id,
                    run.phase,
                    run.row_version,
                    ArchiveCleanupPhaseV1::WorktreeRemoved,
                    "worktree_remove_ack_recovered",
                    None,
                )
                .map_err(|_| {
                    error_for_run(
                        &run,
                        ArchiveCleanupSafeCodeV1::DatabaseSettlementFailed,
                        true,
                    )
                })?;
        } else {
            let retained = parse_and_match_marker(&run).map_err(|error| {
                terminal_effect_error(
                    store,
                    &run,
                    ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                    "marker_replay_failed",
                    error,
                )
            })?;
            let current = match prove_quarantine_candidate(store, &run) {
                Ok(current) => current,
                Err(error) => {
                    terminalize_run(
                        store,
                        &run,
                        ArchiveCleanupPhaseV1::RecoveryRequired,
                        ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                        "removal_reproof_failed",
                    )?;
                    return Err(classify_effect_error(error, &run));
                }
            };
            if retained != current {
                terminalize_run(
                    store,
                    &run,
                    ArchiveCleanupPhaseV1::RecoveryRequired,
                    ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                    "marker_reproof_mismatch",
                )?;
                return Err(error_for_run(
                    &run,
                    ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                    false,
                ));
            }
            if let Err(error) = git_worktree::remove_worktree_non_force_locked(
                origin,
                original,
                quarantine,
                &run.source_ref,
                &run.source_oid,
            ) {
                let removed = post_removal_is_exact(
                    origin,
                    original,
                    quarantine,
                    &run.source_ref,
                    &run.source_oid,
                )
                .map_err(|post_error| {
                    terminal_effect_error(
                        store,
                        &run,
                        ArchiveCleanupSafeCodeV1::RemovalRefused,
                        "worktree_remove_postcondition_unreadable",
                        post_error,
                    )
                })?;
                if !removed {
                    return Err(terminal_effect_error(
                        store,
                        &run,
                        ArchiveCleanupSafeCodeV1::RemovalRefused,
                        "worktree_remove_ambiguous",
                        error,
                    ));
                }
            }
            let removed = post_removal_is_exact(
                origin,
                original,
                quarantine,
                &run.source_ref,
                &run.source_oid,
            )
            .map_err(|error| {
                terminal_effect_error(
                    store,
                    &run,
                    ArchiveCleanupSafeCodeV1::RemovalRefused,
                    "worktree_remove_postcondition_unreadable",
                    error,
                )
            })?;
            if !removed {
                terminalize_run(
                    store,
                    &run,
                    ArchiveCleanupPhaseV1::RecoveryRequired,
                    ArchiveCleanupSafeCodeV1::RemovalRefused,
                    "worktree_remove_postcondition_failed",
                )?;
                return Err(error_for_run(
                    &run,
                    ArchiveCleanupSafeCodeV1::RemovalRefused,
                    false,
                ));
            }
            prove_run_target(origin, &run).map_err(|error| {
                terminal_effect_error(
                    store,
                    &run,
                    ArchiveCleanupSafeCodeV1::TargetUnavailable,
                    "removed_target_reproof_failed",
                    error,
                )
            })?;
            run = store
                .blocking_lock()
                .advance_archive_cleanup_phase(
                    run.run_id,
                    run.phase,
                    run.row_version,
                    ArchiveCleanupPhaseV1::WorktreeRemoved,
                    "worktree_removed",
                    None,
                )
                .map_err(|_| {
                    error_for_run(
                        &run,
                        ArchiveCleanupSafeCodeV1::DatabaseSettlementFailed,
                        true,
                    )
                })?;
        }
    }

    if run.phase == ArchiveCleanupPhaseV1::WorktreeRemoved {
        validate_run_inventory(store, &run).map_err(|error| {
            terminal_effect_error(
                store,
                &run,
                ArchiveCleanupSafeCodeV1::SessionTopologyChanged,
                "removed_inventory_reproof_failed",
                error,
            )
        })?;
        prove_run_target(origin, &run).map_err(|error| {
            terminal_effect_error(
                store,
                &run,
                ArchiveCleanupSafeCodeV1::TargetUnavailable,
                "final_target_reproof_failed",
                error,
            )
        })?;
        let removed = post_removal_is_exact(
            origin,
            original,
            quarantine,
            &run.source_ref,
            &run.source_oid,
        )
        .map_err(|error| {
            terminal_effect_error(
                store,
                &run,
                ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                "final_postcondition_unreadable",
                error,
            )
        })?;
        if !removed {
            terminalize_run(
                store,
                &run,
                ArchiveCleanupPhaseV1::RecoveryRequired,
                ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                "removed_state_ambiguous",
            )?;
            return Err(error_for_run(
                &run,
                ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
                false,
            ));
        }
        return store
            .blocking_lock()
            .finalize_archive_cleanup(run.run_id, run.row_version)
            .map_err(|_| {
                error_for_run(
                    &run,
                    ArchiveCleanupSafeCodeV1::DatabaseSettlementFailed,
                    true,
                )
            });
    }

    Err(error_for_run(
        &run,
        run.safe_code
            .unwrap_or(ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous),
        false,
    ))
}

fn load_candidate(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    session_id: Uuid,
) -> Result<(Session, PersistedCustody)> {
    let store = store.blocking_lock();
    let session = store
        .get_session(session_id)?
        .ok_or(DaemonError::SessionNotFound(session_id))?;
    if !archive_cleanup_route_applies(&session) {
        return Err(archive_cleanup_error(
            ArchiveCleanupSafeCodeV1::SessionTopologyChanged,
            None,
            None,
            true,
        ));
    }
    let custody = store.live_custody_for_session(session_id).map_err(|_| {
        archive_cleanup_error(ArchiveCleanupSafeCodeV1::CustodyChanged, None, None, true)
    })?;
    Ok((session, custody))
}

fn prove_original_candidate(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    sandbox_base: &Path,
    session: &Session,
    custody: &PersistedCustody,
    run_id: Uuid,
) -> Result<ArchiveProof> {
    let inventory = candidate_inventory(store, session, custody)?;
    let origin = Path::new(&custody.canonical_repo_dir);
    let root = Path::new(&custody.sandbox_root);
    let canonical_base = std::fs::canonicalize(sandbox_base)
        .map_err(|_| DaemonError::Process("sandbox base is unavailable".into()))?;
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|_| DaemonError::Process("sandbox root is unavailable".into()))?;
    let allocation_name = custody.allocation_id.to_string();
    if canonical_root.parent() != Some(canonical_base.as_path())
        || canonical_root.file_name().and_then(|value| value.to_str())
            != Some(allocation_name.as_str())
    {
        return Err(DaemonError::Process(
            "sandbox root is not the exact private allocation path".into(),
        ));
    }
    let source_ref = format!("refs/heads/{}", custody.sandbox_branch);
    let source_oid = match git_worktree::observe_direct_ref_locked(origin, &source_ref)? {
        git_worktree::DirectRefObservation::Commit(oid) => oid,
        _ => {
            return Err(DaemonError::Process(
                "source branch is not an exact direct commit ref".into(),
            ));
        }
    };
    let admin = git_worktree::prove_registered_worktree_exact_locked(
        origin,
        root,
        &source_ref,
        &source_oid,
    )?;
    if admin.repository_identity.to_string_lossy() != custody.repository_identity {
        return Err(DaemonError::Process("repository identity changed".into()));
    }
    let observation = git_worktree::observe_worktree_locked(origin, root)?;
    let tree = git_worktree::prove_quarantine_tree_safe(root)?;
    let holder = super::reaper::prove_quarantine_has_no_untrusted_same_uid_holders(&tree)?;
    let (preservation_class, target_ref, target_oid) = preservation_class(
        origin,
        &custody.repository_identity,
        &custody.source_commit,
        &source_ref,
        &source_oid,
    )?;
    let quarantine =
        git_worktree::derive_settlement_quarantine_path(root, run_id, custody.allocation_id)?;
    let dependency_digest = digest_field(
        "archive-cleanup-dependencies-v1",
        &format!(
            "{}\0{}",
            inventory.scheduled_dependency_digest, inventory.session_path_dependency_digest
        ),
    );
    let session_kind = format!("{:?}", session.session_kind);
    let session_status = format!("{:?}", session.status);
    let session_updated_at = inventory
        .session_updated_at
        .clone()
        .ok_or_else(|| DaemonError::Process("archive Session timestamp is unavailable".into()))?;
    let topology_digest = archive_topology_digest(
        session.id,
        &session_kind,
        &session_status,
        &session_updated_at,
        session.parent_id,
        session.continued_from,
        session.title.as_deref(),
        session.agent_role.as_deref(),
        session.epic_spawn_ordinal,
    )?;
    let holder_digest = holder.evidence_digest().to_string();
    #[derive(Serialize)]
    struct Evidence<'a> {
        session_id: Uuid,
        custody_id: Uuid,
        custody_generation: u64,
        topology_digest: &'a str,
        source_ref: &'a str,
        source_oid: &'a str,
        target_ref: Option<&'a str>,
        target_oid: Option<&'a str>,
        clean_state_digest: &'a str,
        tree_digest: &'a str,
        dependency_digest: &'a str,
        holder_digest: &'a str,
    }
    let evidence_json = serde_json::to_string(&Evidence {
        session_id: session.id,
        custody_id: custody.custody_id,
        custody_generation: custody.generation,
        topology_digest: &topology_digest,
        source_ref: &source_ref,
        source_oid: &source_oid,
        target_ref: target_ref.as_deref(),
        target_oid: target_oid.as_deref(),
        clean_state_digest: &observation.clean_state_digest,
        tree_digest: tree.tree_digest(),
        dependency_digest: &dependency_digest,
        holder_digest: &holder_digest,
    })?;
    let evidence_digest = digest_field("archive-cleanup-evidence-v1", &evidence_json);
    let intent = NewArchiveCleanupIntent {
        run_id,
        session_id: session.id,
        custody_id: custody.custody_id,
        custody_generation: custody.generation,
        session_kind,
        session_status,
        session_updated_at,
        parent_id: session.parent_id,
        continued_from: session.continued_from,
        topology_digest,
        repository_identity: custody.repository_identity.clone(),
        canonical_repo_dir: custody.canonical_repo_dir.clone(),
        original_root: custody.sandbox_root.clone(),
        quarantine_root: quarantine.to_string_lossy().to_string(),
        root_device: admin.root_identity.device(),
        root_inode: admin.root_identity.inode(),
        git_common_dir: admin.repository_identity.to_string_lossy().to_string(),
        git_admin_dir: admin.admin_directory.to_string_lossy().to_string(),
        git_admin_id: admin.admin_id,
        source_ref,
        source_oid,
        preservation_class,
        target_ref,
        target_oid,
        clean_state_digest: observation.clean_state_digest,
        tree_digest: tree.tree_digest().to_string(),
        dependency_digest,
        holder_digest,
        evidence_digest,
    };
    Ok(ArchiveProof { intent })
}

fn candidate_inventory(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    session: &Session,
    custody: &PersistedCustody,
) -> Result<crate::store::cohort_settlement::SourceWorktreeInventoryRow> {
    let store = store.blocking_lock();
    if store.archive_cleanup_lineage_successor_count(session.id)? != 0 {
        return Err(DaemonError::Process(
            "archive cleanup Session has a later lineage successor".into(),
        ));
    }
    let rows = store.source_worktree_inventory(
        &custody.repository_identity,
        SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
    )?;
    if rows.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS as usize {
        return Err(DaemonError::Process(
            "archive cleanup repository inventory exceeded its bound".into(),
        ));
    }
    let session_status = format!("{:?}", session.status);
    let exact = rows.into_iter().find(|row| {
        let row_updated_at = row
            .session_updated_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&chrono::Utc));
        row.custody_id == custody.custody_id
            && row.owner_session_id == Some(session.id)
            && row.session_id == Some(session.id)
            && row.generation == custody.generation
            && row.validation_state == "verified"
            && row.validated_generation == custody.generation
            && row.reserved_effects == 0
            && row.active_effects == 0
            && row.participant_count == 1
            && row.scheduled_dependency_count == 0
            && row.scheduled_dependency_digest == SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST
            && row.session_path_dependency_count == 0
            && row.session_path_dependency_digest
                == SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST
            && row.status.as_deref() == Some(session_status.as_str())
            && row_updated_at == Some(session.updated_at)
            && row.canonical_repo_dir == custody.canonical_repo_dir
            && row.repository_identity == custody.repository_identity
            && row.sandbox_root == custody.sandbox_root
            && row.sandbox_branch == custody.sandbox_branch
            && row.session_working_dir.as_deref() == Some(custody.canonical_repo_dir.as_str())
            && row.session_sandbox_kind.as_deref() == Some("GitWorktree")
            && row.session_sandbox_root.as_deref() == Some(custody.sandbox_root.as_str())
            && row.session_sandbox_branch.as_deref() == Some(custody.sandbox_branch.as_str())
            && row.session_cleanup_state.as_deref() == Some("Live")
            && !row.pending_archive
    });
    exact.ok_or_else(|| {
        DaemonError::Process("archive cleanup ownership or dependency inventory changed".into())
    })
}

fn preservation_class(
    origin: &Path,
    repository_identity: &str,
    allocation_oid: &str,
    source_ref: &str,
    source_oid: &str,
) -> Result<(ArchivePreservationClassV1, Option<String>, Option<String>)> {
    if source_oid == allocation_oid {
        return Ok((ArchivePreservationClassV1::NoOutput, None, None));
    }
    let target = git_worktree::observe_repository_target_locked(origin)?;
    if target.repository_identity != repository_identity
        || target.target_ref == source_ref
        || !git_worktree::is_ancestor_locked(origin, source_oid, &target.target_oid)?
    {
        return Err(DaemonError::Process(
            "source output is not an ancestor of the current local target".into(),
        ));
    }
    Ok((
        ArchivePreservationClassV1::IntegratedAncestor,
        Some(target.target_ref),
        Some(target.target_oid),
    ))
}

fn intent_matches_run(intent: &NewArchiveCleanupIntent, run: &ArchiveCleanupRun) -> bool {
    intent.run_id == run.run_id
        && intent.session_id == run.session_id
        && intent.custody_id == run.custody_id
        && intent.custody_generation == run.custody_generation
        && intent.session_kind == run.session_kind
        && intent.session_status == run.session_status
        && intent.session_updated_at == run.session_updated_at
        && intent.parent_id == run.parent_id
        && intent.continued_from == run.continued_from
        && intent.topology_digest == run.topology_digest
        && intent.repository_identity == run.repository_identity
        && intent.canonical_repo_dir == run.canonical_repo_dir
        && intent.original_root == run.original_root
        && intent.quarantine_root == run.quarantine_root
        && intent.root_device == run.root_device
        && intent.root_inode == run.root_inode
        && digest_field("archive-git-common-directory-v1", &intent.git_common_dir)
            == run.git_common_dir_digest
        && digest_field("archive-git-admin-directory-v1", &intent.git_admin_dir)
            == run.git_admin_dir_digest
        && intent.git_admin_id == run.git_admin_id
        && intent.source_ref == run.source_ref
        && intent.source_oid == run.source_oid
        && intent.preservation_class == run.preservation_class
        && intent.target_ref == run.target_ref
        && intent.target_oid == run.target_oid
        && intent.clean_state_digest == run.clean_state_digest
        && intent.tree_digest == run.tree_digest
        && intent.dependency_digest == run.dependency_digest
        && intent.holder_digest == run.holder_digest
        && intent.evidence_digest == run.evidence_digest
}

fn prove_quarantine_candidate(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run: &ArchiveCleanupRun,
) -> Result<ArchiveRemovalAuthorityV1> {
    let origin = Path::new(&run.canonical_repo_dir);
    let original = Path::new(&run.original_root);
    let quarantine = Path::new(&run.quarantine_root);
    validate_run_inventory(store, run)?;
    let admin = git_worktree::prove_moved_worktree_exact_locked(
        origin,
        original,
        quarantine,
        &run.source_ref,
        &run.source_oid,
    )?;
    if admin.root_identity.device() != run.root_device
        || admin.root_identity.inode() != run.root_inode
        || digest_field(
            "archive-git-common-directory-v1",
            &admin.repository_identity.to_string_lossy(),
        ) != run.git_common_dir_digest
        || digest_field(
            "archive-git-admin-directory-v1",
            &admin.admin_directory.to_string_lossy(),
        ) != run.git_admin_dir_digest
        || admin.admin_id != run.git_admin_id
    {
        return Err(DaemonError::Process(
            "quarantine Git administrative identity changed".into(),
        ));
    }
    let observation = git_worktree::observe_worktree_locked(origin, quarantine)?;
    if !observation.clean || observation.clean_state_digest != run.clean_state_digest {
        return Err(DaemonError::Process(
            "quarantine clean-state proof changed".into(),
        ));
    }
    prove_run_target(origin, run)?;
    let tree = git_worktree::prove_quarantine_tree_safe(quarantine)?;
    let holder = super::reaper::prove_quarantine_has_no_untrusted_same_uid_holders(&tree)?;
    if holder.evidence_digest() != run.holder_digest {
        return Err(DaemonError::Process(
            "quarantine holder inventory changed".into(),
        ));
    }
    let dependency_digest = run_dependency_digest(store, run)?;
    if dependency_digest != run.dependency_digest {
        return Err(DaemonError::Process(
            "quarantine dependency proof changed".into(),
        ));
    }
    Ok(ArchiveRemovalAuthorityV1 {
        version: 1,
        run_id: run.run_id,
        session_id: run.session_id,
        custody_id: run.custody_id,
        custody_generation: run.custody_generation,
        preservation_class: run.preservation_class,
        source_ref: run.source_ref.clone(),
        source_oid: run.source_oid.clone(),
        target_ref: run.target_ref.clone(),
        target_oid: run.target_oid.clone(),
        original_root_digest: digest_field("archive-original-root-v1", &run.original_root),
        quarantine_root_digest: digest_field("archive-quarantine-root-v1", &run.quarantine_root),
        repository_identity_digest: digest_field(
            "archive-repository-identity-v1",
            &run.repository_identity,
        ),
        git_admin_dir_digest: digest_field(
            "archive-git-admin-directory-v1",
            &admin.admin_directory.to_string_lossy(),
        ),
        git_admin_id: admin.admin_id,
        clean_state_digest: observation.clean_state_digest,
        stable_tree_digest: tree.tree_digest().to_string(),
        dependency_digest,
        holder_digest: holder.evidence_digest().to_string(),
        journal_evidence_digest: run.evidence_digest.clone(),
        branch_preserved: true,
    })
}

fn validate_run_inventory(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run: &ArchiveCleanupRun,
) -> Result<()> {
    let (session, custody) = load_candidate(store, run.session_id)?;
    let inventory = candidate_inventory(store, &session, &custody)?;
    if custody.custody_id != run.custody_id
        || custody.generation != run.custody_generation
        || custody.repository_identity != run.repository_identity
        || custody.canonical_repo_dir != run.canonical_repo_dir
        || custody.sandbox_root != run.original_root
        || format!("refs/heads/{}", custody.sandbox_branch) != run.source_ref
        || inventory.session_updated_at.as_deref() != Some(run.session_updated_at.as_str())
    {
        return Err(DaemonError::Process(
            "archive cleanup journal custody changed".into(),
        ));
    }
    let topology = archive_topology_digest(
        session.id,
        &format!("{:?}", session.session_kind),
        &format!("{:?}", session.status),
        &run.session_updated_at,
        session.parent_id,
        session.continued_from,
        session.title.as_deref(),
        session.agent_role.as_deref(),
        session.epic_spawn_ordinal,
    )?;
    if topology != run.topology_digest {
        return Err(DaemonError::Process(
            "archive cleanup Session topology changed".into(),
        ));
    }
    Ok(())
}

fn run_dependency_digest(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run: &ArchiveCleanupRun,
) -> Result<String> {
    let store = store.blocking_lock();
    let rows = store.source_worktree_inventory(
        &run.repository_identity,
        SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
    )?;
    let row = rows
        .iter()
        .find(|row| row.custody_id == run.custody_id)
        .ok_or_else(|| DaemonError::Process("archive cleanup inventory disappeared".into()))?;
    Ok(digest_field(
        "archive-cleanup-dependencies-v1",
        &format!(
            "{}\0{}",
            row.scheduled_dependency_digest, row.session_path_dependency_digest
        ),
    ))
}

fn prove_run_target(origin: &Path, run: &ArchiveCleanupRun) -> Result<()> {
    if git_worktree::observe_direct_ref_locked(origin, &run.source_ref)?
        != git_worktree::DirectRefObservation::Commit(run.source_oid.clone())
    {
        return Err(DaemonError::Process("source branch changed".into()));
    }
    match run.preservation_class {
        ArchivePreservationClassV1::NoOutput => {
            if run.target_ref.is_some() || run.target_oid.is_some() {
                return Err(DaemonError::Process(
                    "no-output journal carried a target".into(),
                ));
            }
        }
        ArchivePreservationClassV1::IntegratedAncestor => {
            let target = git_worktree::observe_repository_target_locked(origin)?;
            if Some(target.target_ref.as_str()) != run.target_ref.as_deref()
                || Some(target.target_oid.as_str()) != run.target_oid.as_deref()
                || target.repository_identity != run.repository_identity
                || !git_worktree::is_ancestor_locked(origin, &run.source_oid, &target.target_oid)?
            {
                return Err(DaemonError::Process(
                    "archive cleanup target changed".into(),
                ));
            }
        }
    }
    Ok(())
}

fn post_removal_is_exact(
    origin: &Path,
    original: &Path,
    quarantine: &Path,
    source_ref: &str,
    source_oid: &str,
) -> Result<bool> {
    let mut paths_absent = true;
    for path in [original, quarantine] {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => paths_absent = false,
            Err(_) => {
                return Err(DaemonError::Process(
                    "archive cleanup path postcondition is unreadable".into(),
                ));
            }
        }
    }
    Ok(paths_absent
        && !git_worktree::observe_worktree_locked(origin, original)?.registered
        && !git_worktree::observe_worktree_locked(origin, quarantine)?.registered
        && git_worktree::source_ref_has_zero_registrations_locked(origin, source_ref)?
        && git_worktree::observe_direct_ref_locked(origin, source_ref)?
            == git_worktree::DirectRefObservation::Commit(source_oid.to_string()))
}

fn parse_and_match_marker(run: &ArchiveCleanupRun) -> Result<ArchiveRemovalAuthorityV1> {
    let raw = run.removal_authority_json.as_deref().ok_or_else(|| {
        DaemonError::Process("archive removal authority marker is missing".into())
    })?;
    let marker: ArchiveRemovalAuthorityV1 = serde_json::from_str(raw).map_err(|_| {
        DaemonError::Process("archive removal authority marker is malformed".into())
    })?;
    let canonical = canonical_marker(&marker)?;
    let digest = digest_field("archive-removal-authority-v1", &canonical);
    if canonical != raw
        || run.removal_authority_digest.as_deref() != Some(digest.as_str())
        || marker.run_id != run.run_id
        || marker.session_id != run.session_id
        || marker.custody_id != run.custody_id
        || marker.custody_generation != run.custody_generation
        || !marker.branch_preserved
    {
        return Err(DaemonError::Process(
            "archive removal authority marker does not match its journal".into(),
        ));
    }
    Ok(marker)
}

fn canonical_marker(marker: &ArchiveRemovalAuthorityV1) -> Result<String> {
    serde_json::to_string(marker).map_err(Into::into)
}

fn terminalize_run(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run: &ArchiveCleanupRun,
    phase: ArchiveCleanupPhaseV1,
    code: ArchiveCleanupSafeCodeV1,
    detail: &str,
) -> Result<()> {
    store.blocking_lock().terminate_archive_cleanup_run(
        run.run_id,
        run.phase,
        run.row_version,
        phase,
        code,
        Some(detail),
    )
}

fn classify_preintent_error(error: DaemonError, run: Option<&ArchiveCleanupRun>) -> DaemonError {
    let rendered = error.to_string();
    let text = match &error {
        DaemonError::Process(detail) => detail.as_str(),
        _ => rendered.as_str(),
    };
    let code = if text.contains("ancestor") || text.contains("output") {
        ArchiveCleanupSafeCodeV1::OutputNotIntegrated
    } else if text.contains("topology") || text.contains("lineage") {
        ArchiveCleanupSafeCodeV1::SessionTopologyChanged
    } else if preintent_message_has_word(text, "clean")
        || preintent_message_has_word(text, "cleanliness")
        || preintent_message_has_word(text, "dirty")
        || preintent_message_has_word(text, "index")
    {
        ArchiveCleanupSafeCodeV1::WorktreeDirty
    } else if text.contains("holder") || text.contains("process") {
        ArchiveCleanupSafeCodeV1::ProcessHolderPresent
    } else if text.contains("dependency") {
        ArchiveCleanupSafeCodeV1::DependencyPresent
    } else if text.contains("target") {
        ArchiveCleanupSafeCodeV1::TargetUnavailable
    } else if text.contains("source branch") {
        ArchiveCleanupSafeCodeV1::SourceRefChanged
    } else if text.contains("sandbox") || text.contains("custody") {
        ArchiveCleanupSafeCodeV1::CustodyChanged
    } else {
        ArchiveCleanupSafeCodeV1::ProofUnavailable
    };
    match run {
        Some(run) => error_for_run(run, code, true),
        None => archive_cleanup_error(code, None, None, true),
    }
}

fn preintent_message_has_word(text: &str, expected: &str) -> bool {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|word| word == expected)
}

fn classify_effect_error(_error: DaemonError, run: &ArchiveCleanupRun) -> DaemonError {
    error_for_run(
        run,
        ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous,
        false,
    )
}

fn terminal_effect_error(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run: &ArchiveCleanupRun,
    code: ArchiveCleanupSafeCodeV1,
    detail: &str,
    _error: DaemonError,
) -> DaemonError {
    let _ = terminalize_run(
        store,
        run,
        ArchiveCleanupPhaseV1::RecoveryRequired,
        code,
        detail,
    );
    error_for_run(run, code, false)
}

fn error_for_run(
    run: &ArchiveCleanupRun,
    code: ArchiveCleanupSafeCodeV1,
    retryable: bool,
) -> DaemonError {
    archive_cleanup_error(code, Some(run.run_id), Some(run.phase), retryable)
}

fn archive_cleanup_error(
    code: ArchiveCleanupSafeCodeV1,
    run_id: Option<Uuid>,
    phase: Option<ArchiveCleanupPhaseV1>,
    retryable: bool,
) -> DaemonError {
    let next_action = if retryable {
        "Stop competing maintenance, resolve the reported condition, then retry archive."
    } else {
        "Preserve the repository and quarantine evidence; use a compatible recovery binary."
    };
    let data = ArchiveCleanupErrorV1 {
        version: ARCHIVE_CLEANUP_SCHEMA_VERSION,
        safe_code: code,
        run_id,
        phase,
        retryable,
        next_action: next_action.into(),
    };
    DaemonError::StructuredRpc {
        rpc_code: ARCHIVE_CLEANUP_RPC_ERROR,
        message: format!("archive_cleanup_{}", code.as_str()),
        data: serde_json::to_value(data).unwrap_or(serde_json::Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::memory::embedding::EmbeddingProviderResult;
    use crate::memory::store::MemoryStore;
    use crate::memory::sync::MemorySyncEngine;
    use crate::memory::types::MemoryConfig;
    use crate::memory::worker::{
        MemoryCommand, MemoryHandle, MemoryProjectionSyncCompletion,
        MemoryProjectionSyncDisposition, MemoryProjectionSyncFaultPoint, MemoryWorker,
        install_memory_projection_sync_failure, install_memory_projection_sync_pause,
    };
    use crate::sandbox::SandboxAllocator;
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
    use rsi_common::types::SessionKind;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("run fixture Git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout)
            .expect("Git output is UTF-8")
            .trim()
            .to_string()
    }

    struct CleanupFixture {
        _temp: tempfile::TempDir,
        store: Arc<tokio::sync::Mutex<crate::store::Store>>,
        sandbox_base: PathBuf,
        database: PathBuf,
        repository: PathBuf,
        root: PathBuf,
        session_id: Uuid,
        source_ref: String,
        allocation_oid: String,
    }

    struct ProjectionMemoryWorker {
        handle: MemoryHandle,
        bus: Arc<EventBus>,
        task: tokio::task::JoinHandle<()>,
    }

    impl ProjectionMemoryWorker {
        async fn abort_when_reached(self, pause: crate::memory::worker::MemoryProjectionSyncPause) {
            pause.wait_until_reached().await;
            self.task.abort();
            let error = self
                .task
                .await
                .expect_err("memory worker stops at injected crash boundary");
            assert!(error.is_cancelled());
        }

        async fn shutdown(self) {
            self.handle.shutdown().await.expect("stop memory worker");
            self.task.await.expect("join memory worker");
        }
    }

    impl CleanupFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("fixture root");
            let repository = temp.path().join("repository");
            let sandbox_base = temp.path().join("sandboxes");
            std::fs::create_dir(&repository).expect("repository directory");
            std::fs::create_dir(&sandbox_base).expect("sandbox directory");
            git(&repository, &["init", "-q", "-b", "main"]);
            git(
                &repository,
                &["config", "user.email", "archive@example.test"],
            );
            git(&repository, &["config", "user.name", "Archive Fixture"]);
            std::fs::write(repository.join("tracked"), "base\n").expect("tracked file");
            git(&repository, &["add", "tracked"]);
            git(&repository, &["commit", "-qm", "base"]);
            let allocation_oid = git(&repository, &["rev-parse", "HEAD"]);
            let allocation_id = Uuid::new_v4();
            let allocator = SandboxAllocator::new(sandbox_base.clone());
            let allocation = allocator
                .allocate(
                    allocation_id,
                    &repository,
                    SandboxKind::GitWorktree,
                    &allocation_oid,
                    None,
                )
                .expect("allocate isolated worktree");
            let branch = allocation.branch.clone().expect("sandbox branch");
            let source_ref = format!("refs/heads/{branch}");
            let common_dir = git(
                &repository,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            );
            let common_dir = std::fs::canonicalize(common_dir).expect("canonical common dir");

            let session_id = Uuid::new_v4();
            let mut session = crate::store::tests::make_test_session();
            session.id = session_id;
            session.project_id = None;
            session.session_kind = SessionKind::Task;
            session.status = SessionStatus::Completed;
            session.title = Some("raw operator title".into());
            session.agent_role = Some("implementer".into());
            session.epic_spawn_ordinal = Some(7);
            session.working_dir = repository.clone();
            session.git_branch = Some("main".into());
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(allocation.root.clone());
            session.sandbox_branch = Some(branch.clone());
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            session.updated_at = chrono::Utc::now();

            let database = temp.path().join("archive.db");
            let mut store = crate::store::Store::open(&database).expect("store");
            store
                .insert_session_with_custody(
                    &session,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id: Uuid::new_v4(),
                        canonical_repo_dir: repository.display().to_string(),
                        sandbox_root: allocation.root.display().to_string(),
                        sandbox_branch: branch,
                        repository_identity: common_dir.display().to_string(),
                        source_commit: allocation_oid.clone(),
                        cause: CustodyCause::FreshLaunch,
                    }),
                )
                .expect("insert live custody session");
            Self {
                _temp: temp,
                store: Arc::new(tokio::sync::Mutex::new(store)),
                sandbox_base,
                database,
                repository,
                root: allocation.root,
                session_id,
                source_ref,
                allocation_oid,
            }
        }

        fn commit_and_integrate_output(&self) -> String {
            std::fs::write(self.root.join("tracked"), "integrated output\n").expect("write output");
            git(&self.root, &["add", "tracked"]);
            git(&self.root, &["commit", "-qm", "integrated output"]);
            let source_oid = git(&self.root, &["rev-parse", "HEAD"]);
            let branch = self.source_ref.trim_start_matches("refs/heads/");
            git(&self.repository, &["merge", "--ff-only", branch]);
            source_oid
        }

        fn commit_unique_output(&self) -> String {
            std::fs::write(self.root.join("tracked"), "unique output\n").expect("write output");
            git(&self.root, &["add", "tracked"]);
            git(&self.root, &["commit", "-qm", "unique output"]);
            git(&self.root, &["rev-parse", "HEAD"])
        }

        fn with_process_fixture<T>(&self, body: impl FnOnce() -> T) -> T {
            let proc = super::super::reaper::SyntheticQuarantineHolderProc::new();
            super::super::reaper::with_quarantine_holder_test_proc(
                proc.proc_root(),
                proc.uid(),
                body,
            )
        }

        fn cleanup_result(&self) -> Result<ArchiveCleanupReceiptV1> {
            self.with_process_fixture(|| {
                resume_or_start_cleanup_blocking(&self.store, &self.sandbox_base, self.session_id)
            })
        }

        fn settle(&self) -> ArchiveCleanupReceiptV1 {
            self.cleanup_result().expect("cleanup settles")
        }

        fn projection_manager(
            &self,
            event_bus: Arc<EventBus>,
            memory_handle: Option<MemoryHandle>,
        ) -> SessionManager {
            SessionManager::new(
                event_bus,
                crate::store::Store::open(&self.database).expect("reopen projection store"),
                false,
                self._temp
                    .path()
                    .join(format!("daemon-{}.sock", Uuid::new_v4())),
                memory_handle,
                Vec::new(),
                RuntimeConfig::from_config(&Config::from_env()),
                self.sandbox_base.clone(),
            )
            .expect("projection manager")
        }

        fn projection_id(&self) -> Uuid {
            self.store
                .blocking_lock()
                .conn
                .query_row(
                    "SELECT projection_id FROM archive_cleanup_success_projections WHERE session_id=?1",
                    [self.session_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .map(|value| Uuid::parse_str(&value).expect("projection UUID"))
                .expect("projection row")
        }

        fn settle_projection_batch(&self, count: usize) -> Vec<Uuid> {
            assert!(count > 0);
            let first = self.settle();
            let mut projection_ids = vec![crate::store::archive_cleanup::archive_projection_id(
                first.run_id,
            )];
            for ordinal in 1..count {
                let allocation_id = Uuid::new_v4();
                let allocation = SandboxAllocator::new(self.sandbox_base.clone())
                    .allocate(
                        allocation_id,
                        &self.repository,
                        SandboxKind::GitWorktree,
                        &self.allocation_oid,
                        None,
                    )
                    .expect("allocate batch projection worktree");
                let branch = allocation.branch.clone().expect("batch projection branch");
                let common_dir = git(
                    &self.repository,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                );
                let common_dir =
                    std::fs::canonicalize(common_dir).expect("canonical batch common dir");
                let session_id = Uuid::new_v4();
                let mut session = crate::store::tests::make_test_session();
                session.id = session_id;
                session.project_id = None;
                session.session_kind = SessionKind::Task;
                session.status = SessionStatus::Completed;
                session.title = Some(format!("batch projection {ordinal}"));
                session.working_dir = self.repository.clone();
                session.git_branch = Some("main".into());
                session.sandbox_kind = Some(SandboxKind::GitWorktree);
                session.sandbox_root = Some(allocation.root.clone());
                session.sandbox_branch = Some(branch.clone());
                session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
                session.updated_at = chrono::Utc::now();
                self.store
                    .blocking_lock()
                    .insert_session_with_custody(
                        &session,
                        SessionCustodyBinding::New(NewCustodyRoot {
                            custody_id: Uuid::new_v4(),
                            canonical_repo_dir: self.repository.display().to_string(),
                            sandbox_root: allocation.root.display().to_string(),
                            sandbox_branch: branch,
                            repository_identity: common_dir.display().to_string(),
                            source_commit: self.allocation_oid.clone(),
                            cause: CustodyCause::FreshLaunch,
                        }),
                    )
                    .expect("insert batch projection Session");
                let receipt = self
                    .with_process_fixture(|| {
                        resume_or_start_cleanup_blocking(
                            &self.store,
                            &self.sandbox_base,
                            session_id,
                        )
                    })
                    .expect("settle batch projection cleanup");
                projection_ids.push(crate::store::archive_cleanup::archive_projection_id(
                    receipt.run_id,
                ));
            }
            projection_ids
        }

        async fn spawn_projection_memory_worker(
            &self,
            memory_dir: &Path,
            memory_db: &Path,
        ) -> ProjectionMemoryWorker {
            std::fs::create_dir_all(memory_dir).expect("memory fixture directory");
            let memory_store = MemoryStore::open(memory_db).expect("memory fixture store");
            let provider = Arc::new(EmbeddingProviderResult {
                provider: None,
                requested: "none".into(),
                provider_label: "none".into(),
                backend: "none".into(),
                base_url: None,
                fallback_reason: None,
                unavailable_reason: None,
            });
            let bus = Arc::new(EventBus::new(128));
            let sync_engine = MemorySyncEngine::new(
                MemoryConfig {
                    memory_dir: memory_dir.to_path_buf(),
                    db_path: memory_db.to_path_buf(),
                    watch_enabled: false,
                    ..Default::default()
                },
                memory_store,
                self.database.clone(),
                provider,
                Arc::clone(&bus),
                memory_dir.to_path_buf(),
                memory_db.to_path_buf(),
            );
            let main_store = Arc::new(tokio::sync::Mutex::new(
                crate::store::Store::open(&self.database).expect("memory main store"),
            ));
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let handle = MemoryHandle::new(tx);
            let mut worker = MemoryWorker::new(
                rx,
                sync_engine,
                main_store,
                None,
                Arc::clone(&bus),
                RuntimeConfig::from_config(&Config::from_env()),
            );
            let task = tokio::spawn(async move { worker.run().await });
            handle.status().await.expect("memory worker startup");
            ProjectionMemoryWorker { handle, bus, task }
        }

        fn move_without_journal_ack(&self) -> ArchiveCleanupRun {
            self.with_process_fixture(|| {
                git_worktree::with_repository_mutation(&self.repository, || {
                    let (session, custody) = load_candidate(&self.store, self.session_id)?;
                    let _root_guard =
                        crate::store::sandbox_custody::lock_custody_root(custody.custody_id);
                    let proof = prove_original_candidate(
                        &self.store,
                        &self.sandbox_base,
                        &session,
                        &custody,
                        Uuid::new_v4(),
                    )?;
                    let run = self
                        .store
                        .blocking_lock()
                        .insert_archive_cleanup_intent(&proof.intent)?;
                    let path = git_worktree::prepare_settlement_quarantine_path(
                        &self.root,
                        run.run_id,
                        custody.allocation_id,
                    )?;
                    git_worktree::move_worktree_to_quarantine_non_force_locked(
                        &self.repository,
                        &path,
                        &run.source_ref,
                        &run.source_oid,
                    )?;
                    Ok::<_, DaemonError>(run)
                })
                .expect("move fixture without journal acknowledgement")
            })
        }

        fn authorize_removal(
            &self,
            alter_marker: impl FnOnce(&mut ArchiveRemovalAuthorityV1),
        ) -> ArchiveCleanupRun {
            self.with_process_fixture(|| {
                git_worktree::with_repository_mutation(&self.repository, || {
                    let (session, custody) = load_candidate(&self.store, self.session_id)?;
                    let _root_guard =
                        crate::store::sandbox_custody::lock_custody_root(custody.custody_id);
                    let proof = prove_original_candidate(
                        &self.store,
                        &self.sandbox_base,
                        &session,
                        &custody,
                        Uuid::new_v4(),
                    )?;
                    let mut run = self
                        .store
                        .blocking_lock()
                        .insert_archive_cleanup_intent(&proof.intent)?;
                    let path = git_worktree::prepare_settlement_quarantine_path(
                        &self.root,
                        run.run_id,
                        custody.allocation_id,
                    )?;
                    git_worktree::move_worktree_to_quarantine_non_force_locked(
                        &self.repository,
                        &path,
                        &run.source_ref,
                        &run.source_oid,
                    )?;
                    run = self.store.blocking_lock().advance_archive_cleanup_phase(
                        run.run_id,
                        run.phase,
                        run.row_version,
                        ArchiveCleanupPhaseV1::Quarantined,
                        "test_quarantined",
                        None,
                    )?;
                    let mut marker = prove_quarantine_candidate(&self.store, &run)?;
                    alter_marker(&mut marker);
                    let marker_json = canonical_marker(&marker)?;
                    let marker_digest = digest_field("archive-removal-authority-v1", &marker_json);
                    self.store.blocking_lock().advance_archive_cleanup_phase(
                        run.run_id,
                        run.phase,
                        run.row_version,
                        ArchiveCleanupPhaseV1::RemovalAuthorized,
                        "test_removal_authorized",
                        Some((&marker_json, &marker_digest)),
                    )
                })
                .expect("authorize fixture removal")
            })
        }

        fn remove_without_journal_ack(&self) -> ArchiveCleanupRun {
            let run = self.authorize_removal(|_| {});
            git_worktree::with_repository_mutation(&self.repository, || {
                let _root_guard = crate::store::sandbox_custody::lock_custody_root(run.custody_id);
                git_worktree::remove_worktree_non_force_locked(
                    &self.repository,
                    &self.root,
                    Path::new(&run.quarantine_root),
                    &run.source_ref,
                    &run.source_oid,
                )
            })
            .expect("remove fixture without journal acknowledgement");
            run
        }

        fn assert_retained(&self, oid: &str) {
            assert!(self.root.exists(), "refusal must retain the worktree");
            assert_eq!(git(&self.repository, &["rev-parse", &self.source_ref]), oid);
            let store = self.store.blocking_lock();
            let session = store
                .get_session(self.session_id)
                .expect("read retained session")
                .expect("retained session exists");
            assert_eq!(session.status, SessionStatus::Completed);
            assert_eq!(
                session.sandbox_cleanup_state,
                Some(SandboxCleanupState::Live)
            );
        }

        fn assert_settled(&self, receipt: &ArchiveCleanupReceiptV1, oid: &str) {
            receipt.validate_wire().expect("receipt wire contract");
            assert_eq!(receipt.session_id, self.session_id);
            assert_eq!(receipt.source_branch, self.source_ref);
            assert_eq!(receipt.source_oid.as_str(), oid);
            assert!(!self.root.exists());
            assert_eq!(git(&self.repository, &["rev-parse", &self.source_ref]), oid);
            let store = self.store.blocking_lock();
            let session = store
                .get_session(self.session_id)
                .expect("read session")
                .expect("session remains");
            assert_eq!(session.status, SessionStatus::Archived);
            assert_eq!(
                session.sandbox_cleanup_state,
                Some(SandboxCleanupState::Purged)
            );
            assert_eq!(session.sandbox_root, None);
            assert_eq!(session.sandbox_branch, None);
            assert_eq!(session.title.as_deref(), Some("raw operator title"));
            assert_eq!(session.agent_role.as_deref(), Some("implementer"));
            assert_eq!(session.epic_spawn_ordinal, Some(7));
            store
                .verify_archive_cleanup_unarchive_gate(self.session_id)
                .expect("settled receipt admits fresh-custody unarchive");
            let status = store
                .archive_cleanup_status(self.session_id)
                .expect("read settled cleanup status");
            status.validate_wire().expect("status wire contract");
            assert_eq!(status.receipt.as_ref(), Some(receipt));
        }
    }

    async fn projection_id_for(manager: &SessionManager, session_id: Uuid) -> Uuid {
        manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT projection_id FROM archive_cleanup_success_projections WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map(|value| Uuid::parse_str(&value).expect("projection UUID"))
            .expect("projection row")
    }

    async fn projection_consumer_state(
        manager: &SessionManager,
        projection_id: Uuid,
        consumer: &str,
    ) -> String {
        manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT delivery_state FROM archive_cleanup_projection_consumers
                 WHERE projection_id=?1 AND consumer_kind=?2",
                rusqlite::params![projection_id.to_string(), consumer],
                |row| row.get(0),
            )
            .expect("consumer state")
    }

    async fn delivered_consumer_count(manager: &SessionManager, projection_id: Uuid) -> i64 {
        manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT count(*) FROM archive_cleanup_projection_consumers
                 WHERE projection_id=?1 AND delivery_state='delivered'",
                [projection_id.to_string()],
                |row| row.get(0),
            )
            .expect("delivered consumer count")
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ProjectionAttemptSnapshot {
        projection_id: Uuid,
        projection_state: String,
        projection_attempt_count: i64,
        consumers: Vec<(String, String, i64)>,
    }

    async fn projection_attempt_snapshots(
        manager: &SessionManager,
        projection_ids: &[Uuid],
    ) -> Vec<ProjectionAttemptSnapshot> {
        let store = manager.store.lock().await;
        let mut snapshots = Vec::with_capacity(projection_ids.len());
        for projection_id in projection_ids {
            let (projection_state, projection_attempt_count) = store
                .conn
                .query_row(
                    "SELECT delivery_state,attempt_count
                     FROM archive_cleanup_success_projections WHERE projection_id=?1",
                    [projection_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("projection attempt state");
            let mut statement = store
                .conn
                .prepare(
                    "SELECT consumer_kind,delivery_state,attempt_count
                     FROM archive_cleanup_projection_consumers
                     WHERE projection_id=?1 ORDER BY consumer_kind",
                )
                .expect("prepare projection consumer snapshot");
            let consumers = statement
                .query_map([projection_id.to_string()], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .expect("query projection consumer snapshot")
                .collect::<std::result::Result<Vec<_>, _>>()
                .expect("collect projection consumer snapshot");
            snapshots.push(ProjectionAttemptSnapshot {
                projection_id: *projection_id,
                projection_state,
                projection_attempt_count,
                consumers,
            });
        }
        snapshots
    }

    fn assert_memory_only_retryable(snapshot: &ProjectionAttemptSnapshot) {
        assert_eq!(snapshot.projection_state, "delivering");
        assert_eq!(snapshot.projection_attempt_count, 3);
        assert_eq!(
            snapshot.consumers,
            vec![
                ("bus".into(), "delivered".into(), 1),
                ("memory".into(), "delivering".into(), 1),
                ("watch".into(), "delivered".into(), 1),
            ]
        );
    }

    fn assert_disabled_memory_projection_batch_is_bounded(projection_count: usize) {
        let fixture = CleanupFixture::new();
        let projection_ids = fixture.settle_projection_batch(projection_count);
        let expected_ids = projection_ids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(expected_ids.len(), projection_count);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("projection batch runtime");
        runtime.block_on(async {
            let event_bus = Arc::new(EventBus::new(projection_count.saturating_mul(2)));
            let mut events = event_bus.subscribe();
            let manager = fixture.projection_manager(Arc::clone(&event_bus), None);
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                manager.recover_archive_cleanups(),
            )
            .await
            .expect("disabled-memory projection recovery must return")
            .expect("disabled-memory projection recovery succeeds");

            let mut delivered_bus_ids = std::collections::BTreeSet::new();
            for _ in 0..projection_count {
                let event = events.try_recv().expect("one bus event per projection");
                let DaemonEvent::SessionArchived {
                    projection_id: Some(projection_id),
                    ..
                } = event.as_ref()
                else {
                    panic!("unexpected archive projection event: {event:?}");
                };
                assert!(
                    delivered_bus_ids.insert(*projection_id),
                    "bus projection ID must not be duplicated"
                );
            }
            assert_eq!(delivered_bus_ids, expected_ids);
            assert!(events.try_recv().is_err(), "no extra bus projection event");

            let bounded = projection_attempt_snapshots(&manager, &projection_ids).await;
            assert_eq!(bounded.len(), projection_count);
            for snapshot in &bounded {
                assert_memory_only_retryable(snapshot);
            }
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                manager.recover_archive_cleanups(),
            )
            .await
            .expect("second disabled-memory recovery must return")
            .expect("second disabled-memory recovery succeeds");
            assert_eq!(
                projection_attempt_snapshots(&manager, &projection_ids).await,
                bounded,
                "memory-only projections are not reclaimed within the bounded pass"
            );
            assert!(
                events.try_recv().is_err(),
                "bounded replay emits no duplicate bus effect"
            );
            event_bus.unsubscribe();

            let memory_dir = fixture
                ._temp
                .path()
                .join(format!("memory-batch-{projection_count}"));
            let memory_db = fixture
                ._temp
                .path()
                .join(format!("memory-batch-{projection_count}.sqlite"));
            let worker = fixture
                .spawn_projection_memory_worker(&memory_dir, &memory_db)
                .await;
            let later_bus = Arc::new(EventBus::new(projection_count.saturating_mul(2)));
            let mut later_events = later_bus.subscribe();
            let later =
                fixture.projection_manager(Arc::clone(&later_bus), Some(worker.handle.clone()));
            tokio::time::timeout(
                std::time::Duration::from_secs(60),
                later.recover_archive_cleanups(),
            )
            .await
            .expect("memory-enabled projection recovery must return")
            .expect("memory-enabled projection recovery succeeds");
            let completed = projection_attempt_snapshots(&later, &projection_ids).await;
            assert_eq!(completed.len(), projection_count);
            assert_eq!(
                completed
                    .iter()
                    .map(|snapshot| snapshot.projection_id)
                    .collect::<std::collections::BTreeSet<_>>(),
                expected_ids,
                "later worker recovers the same stable projection IDs"
            );
            for snapshot in completed {
                assert_eq!(snapshot.projection_state, "delivered");
                assert_eq!(snapshot.projection_attempt_count, 4);
                assert_eq!(
                    snapshot.consumers,
                    vec![
                        ("bus".into(), "delivered".into(), 1),
                        ("memory".into(), "delivered".into(), 2),
                        ("watch".into(), "delivered".into(), 1),
                    ]
                );
            }
            assert!(
                later_events.try_recv().is_err(),
                "later memory recovery does not replay delivered bus effects"
            );
            later_bus.unsubscribe();
            worker.shutdown().await;
        });
    }

    async fn expect_memory_sync_event(
        receiver: &mut tokio::sync::broadcast::Receiver<Arc<DaemonEvent>>,
    ) {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("memory sync event timeout")
            .expect("memory sync event");
        assert!(matches!(
            event.as_ref(),
            DaemonEvent::MemoryIndexUpdated { .. }
        ));
    }

    fn accept_one_memory_projection(
        mut receiver: tokio::sync::mpsc::Receiver<MemoryCommand>,
    ) -> tokio::task::JoinHandle<(Uuid, String)> {
        tokio::spawn(async move {
            let command = receiver.recv().await.expect("memory projection command");
            let MemoryCommand::SyncArchiveProjection {
                projection_id,
                reason,
                reply_tx,
            } = command
            else {
                panic!("expected archive projection sync command: {command:?}");
            };
            reply_tx
                .send(Ok(MemoryProjectionSyncCompletion {
                    projection_id,
                    disposition: MemoryProjectionSyncDisposition::Applied,
                }))
                .expect("return memory projection completion");
            (projection_id, reason)
        })
    }

    fn cleanup_error(error: DaemonError) -> ArchiveCleanupErrorV1 {
        let DaemonError::StructuredRpc { data, .. } = error else {
            panic!("expected structured cleanup error: {error}");
        };
        let error: ArchiveCleanupErrorV1 = serde_json::from_value(data).expect("safe error data");
        error.validate_wire().expect("safe error wire contract");
        error
    }

    #[test]
    fn removal_marker_is_canonical_and_rejects_unknown_authority() {
        let marker = ArchiveRemovalAuthorityV1 {
            version: 1,
            run_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            custody_id: Uuid::new_v4(),
            custody_generation: 1,
            preservation_class: ArchivePreservationClassV1::NoOutput,
            source_ref: "refs/heads/rsi/test".into(),
            source_oid: "a".repeat(40),
            target_ref: None,
            target_oid: None,
            original_root_digest: format!("sha256:{}", "1".repeat(64)),
            quarantine_root_digest: format!("sha256:{}", "2".repeat(64)),
            repository_identity_digest: format!("sha256:{}", "3".repeat(64)),
            git_admin_dir_digest: format!("sha256:{}", "4".repeat(64)),
            git_admin_id: "worktree".into(),
            clean_state_digest: format!("sha256:{}", "5".repeat(64)),
            stable_tree_digest: format!("sha256:{}", "6".repeat(64)),
            dependency_digest: format!("sha256:{}", "7".repeat(64)),
            holder_digest: format!("sha256:{}", "8".repeat(64)),
            journal_evidence_digest: format!("sha256:{}", "9".repeat(64)),
            branch_preserved: true,
        };
        let canonical = canonical_marker(&marker).unwrap();
        let decoded: ArchiveRemovalAuthorityV1 = serde_json::from_str(&canonical).unwrap();
        assert_eq!(decoded, marker);
        let mut value: serde_json::Value = serde_json::from_str(&canonical).unwrap();
        value["authority"] = serde_json::json!("caller");
        assert!(serde_json::from_value::<ArchiveRemovalAuthorityV1>(value).is_err());
    }

    #[test]
    fn no_output_cleanup_settles_and_preserves_direct_branch() {
        let fixture = CleanupFixture::new();
        let receipt = fixture.settle();
        assert_eq!(
            receipt.preservation_class,
            ArchivePreservationClassV1::NoOutput
        );
        fixture.assert_settled(&receipt, &fixture.allocation_oid);
        let replay = fixture.settle();
        assert_eq!(replay, receipt);
    }

    #[test]
    fn settled_replay_retains_one_projection_and_forward_consumer_receipts() {
        let fixture = CleanupFixture::new();
        let receipt = fixture.settle();
        let mut store = fixture.store.blocking_lock();
        let projections = store
            .pending_archive_cleanup_projections(Some(fixture.session_id), 8)
            .expect("load pending success projection");
        assert_eq!(projections.len(), 1);
        let projection = &projections[0];
        assert_eq!(projection.run_id, receipt.run_id);
        assert_eq!(projection.pending_consumers.len(), 3);
        let projection_id = projection.projection_id;

        assert!(
            store
                .begin_archive_cleanup_projection_consumer(
                    projection_id,
                    ArchiveProjectionConsumer::Bus,
                )
                .expect("claim bus projection")
        );
        store
            .complete_archive_cleanup_projection_consumer(
                projection_id,
                ArchiveProjectionConsumer::Bus,
            )
            .expect("accept bus projection");
        assert!(
            !store
                .begin_archive_cleanup_projection_consumer(
                    projection_id,
                    ArchiveProjectionConsumer::Bus,
                )
                .expect("delivered bus replay is a no-op")
        );
        drop(store);

        assert_eq!(fixture.settle(), receipt);
        let store = fixture.store.blocking_lock();
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM archive_cleanup_success_projections WHERE run_id=?1",
                    [receipt.run_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("logical projection cardinality"),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM archive_cleanup_projection_consumers WHERE projection_id=?1",
                    [projection_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("consumer receipt cardinality"),
            3
        );
        let pending = store
            .pending_archive_cleanup_projections(Some(fixture.session_id), 8)
            .expect("load replay projection");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].projection_id, projection_id);
        assert_eq!(pending[0].pending_consumers.len(), 2);
        assert!(
            !pending[0]
                .pending_consumers
                .contains(&ArchiveProjectionConsumer::Bus)
        );
    }

    #[test]
    fn projection_crash_before_application_recovers_all_consumers_once() {
        let fixture = CleanupFixture::new();
        let receipt = fixture.settle();
        let projection_id = fixture.projection_id();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("projection runtime");
        runtime.block_on(async {
            let event_bus = Arc::new(EventBus::new(8));
            let mut events = event_bus.subscribe();
            let (memory_tx, memory_rx) = tokio::sync::mpsc::channel(8);
            let manager = fixture
                .projection_manager(Arc::clone(&event_bus), Some(MemoryHandle::new(memory_tx)));

            install_archive_projection_dispatch_fault(
                projection_id,
                ArchiveProjectionConsumer::Watch,
                ArchiveProjectionDispatchFaultPoint::BeforeApplication,
            );
            manager
                .finish_archive_cleanup_projection(fixture.session_id)
                .await
                .expect_err("injected crash before application");
            assert_eq!(
                projection_consumer_state(&manager, projection_id, "watch").await,
                "delivering"
            );
            assert_eq!(delivered_consumer_count(&manager, projection_id).await, 0);
            assert!(events.try_recv().is_err(), "no effect precedes application");
            let memory_completion = accept_one_memory_projection(memory_rx);

            manager
                .recover_archive_cleanups()
                .await
                .expect("restart recovery completes the pending projection");
            let event = events.try_recv().expect("recovered bus application");
            assert!(matches!(
                event.as_ref(),
                DaemonEvent::SessionArchived {
                    session_id,
                    projection_id: Some(actual_projection),
                } if *session_id == fixture.session_id && *actual_projection == projection_id
            ));
            let (memory_projection_id, reason) = memory_completion
                .await
                .expect("join recovered memory completion");
            assert_eq!(memory_projection_id, projection_id);
            assert_eq!(
                reason,
                format!("session_archived:{}:{projection_id}", fixture.session_id)
            );
            assert_eq!(delivered_consumer_count(&manager, projection_id).await, 3);
            let replay = manager
                .try_archive_cleanup(fixture.session_id)
                .await
                .expect("exact replay succeeds")
                .expect("cleanup-backed replay result");
            assert_eq!(replay.receipt.as_ref(), Some(&receipt));
            manager
                .finish_archive_cleanup_projection(fixture.session_id)
                .await
                .expect("delivered replay is inert");
            assert!(
                events.try_recv().is_err(),
                "replay emits no second bus effect"
            );
            event_bus.unsubscribe();
        });
    }

    #[test]
    fn projection_crash_after_bus_application_redelivers_same_id_then_acks_once() {
        let fixture = CleanupFixture::new();
        let receipt = fixture.settle();
        let projection_id = fixture.projection_id();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("projection runtime");
        runtime.block_on(async {
            let first_bus = Arc::new(EventBus::new(8));
            let mut first_events = first_bus.subscribe();
            let first_manager = fixture.projection_manager(Arc::clone(&first_bus), None);
            install_archive_projection_dispatch_fault(
                projection_id,
                ArchiveProjectionConsumer::Bus,
                ArchiveProjectionDispatchFaultPoint::AfterApplicationBeforeAcknowledgement,
            );
            first_manager
                .finish_archive_cleanup_projection(fixture.session_id)
                .await
                .expect_err("injected crash after bus application");
            let first_event = first_events.try_recv().expect("pre-crash bus application");
            assert!(matches!(
                first_event.as_ref(),
                DaemonEvent::SessionArchived {
                    projection_id: Some(actual_projection),
                    ..
                } if *actual_projection == projection_id
            ));
            assert_eq!(
                projection_consumer_state(&first_manager, projection_id, "watch").await,
                "delivered"
            );
            assert_eq!(
                projection_consumer_state(&first_manager, projection_id, "bus").await,
                "delivering"
            );
            assert_eq!(
                projection_consumer_state(&first_manager, projection_id, "memory").await,
                "pending"
            );
            first_bus.unsubscribe();
            drop(first_events);
            drop(first_manager);
            drop(first_bus);

            let restarted_bus = Arc::new(EventBus::new(8));
            let mut restarted_events = restarted_bus.subscribe();
            let (memory_tx, memory_rx) = tokio::sync::mpsc::channel(8);
            let restarted = fixture.projection_manager(
                Arc::clone(&restarted_bus),
                Some(MemoryHandle::new(memory_tx)),
            );
            let memory_completion = accept_one_memory_projection(memory_rx);
            restarted
                .recover_archive_cleanups()
                .await
                .expect("restart accepts the stable projection ID");
            let restarted_event = restarted_events
                .try_recv()
                .expect("restart redelivers bus application");
            assert!(matches!(
                restarted_event.as_ref(),
                DaemonEvent::SessionArchived {
                    projection_id: Some(actual_projection),
                    ..
                } if *actual_projection == projection_id
            ));
            let (memory_projection_id, reason) = memory_completion
                .await
                .expect("join restarted memory completion");
            assert_eq!(memory_projection_id, projection_id);
            assert_eq!(
                reason,
                format!("session_archived:{}:{projection_id}", fixture.session_id)
            );
            assert_eq!(delivered_consumer_count(&restarted, projection_id).await, 3);
            let replay = restarted
                .try_archive_cleanup(fixture.session_id)
                .await
                .expect("post-recovery exact replay")
                .expect("cleanup-backed replay result");
            assert_eq!(replay.receipt.as_ref(), Some(&receipt));
            restarted
                .finish_archive_cleanup_projection(fixture.session_id)
                .await
                .expect("post-recovery replay is inert");
            assert!(restarted_events.try_recv().is_err());
            restarted_bus.unsubscribe();
        });
    }

    #[test]
    fn projection_memory_worker_failure_remains_delivering_until_same_id_succeeds() {
        let fixture = CleanupFixture::new();
        fixture.settle();
        let projection_id = fixture.projection_id();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("projection runtime");
        runtime.block_on(async {
            let memory_dir = fixture._temp.path().join("memory-worker-failure");
            let memory_db = fixture._temp.path().join("memory-worker-failure.sqlite");
            let worker = fixture
                .spawn_projection_memory_worker(&memory_dir, &memory_db)
                .await;
            let mut memory_events = worker.bus.subscribe();
            let first_bus = Arc::new(EventBus::new(8));
            let mut first_events = first_bus.subscribe();
            let first_manager =
                fixture.projection_manager(Arc::clone(&first_bus), Some(worker.handle.clone()));
            install_memory_projection_sync_failure(
                projection_id,
                MemoryProjectionSyncFaultPoint::BeforeSynchronization,
            );
            first_manager
                .finish_archive_cleanup_projection(fixture.session_id)
                .await
                .expect_err("worker-side sync failure propagates");
            assert!(matches!(
                first_events.try_recv().expect("bus application precedes memory" ).as_ref(),
                DaemonEvent::SessionArchived { projection_id: Some(actual), .. }
                    if *actual == projection_id
            ));
            assert_eq!(
                projection_consumer_state(&first_manager, projection_id, "watch").await,
                "delivered"
            );
            assert_eq!(
                projection_consumer_state(&first_manager, projection_id, "bus").await,
                "delivered"
            );
            assert_eq!(
                projection_consumer_state(&first_manager, projection_id, "memory").await,
                "delivering"
            );
            assert!(
                memory_events.try_recv().is_err(),
                "failed worker sync emits no completion event"
            );

            first_manager
                .recover_archive_cleanups()
                .await
                .expect("same worker retries the stable ID");
            expect_memory_sync_event(&mut memory_events).await;
            assert_eq!(
                delivered_consumer_count(&first_manager, projection_id).await,
                3
            );
            let completion = worker
                .handle
                .sync_archive_projection(projection_id, "exact-delivered-replay")
                .await
                .expect("delivered ID is idempotent");
            assert_eq!(completion.projection_id, projection_id);
            assert_eq!(
                completion.disposition,
                MemoryProjectionSyncDisposition::AlreadyDelivered
            );
            assert!(
                memory_events.try_recv().is_err(),
                "delivered replay does not reapply memory sync"
            );
            first_manager
                .recover_archive_cleanups()
                .await
                .expect("recovery replay is inert");
            first_bus.unsubscribe();
            worker.shutdown().await;
        });
    }

    #[test]
    fn projection_without_memory_worker_remains_delivering_for_later_recovery() {
        let fixture = CleanupFixture::new();
        fixture.settle();
        let projection_id = fixture.projection_id();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("projection runtime");
        runtime.block_on(async {
            let event_bus = Arc::new(EventBus::new(8));
            let mut events = event_bus.subscribe();
            let manager = fixture.projection_manager(Arc::clone(&event_bus), None);
            manager
                .finish_archive_cleanup_projection(fixture.session_id)
                .await
                .expect("unavailable memory worker leaves a retryable claim");
            assert!(matches!(
                events.try_recv().expect("bus projection applies").as_ref(),
                DaemonEvent::SessionArchived { projection_id: Some(actual), .. }
                    if *actual == projection_id
            ));
            assert_eq!(
                projection_consumer_state(&manager, projection_id, "memory").await,
                "delivering"
            );
            assert_eq!(delivered_consumer_count(&manager, projection_id).await, 2);

            manager
                .recover_archive_cleanups()
                .await
                .expect("recovery without a worker remains bounded");
            assert_eq!(
                projection_consumer_state(&manager, projection_id, "memory").await,
                "delivering"
            );
            assert!(
                events.try_recv().is_err(),
                "recovery does not replay the bus"
            );
            event_bus.unsubscribe();
        });
    }

    #[test]
    fn projection_recovery_without_memory_worker_terminates_at_full_32_row_page() {
        assert_disabled_memory_projection_batch_is_bounded(
            crate::store::archive_cleanup::ARCHIVE_CLEANUP_RECOVERY_BATCH as usize,
        );
    }

    #[test]
    fn projection_recovery_without_memory_worker_drains_33_rows_in_bounded_pages() {
        assert_disabled_memory_projection_batch_is_bounded(
            crate::store::archive_cleanup::ARCHIVE_CLEANUP_RECOVERY_BATCH as usize + 1,
        );
    }

    #[test]
    fn same_session_33rd_public_archive_drains_new_watch_and_bus_without_memory() {
        let fixture = Arc::new(CleanupFixture::new());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("same-Session projection runtime");
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(300), async {
                let projection_count =
                    crate::store::archive_cleanup::ARCHIVE_CLEANUP_RECOVERY_BATCH as usize + 1;
                let event_bus = Arc::new(EventBus::new(128));
                let mut events = event_bus.subscribe();
                let manager = fixture.projection_manager(Arc::clone(&event_bus), None);
                let mut projection_ids = Vec::with_capacity(projection_count);
                let mut older_before_final_archive = None;

                for ordinal in 0..projection_count {
                    let pre_settled = if ordinal + 1 < projection_count {
                        let fixture = Arc::clone(&fixture);
                        Some(
                            tokio::task::spawn_blocking(move || fixture.settle())
                                .await
                                .expect("join production cleanup saga"),
                        )
                    } else {
                        None
                    };
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        manager.archive_session(fixture.session_id),
                    )
                    .await
                    .expect("public archive must return")
                    .expect("public archive succeeds");
                    let receipt = result.receipt.expect("cleanup-backed archive receipt");
                    if let Some(pre_settled) = pre_settled {
                        assert_eq!(
                            receipt, pre_settled,
                            "public archive replays the exact saga receipt"
                        );
                    }
                    let projection_id =
                        crate::store::archive_cleanup::archive_projection_id(receipt.run_id);
                    assert!(
                        !projection_ids.contains(&projection_id),
                        "archive projection identity is unique per cleanup run"
                    );
                    projection_ids.push(projection_id);
                    assert_eq!(
                        projection_consumer_state(&manager, projection_id, "watch").await,
                        "delivered",
                        "archive cannot return before its watch projection"
                    );
                    assert_eq!(
                        projection_consumer_state(&manager, projection_id, "bus").await,
                        "delivered",
                        "archive cannot return before its bus projection"
                    );
                    assert_eq!(
                        projection_consumer_state(&manager, projection_id, "memory").await,
                        "delivering"
                    );

                    let event =
                        tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                            .await
                            .expect("archive bus event timeout")
                            .expect("archive bus event");
                    assert!(matches!(
                        event.as_ref(),
                        DaemonEvent::SessionArchived {
                            session_id,
                            projection_id: Some(actual),
                        } if *session_id == fixture.session_id && *actual == projection_id
                    ));

                    if ordinal + 1
                        == crate::store::archive_cleanup::ARCHIVE_CLEANUP_RECOVERY_BATCH as usize
                    {
                        let snapshot =
                            projection_attempt_snapshots(&manager, &projection_ids).await;
                        for projection in &snapshot {
                            assert_memory_only_retryable(projection);
                        }
                        older_before_final_archive = Some(snapshot);
                    }

                    if ordinal + 1 < projection_count {
                        tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            manager.unarchive_session(fixture.session_id),
                        )
                        .await
                        .expect("public unarchive must return")
                        .expect("public unarchive succeeds");
                        let event =
                            tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                                .await
                                .expect("unarchive bus event timeout")
                                .expect("unarchive bus event");
                        assert!(matches!(
                            event.as_ref(),
                            DaemonEvent::SessionUnarchived { session_id }
                                if *session_id == fixture.session_id
                        ));
                    }
                }

                assert_eq!(projection_ids.len(), projection_count);
                assert_eq!(
                    projection_ids
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len(),
                    projection_count,
                    "no bus projection/effect ID is duplicated"
                );
                assert_eq!(
                    projection_attempt_snapshots(
                        &manager,
                        &projection_ids[..projection_count - 1],
                    )
                    .await,
                    older_before_final_archive.expect("32-projection history snapshot"),
                    "the 33rd disabled-memory archive does not churn older memory attempts"
                );
                let stable_after_archive =
                    projection_attempt_snapshots(&manager, &projection_ids).await;
                for projection in &stable_after_archive {
                    assert_memory_only_retryable(projection);
                }
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    manager.finish_archive_cleanup_projection(fixture.session_id),
                )
                .await
                .expect("later disabled-memory finish must return")
                .expect("later disabled-memory finish succeeds");
                assert_eq!(
                    projection_attempt_snapshots(&manager, &projection_ids).await,
                    stable_after_archive,
                    "later disabled-memory finish leaves memory-only attempts unchanged"
                );
                assert!(
                    events.try_recv().is_err(),
                    "disabled-memory replay emits no duplicate bus effect"
                );
                event_bus.unsubscribe();

                let memory_dir = fixture._temp.path().join("memory-same-session-33");
                let memory_db = fixture._temp.path().join("memory-same-session-33.sqlite");
                let worker = fixture
                    .spawn_projection_memory_worker(&memory_dir, &memory_db)
                    .await;
                let mut memory_events = worker.bus.subscribe();
                let memory_event_collector = tokio::spawn(async move {
                    for _ in 0..projection_count {
                        expect_memory_sync_event(&mut memory_events).await;
                    }
                    assert!(
                        memory_events.try_recv().is_err(),
                        "one real worker effect is accepted per stable projection ID"
                    );
                });
                let later_bus = Arc::new(EventBus::new(128));
                let mut later_events = later_bus.subscribe();
                let later =
                    fixture.projection_manager(Arc::clone(&later_bus), Some(worker.handle.clone()));
                tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    later.recover_archive_cleanups(),
                )
                .await
                .expect("memory-enabled recovery must return")
                .expect("memory-enabled recovery succeeds");
                memory_event_collector
                    .await
                    .expect("collect every memory worker application");
                let completed = projection_attempt_snapshots(&later, &projection_ids).await;
                assert_eq!(completed.len(), projection_count);
                for projection in &completed {
                    assert_eq!(projection.projection_state, "delivered");
                    assert_eq!(projection.projection_attempt_count, 4);
                    assert_eq!(
                        projection.consumers,
                        vec![
                            ("bus".into(), "delivered".into(), 1),
                            ("memory".into(), "delivered".into(), 2),
                            ("watch".into(), "delivered".into(), 1),
                        ]
                    );
                }
                assert!(
                    later_events.try_recv().is_err(),
                    "memory recovery does not replay delivered bus effects"
                );
                later_bus.unsubscribe();

                let mut replay_events = worker.bus.subscribe();
                for projection_id in &projection_ids {
                    let completion = worker
                        .handle
                        .sync_archive_projection(*projection_id, "same-session-delivered-replay")
                        .await
                        .expect("delivered memory projection is idempotent");
                    assert_eq!(completion.projection_id, *projection_id);
                    assert_eq!(
                        completion.disposition,
                        MemoryProjectionSyncDisposition::AlreadyDelivered
                    );
                }
                assert!(
                    replay_events.try_recv().is_err(),
                    "stable-ID replay applies no second memory effect"
                );
                worker.shutdown().await;
            })
            .await
            .expect("same-Session 33-projection lifecycle must remain bounded");
        });
    }

    #[test]
    fn projection_memory_enqueue_then_worker_crash_redelivers_same_id_after_restart() {
        let fixture = CleanupFixture::new();
        fixture.settle();
        let projection_id = fixture.projection_id();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("projection runtime");
        runtime.block_on(async {
            let memory_dir = fixture._temp.path().join("memory-enqueue-crash");
            let memory_db = fixture._temp.path().join("memory-enqueue-crash.sqlite");
            let worker = fixture
                .spawn_projection_memory_worker(&memory_dir, &memory_db)
                .await;
            let mut first_memory_events = worker.bus.subscribe();
            let manager_bus = Arc::new(EventBus::new(8));
            let manager =
                fixture.projection_manager(Arc::clone(&manager_bus), Some(worker.handle.clone()));
            let pause = install_memory_projection_sync_pause(
                projection_id,
                MemoryProjectionSyncFaultPoint::BeforeSynchronization,
            );
            let (result, ()) = tokio::join!(
                manager.finish_archive_cleanup_projection(fixture.session_id),
                worker.abort_when_reached(pause),
            );
            assert!(matches!(result, Err(DaemonError::ChannelClosed)));
            assert_eq!(
                projection_consumer_state(&manager, projection_id, "memory").await,
                "delivering"
            );
            assert!(
                first_memory_events.try_recv().is_err(),
                "crash before worker effect emits no sync completion"
            );
            drop(manager);

            let restarted_worker = fixture
                .spawn_projection_memory_worker(&memory_dir, &memory_db)
                .await;
            let mut restarted_memory_events = restarted_worker.bus.subscribe();
            let restarted = fixture.projection_manager(
                Arc::new(EventBus::new(8)),
                Some(restarted_worker.handle.clone()),
            );
            restarted
                .recover_archive_cleanups()
                .await
                .expect("restart redelivers the same memory projection ID");
            expect_memory_sync_event(&mut restarted_memory_events).await;
            assert_eq!(
                projection_consumer_state(&restarted, projection_id, "memory").await,
                "delivered"
            );
            restarted
                .recover_archive_cleanups()
                .await
                .expect("post-restart replay is inert");
            assert!(restarted_memory_events.try_recv().is_err());
            restarted_worker.shutdown().await;
        });
    }

    #[test]
    fn projection_memory_success_before_ack_crash_retries_idempotently_after_restart() {
        let fixture = CleanupFixture::new();
        fixture.settle();
        let projection_id = fixture.projection_id();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("projection runtime");
        runtime.block_on(async {
            let memory_dir = fixture._temp.path().join("memory-success-crash");
            let memory_db = fixture._temp.path().join("memory-success-crash.sqlite");
            let worker = fixture
                .spawn_projection_memory_worker(&memory_dir, &memory_db)
                .await;
            let mut first_memory_events = worker.bus.subscribe();
            let manager =
                fixture.projection_manager(Arc::new(EventBus::new(8)), Some(worker.handle.clone()));
            let pause = install_memory_projection_sync_pause(
                projection_id,
                MemoryProjectionSyncFaultPoint::AfterSynchronizationBeforeAcknowledgement,
            );
            let (result, ()) = tokio::join!(
                manager.finish_archive_cleanup_projection(fixture.session_id),
                worker.abort_when_reached(pause),
            );
            assert!(matches!(result, Err(DaemonError::ChannelClosed)));
            assert!(
                first_memory_events.try_recv().is_err(),
                "worker crash before durable acknowledgement emits no accepted update"
            );
            assert_eq!(
                projection_consumer_state(&manager, projection_id, "memory").await,
                "delivering"
            );
            drop(manager);

            let restarted_worker = fixture
                .spawn_projection_memory_worker(&memory_dir, &memory_db)
                .await;
            let mut restarted_memory_events = restarted_worker.bus.subscribe();
            let restarted = fixture.projection_manager(
                Arc::new(EventBus::new(8)),
                Some(restarted_worker.handle.clone()),
            );
            restarted
                .recover_archive_cleanups()
                .await
                .expect("restart repeats the idempotent effect and acknowledges it");
            expect_memory_sync_event(&mut restarted_memory_events).await;
            assert_eq!(delivered_consumer_count(&restarted, projection_id).await, 3);

            let completion = restarted_worker
                .handle
                .sync_archive_projection(projection_id, "exact-delivered-replay")
                .await
                .expect("stable delivered ID is deduplicated");
            assert_eq!(completion.projection_id, projection_id);
            assert_eq!(
                completion.disposition,
                MemoryProjectionSyncDisposition::AlreadyDelivered
            );
            assert!(
                restarted_memory_events.try_recv().is_err(),
                "stable-ID replay cannot cause a second accepted application"
            );
            restarted_worker.shutdown().await;
        });
    }

    #[test]
    fn interrupted_cleanup_recovery_settles_and_dispatches_one_stable_projection() {
        let fixture = CleanupFixture::new();
        fixture.move_without_journal_ack();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("projection runtime");
        runtime.block_on(async {
            let event_bus = Arc::new(EventBus::new(8));
            let mut events = event_bus.subscribe();
            let (memory_tx, memory_rx) = tokio::sync::mpsc::channel(8);
            let manager = fixture
                .projection_manager(Arc::clone(&event_bus), Some(MemoryHandle::new(memory_tx)));
            let memory_completion = accept_one_memory_projection(memory_rx);
            let recovery_store = Arc::clone(&fixture.store);
            let recovery_sandbox_base = fixture.sandbox_base.clone();
            let recovery_session_id = fixture.session_id;
            tokio::task::spawn_blocking(move || {
                let proc = super::super::reaper::SyntheticQuarantineHolderProc::new();
                super::super::reaper::with_quarantine_holder_test_proc(
                    proc.proc_root(),
                    proc.uid(),
                    || {
                        resume_or_start_cleanup_blocking(
                            &recovery_store,
                            &recovery_sandbox_base,
                            recovery_session_id,
                        )
                    },
                )
            })
            .await
            .expect("join interrupted cleanup recovery")
            .expect("interrupted cleanup settles from exact quarantine evidence");
            manager
                .recover_archive_cleanups()
                .await
                .expect("interrupted cleanup and projection recover");
            let projection_id = projection_id_for(&manager, fixture.session_id).await;
            assert!(matches!(
                events.try_recv().expect("recovery bus application").as_ref(),
                DaemonEvent::SessionArchived { projection_id: Some(actual), .. }
                    if *actual == projection_id
            ));
            let (memory_projection_id, reason) = memory_completion
                .await
                .expect("join interrupted-cleanup memory completion");
            assert_eq!(memory_projection_id, projection_id);
            assert_eq!(
                reason,
                format!("session_archived:{}:{projection_id}", fixture.session_id)
            );
            assert_eq!(delivered_consumer_count(&manager, projection_id).await, 3);
            manager
                .recover_archive_cleanups()
                .await
                .expect("exact recovery replay is inert");
            assert!(events.try_recv().is_err());
            event_bus.unsubscribe();
        });
    }

    #[test]
    fn integrated_ancestor_cleanup_settles_and_preserves_direct_branch() {
        let fixture = CleanupFixture::new();
        let source_oid = fixture.commit_and_integrate_output();
        let receipt = fixture.settle();
        assert_eq!(
            receipt.preservation_class,
            ArchivePreservationClassV1::IntegratedAncestor
        );
        assert_eq!(receipt.target_ref.as_deref(), Some("refs/heads/main"));
        assert_eq!(
            receipt.target_oid.as_ref().map(|oid| oid.as_str()),
            Some(source_oid.as_str())
        );
        fixture.assert_settled(&receipt, &source_oid);
    }

    #[test]
    fn settled_cleanup_allows_fresh_custody_unarchive_without_reusing_source_branch() {
        let fixture = CleanupFixture::new();
        let receipt = fixture.settle();
        let allocation_id = Uuid::new_v4();
        let target_oid = git(&fixture.repository, &["rev-parse", "refs/heads/main"]);
        let allocation = SandboxAllocator::new(fixture.sandbox_base.clone())
            .allocate(
                allocation_id,
                &fixture.repository,
                SandboxKind::GitWorktree,
                &target_oid,
                None,
            )
            .expect("allocate fresh unarchive worktree");
        let new_branch = allocation.branch.clone().expect("fresh branch");
        assert_ne!(format!("refs/heads/{new_branch}"), fixture.source_ref);
        let restored = fixture
            .store
            .blocking_lock()
            .restore_archived_session_with_fresh_custody(
                fixture.session_id,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id: Uuid::new_v4(),
                    canonical_repo_dir: fixture.repository.display().to_string(),
                    sandbox_root: allocation.root.display().to_string(),
                    sandbox_branch: new_branch.clone(),
                    repository_identity: git(
                        &fixture.repository,
                        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    ),
                    source_commit: target_oid,
                    cause: CustodyCause::FreshLaunch,
                }),
            )
            .expect("ordinary fresh-custody unarchive");
        assert_eq!(restored.id, fixture.session_id);
        assert_eq!(restored.status, SessionStatus::Completed);
        assert_eq!(restored.title.as_deref(), Some("raw operator title"));
        assert_eq!(restored.agent_role.as_deref(), Some("implementer"));
        assert_eq!(restored.epic_spawn_ordinal, Some(7));
        assert_eq!(
            restored.sandbox_root.as_deref(),
            Some(allocation.root.as_path())
        );
        assert_eq!(
            restored.sandbox_branch.as_deref(),
            Some(new_branch.as_str())
        );
        let restored_custody = fixture
            .store
            .blocking_lock()
            .live_custody_for_session(fixture.session_id)
            .expect("fresh live custody");
        assert_ne!(restored_custody.custody_id, receipt.custody_id);
        assert_eq!(
            git(&fixture.repository, &["rev-parse", &fixture.source_ref]),
            receipt.source_oid.as_str()
        );

        let new_root = allocation.root.clone();
        let new_ref = format!("refs/heads/{new_branch}");
        let second_receipt = fixture.settle();
        assert_ne!(second_receipt.run_id, receipt.run_id);
        assert_ne!(second_receipt.custody_id, receipt.custody_id);
        assert_eq!(second_receipt.source_branch, new_ref);
        assert_eq!(
            second_receipt.source_oid.as_str(),
            receipt.source_oid.as_str()
        );
        assert!(!new_root.exists());
        assert_eq!(
            git(&fixture.repository, &["rev-parse", &fixture.source_ref]),
            receipt.source_oid.as_str()
        );
    }

    #[test]
    fn unique_output_is_refused_before_intent_with_zero_git_effects() {
        let fixture = CleanupFixture::new();
        let source_oid = fixture.commit_unique_output();
        let error = cleanup_error(
            fixture
                .cleanup_result()
                .expect_err("unique output retained"),
        );
        assert_eq!(
            error.safe_code,
            ArchiveCleanupSafeCodeV1::OutputNotIntegrated
        );
        assert!(error.run_id.is_none());
        fixture.assert_retained(&source_oid);
        assert!(
            fixture
                .store
                .blocking_lock()
                .latest_archive_cleanup_for_session(fixture.session_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn dirty_worktree_is_refused_before_intent_with_zero_git_effects() {
        let fixture = CleanupFixture::new();
        std::fs::write(fixture.root.join("tracked"), "dirty\n").expect("dirty worktree");
        let error = cleanup_error(
            fixture
                .cleanup_result()
                .expect_err("dirty worktree retained"),
        );
        assert_eq!(error.safe_code, ArchiveCleanupSafeCodeV1::WorktreeDirty);
        assert!(error.run_id.is_none());
        fixture.assert_retained(&fixture.allocation_oid);
    }

    #[test]
    fn preintent_classifier_distinguishes_cleanup_dependency_from_dirty_worktree() {
        for (message, expected) in [
            (
                "archive cleanup ownership or dependency inventory changed",
                ArchiveCleanupSafeCodeV1::DependencyPresent,
            ),
            (
                "registered worktree identity or cleanliness did not match",
                ArchiveCleanupSafeCodeV1::WorktreeDirty,
            ),
            (
                "worktree index observation is unavailable",
                ArchiveCleanupSafeCodeV1::WorktreeDirty,
            ),
        ] {
            let classified =
                classify_preintent_error(DaemonError::Process(message.to_owned()), None);
            assert_eq!(cleanup_error(classified).safe_code, expected, "{message}");
        }
    }

    #[test]
    fn later_lineage_successor_is_refused_before_intent() {
        let fixture = CleanupFixture::new();
        let mut successor = crate::store::tests::make_test_session();
        successor.id = Uuid::new_v4();
        successor.continued_from = Some(fixture.session_id);
        successor.session_kind = SessionKind::Task;
        fixture
            .store
            .blocking_lock()
            .insert_session(&successor)
            .expect("insert lineage successor");
        let error = cleanup_error(fixture.cleanup_result().expect_err("lineage retained"));
        assert_eq!(
            error.safe_code,
            ArchiveCleanupSafeCodeV1::SessionTopologyChanged
        );
        fixture.assert_retained(&fixture.allocation_oid);
    }

    #[test]
    fn lost_move_acknowledgement_recovers_from_exact_quarantine() {
        let fixture = CleanupFixture::new();
        let run = fixture.move_without_journal_ack();
        assert_eq!(run.phase, ArchiveCleanupPhaseV1::IntentCommitted);
        assert!(!fixture.root.exists());
        assert!(Path::new(&run.quarantine_root).exists());
        let receipt = fixture.settle();
        fixture.assert_settled(&receipt, &fixture.allocation_oid);
    }

    #[test]
    fn forged_marker_becomes_durable_recovery_required_without_removal() {
        let fixture = CleanupFixture::new();
        let run = fixture.authorize_removal(|marker| marker.source_oid = "f".repeat(40));
        let quarantine = PathBuf::from(&run.quarantine_root);
        let error = cleanup_error(
            fixture
                .cleanup_result()
                .expect_err("forged marker retained"),
        );
        assert_eq!(
            error.safe_code,
            ArchiveCleanupSafeCodeV1::RecoveryEvidenceAmbiguous
        );
        assert!(!error.retryable);
        assert!(quarantine.exists());
        assert_eq!(
            git(&fixture.repository, &["rev-parse", &fixture.source_ref]),
            fixture.allocation_oid
        );
        let latest = fixture
            .store
            .blocking_lock()
            .latest_archive_cleanup_for_session(fixture.session_id)
            .unwrap()
            .unwrap();
        assert_eq!(latest.phase, ArchiveCleanupPhaseV1::RecoveryRequired);
    }

    #[test]
    fn lost_removal_acknowledgement_recovers_only_from_exact_absence() {
        let fixture = CleanupFixture::new();
        let run = fixture.remove_without_journal_ack();
        assert_eq!(run.phase, ArchiveCleanupPhaseV1::RemovalAuthorized);
        assert!(!fixture.root.exists());
        assert!(!Path::new(&run.quarantine_root).exists());
        let receipt = fixture.settle();
        fixture.assert_settled(&receipt, &fixture.allocation_oid);
    }

    #[test]
    fn final_transaction_failure_rolls_back_and_exact_retry_settles() {
        let fixture = CleanupFixture::new();
        let run = fixture.remove_without_journal_ack();
        let run = fixture
            .store
            .blocking_lock()
            .advance_archive_cleanup_phase(
                run.run_id,
                run.phase,
                run.row_version,
                ArchiveCleanupPhaseV1::WorktreeRemoved,
                "test_worktree_removed",
                None,
            )
            .expect("acknowledge fixture removal");
        crate::store::archive_cleanup::fail_next_archive_cleanup_final_commit();
        let error = cleanup_error(
            fixture
                .cleanup_result()
                .expect_err("injected final transaction failure"),
        );
        assert_eq!(
            error.safe_code,
            ArchiveCleanupSafeCodeV1::DatabaseSettlementFailed
        );
        assert!(error.retryable);
        let store = fixture.store.blocking_lock();
        assert_eq!(
            store
                .archive_cleanup_run(run.run_id)
                .unwrap()
                .unwrap()
                .phase,
            ArchiveCleanupPhaseV1::WorktreeRemoved
        );
        assert_eq!(
            store
                .get_session(fixture.session_id)
                .unwrap()
                .unwrap()
                .status,
            SessionStatus::Completed
        );
        drop(store);
        let receipt = fixture.settle();
        fixture.assert_settled(&receipt, &fixture.allocation_oid);
    }
}
