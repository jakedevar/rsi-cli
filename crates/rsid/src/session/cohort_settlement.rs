//! Operator-only repository cohort audit, settlement, and retained recovery.
//!
//! Authority comes exclusively from V94 custody rows plus fresh local Git
//! observations. Audit is a zero-write operation. Apply persists a complete
//! intent before its first Git effect and advances each item monotonically.

use super::SessionManager;
use crate::bus::DaemonEvent;
use crate::error::{DaemonError, Result};
use crate::sandbox::git_worktree;
use crate::store::cohort_settlement::{
    InsertSettlementRunOutcome, NewSettlementItem, NewSettlementRun,
    QuarantineRemoveAuthorityDigestDomainV1, QuarantineRemoveAuthorityFactsV1,
    QuarantineRemoveAuthorityV1, SourceWorktreeInventoryQuarantineAlias,
    SourceWorktreeInventoryRow, SourceWorktreeSettlementJournalItem,
    quarantine_remove_authority_field_digest,
};
use rsi_common::cohort_settlement::{
    ApplySourceWorktreeCohortParams, SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST,
    SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST, SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS,
    SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION, SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
    SourceWorktreeAuditItemV1, SourceWorktreeCohortAuditV1, SourceWorktreeCohortSummaryV1,
    SourceWorktreeDispositionV1, SourceWorktreeGitOidV1, SourceWorktreeProofV1,
    SourceWorktreeSettlementCountsV1, SourceWorktreeSettlementPhaseV1,
    SourceWorktreeSettlementRefusalV1, SourceWorktreeSettlementRunV1, validate_source_ref,
};
use rsi_common::types::{SessionKind, Sha256Digest};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone)]
struct EligibleSettlementItem {
    inventory: SourceWorktreeInventoryRow,
    source_ref: String,
    source_oid: String,
    clean_state_digest: String,
    evidence_digest: String,
}

#[derive(Debug)]
struct AuditSnapshot {
    report: SourceWorktreeCohortAuditV1,
    eligible: Vec<EligibleSettlementItem>,
}

enum AuditStart {
    ReceiptOnly(SourceWorktreeCohortAuditV1),
    Live(String),
}

enum ApplyStart {
    Fresh(AuditSnapshot),
    Replay(SourceWorktreeSettlementRunV1),
}

fn phase_can_still_apply_git_effect(phase: &SourceWorktreeSettlementPhaseV1) -> bool {
    matches!(
        phase,
        SourceWorktreeSettlementPhaseV1::IntentCommitted
            | SourceWorktreeSettlementPhaseV1::WorktreeRemoved
            | SourceWorktreeSettlementPhaseV1::BranchRemoved
    )
}

fn effect_capable_replay_session_ids(receipt: &SourceWorktreeSettlementRunV1) -> Vec<Uuid> {
    let mut session_ids = receipt
        .items
        .iter()
        .filter(|item| phase_can_still_apply_git_effect(&item.phase))
        .map(|item| item.session_id)
        .collect::<Vec<_>>();
    session_ids.sort_unstable();
    session_ids.dedup();
    session_ids
}

struct ApplyOutcome {
    receipt: SourceWorktreeSettlementRunV1,
    settled_ids: Vec<Uuid>,
}

#[derive(Debug, Clone)]
struct RuntimePathInput {
    session_id: Uuid,
    field: &'static str,
    raw: PathBuf,
}

#[derive(Debug, Clone)]
struct RuntimePathObservation {
    session_id: Uuid,
    field: &'static str,
    raw: PathBuf,
    canonical: Option<PathBuf>,
}

#[derive(Serialize)]
struct CanonicalEvidence<'a> {
    schema_version: u32,
    policy_version: u32,
    repository_identity: &'a str,
    canonical_repo_dir: &'a str,
    target_ref: Option<&'a str>,
    target_oid: Option<&'a str>,
    session_id: String,
    linked_session_id: Option<String>,
    session_status: &'a str,
    session_updated_at: &'a str,
    session_updated_at_raw: &'a str,
    session_kind: &'a str,
    session_working_dir: Option<&'a str>,
    session_sandbox_kind: Option<&'a str>,
    session_sandbox_root: Option<&'a str>,
    session_sandbox_branch: Option<&'a str>,
    session_cleanup_state: Option<&'a str>,
    pending_archive: bool,
    custody_id: String,
    custody_generation: u64,
    validated_generation: u64,
    validation_state: &'a str,
    owner_session_id: Option<String>,
    participant_count: u64,
    reserved_effects: u64,
    active_effects: u64,
    scheduled_dependency_count: u64,
    scheduled_dependency_digest: &'a str,
    session_path_dependency_count: u64,
    session_path_dependency_digest: &'a str,
    sandbox_root: &'a str,
    sandbox_branch: &'a str,
    source_commit: &'a str,
    source_ref: &'a str,
    source_oid: Option<&'a str>,
    clean_state_digest: Option<&'a str>,
    proof: &'a SourceWorktreeProofV1,
    disposition: &'a SourceWorktreeDispositionV1,
    git: &'a CanonicalGitEvidence,
}

#[derive(Serialize)]
struct CanonicalGitEvidence {
    observation_outcome: &'static str,
    source_resolution_outcome: &'static str,
    runtime_active: bool,
    runtime_dependency_count: u32,
    runtime_dependency_digest: String,
    root_exists: Option<bool>,
    root_is_symlink: Option<bool>,
    expected_root_match: Option<bool>,
    canonical_base: Option<String>,
    canonical_root: Option<String>,
    canonical_identity_match: Option<bool>,
    registered: Option<bool>,
    registered_head: Option<String>,
    registered_branch: Option<String>,
    head_oid: Option<String>,
    head_ref: Option<String>,
    clean: Option<bool>,
    clean_state_digest: Option<String>,
    unrelated_ref_digest: Option<String>,
    source_has_symref_dependents: Option<bool>,
}

impl Default for CanonicalGitEvidence {
    fn default() -> Self {
        Self {
            observation_outcome: "not_reached",
            source_resolution_outcome: "not_reached",
            runtime_active: false,
            runtime_dependency_count: 0,
            runtime_dependency_digest: runtime_dependency_digest(&[]),
            root_exists: None,
            root_is_symlink: None,
            expected_root_match: None,
            canonical_base: None,
            canonical_root: None,
            canonical_identity_match: None,
            registered: None,
            registered_head: None,
            registered_branch: None,
            head_oid: None,
            head_ref: None,
            clean: None,
            clean_state_digest: None,
            unrelated_ref_digest: None,
            source_has_symref_dependents: None,
        }
    }
}

#[derive(Serialize)]
struct CanonicalAuditPlan<'a> {
    schema_version: u32,
    policy_version: u32,
    repository_identity: &'a str,
    canonical_repo_dir: &'a str,
    target_ref: Option<&'a str>,
    target_oid: Option<&'a str>,
    refusal: Option<&'a str>,
    items: &'a [SourceWorktreeAuditItemV1],
}

#[derive(Serialize)]
struct CanonicalApplyFingerprint<'a> {
    schema_version: u32,
    policy_version: u32,
    repository_identity: &'a str,
    plan_digest: &'a str,
    authorization_digest: &'a str,
    idempotency_key: &'a str,
}

impl SessionManager {
    pub async fn list_source_worktree_cohorts(&self) -> Result<Vec<SourceWorktreeCohortSummaryV1>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let summaries = store.list_source_worktree_cohorts()?;
            summaries
                .into_iter()
                .map(|summary| summary.validate_wire().map_err(DaemonError::Store))
                .collect()
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))?
    }

    pub async fn audit_source_worktree_cohort(
        &self,
        repository_identity: String,
    ) -> Result<SourceWorktreeCohortAuditV1> {
        rsi_common::cohort_settlement::validate_identity(&repository_identity)
            .map_err(DaemonError::InvalidParam)?;
        let identity = repository_identity.clone();
        let store = Arc::clone(&self.store);
        let start = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let inventory = store.source_worktree_inventory(
                &identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
            )?;
            if !inventory.is_empty() {
                return unique_repository_dir(&identity, &inventory).map(AuditStart::Live);
            }
            let receipt = store
                .latest_source_worktree_settlement_run(&identity)?
                .map(|receipt| receipt.validate_wire().map_err(DaemonError::Store))
                .transpose()?
                .ok_or_else(|| {
                    DaemonError::InvalidParam(
                        "repository cohort has neither Live custody roots nor a durable receipt"
                            .into(),
                    )
                })?;
            durable_receipt_only_audit(&receipt).map(AuditStart::ReceiptOnly)
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))??;
        let canonical_repo_dir = match start {
            AuditStart::ReceiptOnly(report) => return Ok(report),
            AuditStart::Live(canonical_repo_dir) => canonical_repo_dir,
        };
        let active = self.active.read().await;
        let active_ids = active.keys().copied().collect::<HashSet<_>>();
        let active_cwds = collect_runtime_path_inputs(&active);
        drop(active);
        let sandbox_base = self.sandbox_allocator.base_dir().to_path_buf();
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let origin = PathBuf::from(&canonical_repo_dir);
            git_worktree::with_repository_mutation(&origin, || {
                let (inventory, latest) = {
                    let store = store.blocking_lock();
                    let inventory = store.source_worktree_inventory(
                        &repository_identity,
                        SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
                    )?;
                    let latest = store
                        .latest_source_worktree_settlement_run(&repository_identity)?
                        .map(|receipt| receipt.validate_wire().map_err(DaemonError::Store))
                        .transpose()?;
                    (inventory, latest)
                };
                if inventory.is_empty() {
                    let receipt = latest.as_ref().ok_or_else(|| {
                        DaemonError::InvalidParam(
                            "repository cohort lost its Live roots and durable receipt during audit"
                                .into(),
                        )
                    })?;
                    if receipt.canonical_repo_dir != canonical_repo_dir {
                        return Err(DaemonError::InvalidParam(
                            "repository cohort path mapping drifted during receipt recovery".into(),
                        ));
                    }
                    return durable_receipt_only_audit(receipt);
                }
                let locked_repo_dir = unique_repository_dir(&repository_identity, &inventory)?;
                if locked_repo_dir != canonical_repo_dir {
                    return Err(DaemonError::InvalidParam(
                        "repository cohort path mapping drifted during audit".into(),
                    ));
                }
                let mut report = build_audit_locked(
                    &repository_identity,
                    &canonical_repo_dir,
                    &sandbox_base,
                    inventory,
                    &active_ids,
                    &active_cwds,
                )?
                .report;
                report.run_id = latest.map(|receipt| receipt.run_id);
                report.validate_wire().map_err(DaemonError::Store)
            })
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))?
    }

    pub async fn get_source_worktree_settlement_run(
        &self,
        run_id: Uuid,
    ) -> Result<Option<SourceWorktreeSettlementRunV1>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store
                .get_source_worktree_settlement_run(run_id)?
                .map(|receipt| receipt.validate_wire().map_err(DaemonError::Store))
                .transpose()
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))?
    }

    pub async fn apply_source_worktree_cohort(
        &self,
        params: ApplySourceWorktreeCohortParams,
    ) -> Result<SourceWorktreeSettlementRunV1> {
        params.validate().map_err(DaemonError::InvalidParam)?;
        let authorization_digest = hash_bytes(params.authorization.as_bytes());
        let request_fingerprint = hash_canonical(&CanonicalApplyFingerprint {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: &params.repository_identity,
            plan_digest: params.plan_digest.as_str(),
            authorization_digest: authorization_digest.as_str(),
            idempotency_key: &params.idempotency_key,
        })?;

        let replay = {
            let store = Arc::clone(&self.store);
            let identity = params.repository_identity.clone();
            let key = params.idempotency_key.clone();
            let fingerprint = request_fingerprint.as_str().to_string();
            tokio::task::spawn_blocking(move || {
                store.blocking_lock().replay_source_worktree_settlement_run(
                    &identity,
                    &key,
                    &fingerprint,
                )
            })
            .await
            .map_err(|error| DaemonError::Process(error.to_string()))??
        }
        .map(|receipt| receipt.validate_wire().map_err(DaemonError::Store))
        .transpose()?;

        if let Some(receipt) = replay.as_ref()
            && receipt.items.iter().all(|item| item.phase.is_terminal())
        {
            let receipt = receipt.clone();
            let settled_ids = receipt
                .items
                .iter()
                .filter(|item| item.phase == SourceWorktreeSettlementPhaseV1::Settled)
                .map(|item| item.session_id)
                .collect::<Vec<_>>();
            self.project_source_worktree_settlement(&settled_ids).await;
            return Ok(receipt);
        }

        let start =
            if let Some(receipt) = replay {
                ApplyStart::Replay(receipt)
            } else {
                let snapshot = self
                    .source_worktree_audit_snapshot(&params.repository_identity)
                    .await?;
                if !snapshot.report.applyable {
                    return Err(DaemonError::InvalidParam(
                        snapshot.report.refusal.clone().unwrap_or_else(|| {
                            "source-worktree cohort has no eligible roots".into()
                        }),
                    ));
                }
                if snapshot.report.plan_digest != params.plan_digest {
                    return Err(DaemonError::InvalidParam(
                        "source-worktree audit digest is stale".into(),
                    ));
                }
                ApplyStart::Fresh(snapshot)
            };

        let mut session_ids = match &start {
            ApplyStart::Fresh(snapshot) => snapshot
                .eligible
                .iter()
                .filter_map(|item| item.inventory.session_id)
                .collect::<Vec<_>>(),
            ApplyStart::Replay(receipt) => effect_capable_replay_session_ids(receipt),
        };
        session_ids.sort_unstable();
        session_ids.dedup();
        let mut spawn_guards = Vec::with_capacity(session_ids.len());
        for &session_id in &session_ids {
            spawn_guards.push(super::spawn_single_flight::acquire_spawn_guard(session_id).await);
        }
        let cwd_exclusion = super::spawn_single_flight::acquire_settlement_cwd_exclusion().await;
        let active = self.active.read().await;
        let owner_became_active = session_ids
            .iter()
            .any(|session_id| active.contains_key(session_id));
        drop(active);
        if owner_became_active {
            return Err(DaemonError::InvalidParam(
                "source-worktree settlement owner became active after audit".into(),
            ));
        }
        let orphan_candidates = session_ids.clone();
        tokio::task::spawn_blocking(move || {
            super::reaper::prove_startup_settlement_orphan_scan_readable(&orphan_candidates)
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))??;

        let canonical_repo_dir = match &start {
            ApplyStart::Fresh(snapshot) => snapshot.report.canonical_repo_dir.clone(),
            ApplyStart::Replay(receipt) => receipt.canonical_repo_dir.clone(),
        };
        let sandbox_base = self.sandbox_allocator.base_dir().to_path_buf();
        let store = Arc::clone(&self.store);
        let active = Arc::clone(&self.active);
        let completed = Arc::clone(&self.completed);
        let identity = params.repository_identity.clone();
        let key = params.idempotency_key.clone();
        let plan_digest = params.plan_digest.as_str().to_string();
        let authorization_digest = authorization_digest.as_str().to_string();
        let request_fingerprint = request_fingerprint.as_str().to_string();
        let outcome = tokio::task::spawn_blocking(move || {
            let _spawn_guards = spawn_guards;
            let origin = PathBuf::from(&canonical_repo_dir);
            git_worktree::with_repository_mutation(&origin, || {
                apply_locked_with_orphan_proof(
                    start,
                    &identity,
                    &key,
                    &plan_digest,
                    &authorization_digest,
                    &request_fingerprint,
                    &sandbox_base,
                    &store,
                    &active,
                    &completed,
                    true,
                )
            })
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))??;
        drop(cwd_exclusion);

        self.project_source_worktree_settlement(&outcome.settled_ids)
            .await;
        outcome.receipt.validate_wire().map_err(DaemonError::Store)
    }

    /// Resume only retained V94 intents. Recovery never enumerates a new
    /// cohort and never creates a run; each action is selected by exact journal
    /// identity and phase.
    pub(super) async fn recover_source_worktree_settlements(&self) -> Result<()> {
        let mut after_run_id = None;
        loop {
            let run_ids = {
                let store = Arc::clone(&self.store);
                tokio::task::spawn_blocking(move || {
                    store
                        .blocking_lock()
                        .list_nonterminal_source_worktree_settlement_run_ids(after_run_id, 64)
                })
                .await
                .map_err(|error| DaemonError::Process(error.to_string()))??
            };
            if run_ids.is_empty() {
                break;
            }
            for run_id in run_ids {
                after_run_id = Some(run_id);
                let (receipt, journal) = {
                    let store = Arc::clone(&self.store);
                    tokio::task::spawn_blocking(move || {
                        let store = store.blocking_lock();
                        let receipt = store
                            .get_source_worktree_settlement_run(run_id)?
                            .ok_or_else(|| {
                                DaemonError::Store("recovery receipt disappeared".into())
                            })?
                            .validate_wire()
                            .map_err(DaemonError::Store)?;
                        let journal =
                            store.list_source_worktree_settlement_journal_items(run_id)?;
                        Ok::<_, DaemonError>((receipt, journal))
                    })
                    .await
                    .map_err(|error| DaemonError::Process(error.to_string()))??
                };
                let mut session_ids = journal
                    .iter()
                    .filter(|item| phase_can_still_apply_git_effect(&item.phase))
                    .map(|item| item.session_id)
                    .collect::<Vec<_>>();
                session_ids.sort_unstable();
                session_ids.dedup();
                let mut spawn_guards = Vec::with_capacity(session_ids.len());
                for session_id in session_ids {
                    spawn_guards
                        .push(super::spawn_single_flight::acquire_spawn_guard(session_id).await);
                }
                let cwd_exclusion =
                    super::spawn_single_flight::acquire_settlement_cwd_exclusion().await;
                let store = Arc::clone(&self.store);
                let active = Arc::clone(&self.active);
                let completed = Arc::clone(&self.completed);
                let sandbox_base = self.sandbox_allocator.base_dir().to_path_buf();
                tokio::task::spawn_blocking(move || {
                    let _spawn_guards = spawn_guards;
                    let repository = PathBuf::from(&receipt.canonical_repo_dir);
                    let mut entered_repository = false;
                    let recovered = git_worktree::with_repository_mutation(&repository, || {
                        entered_repository = true;
                        let mut stopped = false;
                        for item in &journal {
                            if item.phase.is_terminal() {
                                if item.phase != SourceWorktreeSettlementPhaseV1::Settled {
                                    stopped = true;
                                }
                                continue;
                            }
                            if stopped {
                                mark_unattempted_under_root(&store, run_id, item, true)?;
                                continue;
                            }
                            match settle_one_item_locked(
                                &store,
                                &active,
                                &completed,
                                run_id,
                                &receipt.target_ref,
                                &repository,
                                &sandbox_base,
                                item,
                                true,
                            )? {
                                true => {}
                                false => stopped = true,
                            }
                        }
                        Ok(())
                    });
                    if let Err(error) = recovered {
                        if !entered_repository {
                            tracing::warn!(run_id=%run_id, error=%error, "Settlement repository unavailable during startup recovery");
                            return Ok::<_, DaemonError>(());
                        } else {
                            return Err(error);
                        }
                    }
                    let store = store.blocking_lock();
                    if store.source_worktree_settlement_run_has_nonterminal_items(run_id)? {
                        return Err(DaemonError::Store(format!(
                            "settlement recovery made no terminal progress for run {run_id}"
                        )));
                    }
                    let receipt = store
                        .get_source_worktree_settlement_run(run_id)?
                        .ok_or_else(|| DaemonError::Store("recovery receipt disappeared".into()))?;
                    if receipt.items.iter().any(|item| {
                        item.phase == SourceWorktreeSettlementPhaseV1::RecoveryRequired
                    }) {
                        tracing::warn!(run_id=%run_id, "Settlement recovery retained an operator-inspection fence");
                    }
                    Ok::<_, DaemonError>(())
                })
                .await
                .map_err(|error| DaemonError::Process(error.to_string()))??;
                drop(cwd_exclusion);
            }
        }
        Ok(())
    }

    async fn source_worktree_audit_snapshot(
        &self,
        repository_identity: &str,
    ) -> Result<AuditSnapshot> {
        let inventory = self
            .load_source_worktree_inventory(repository_identity)
            .await?;
        let active = self.active.read().await;
        let active_ids = active.keys().copied().collect::<HashSet<_>>();
        let active_cwds = collect_runtime_path_inputs(&active);
        drop(active);
        let sandbox_base = self.sandbox_allocator.base_dir().to_path_buf();
        let identity = repository_identity.to_string();
        tokio::task::spawn_blocking(move || {
            build_audit(
                &identity,
                &sandbox_base,
                inventory,
                &active_ids,
                &active_cwds,
            )
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))?
    }

    async fn project_source_worktree_settlement(&self, settled_ids: &[Uuid]) {
        if settled_ids.is_empty() {
            return;
        }
        let settled = settled_ids.iter().copied().collect::<HashSet<_>>();
        let mut projected_sessions = Vec::new();
        {
            let mut completed = self.completed.write().await;
            for session_id in settled_ids {
                if let Some(mut session) = completed.remove(session_id) {
                    projected_sessions.push(*session_id);
                    if let Some(cancel) = session.retry_cancel.take() {
                        let _ = cancel.send(());
                    }
                }
            }
        }

        let mut affected_epics = BTreeSet::new();
        {
            let mut active = self.active.write().await;
            for tracked in active.values_mut() {
                if tracked.session.session_kind == SessionKind::Epic
                    && tracked
                        .session
                        .lead_session_id
                        .is_some_and(|lead| settled.contains(&lead))
                {
                    tracked.session.lead_session_id = None;
                    tracked.session.updated_at = chrono::Utc::now();
                    affected_epics.insert(tracked.session.id);
                }
            }
        }
        {
            let mut completed = self.completed.write().await;
            for completed_session in completed.values_mut() {
                if completed_session.session.session_kind == SessionKind::Epic
                    && completed_session
                        .session
                        .lead_session_id
                        .is_some_and(|lead| settled.contains(&lead))
                {
                    completed_session.session.lead_session_id = None;
                    completed_session.session.updated_at = chrono::Utc::now();
                    affected_epics.insert(completed_session.session.id);
                }
            }
        }
        for epic_id in affected_epics {
            self.event_bus.publish(DaemonEvent::SessionMetadataChanged {
                session_id: epic_id,
                model: None,
                pinned_at: None,
                project_id: None,
                parent_id: None,
                lead_session_id: Some(None),
                testing_needed_at: None,
                rotation_disabled_at: None,
                resolved_context_budget: None,
            });
        }
        for session_id in projected_sessions {
            self.event_bus.publish(DaemonEvent::SessionArchived {
                session_id,
                projection_id: None,
            });
        }
    }

    async fn load_source_worktree_inventory(
        &self,
        repository_identity: &str,
    ) -> Result<Vec<SourceWorktreeInventoryRow>> {
        let identity = repository_identity.to_string();
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.source_worktree_inventory(
                &identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
            )
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))?
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_locked(
    start: ApplyStart,
    repository_identity: &str,
    idempotency_key: &str,
    plan_digest: &str,
    authorization_digest: &str,
    request_fingerprint: &str,
    sandbox_base: &Path,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
) -> Result<ApplyOutcome> {
    apply_locked_with_orphan_proof(
        start,
        repository_identity,
        idempotency_key,
        plan_digest,
        authorization_digest,
        request_fingerprint,
        sandbox_base,
        store,
        active,
        completed,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_locked_with_orphan_proof(
    start: ApplyStart,
    repository_identity: &str,
    idempotency_key: &str,
    plan_digest: &str,
    authorization_digest: &str,
    request_fingerprint: &str,
    sandbox_base: &Path,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    reap_orphans_after_intent: bool,
) -> Result<ApplyOutcome> {
    let (run_id, canonical_repo_dir, target_ref, replaying) = match start {
        ApplyStart::Fresh(snapshot) => {
            let active_guard = active.try_read().map_err(|_| {
                DaemonError::InvalidParam(
                    "runtime state is busy; audit was not applied and may be retried".into(),
                )
            })?;
            let active_ids = active_guard.keys().copied().collect::<HashSet<_>>();
            let active_cwds = collect_runtime_path_inputs(&active_guard);
            drop(active_guard);
            let inventory = store.blocking_lock().source_worktree_inventory(
                repository_identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
            )?;
            let canonical_repo_dir = unique_repository_dir(repository_identity, &inventory)?;
            if canonical_repo_dir != snapshot.report.canonical_repo_dir {
                return Err(DaemonError::InvalidParam(
                    "source-worktree audit path mapping drifted".into(),
                ));
            }
            let recomputed = build_audit_locked(
                repository_identity,
                &canonical_repo_dir,
                sandbox_base,
                inventory,
                &active_ids,
                &active_cwds,
            )?;
            if !recomputed.report.applyable
                || recomputed.report.plan_digest.as_str() != plan_digest
                || recomputed.report.plan_digest != snapshot.report.plan_digest
                || recomputed.report.authorization_phrase != snapshot.report.authorization_phrase
            {
                return Err(DaemonError::InvalidParam(
                    "source-worktree audit drifted before durable intent".into(),
                ));
            }
            let target_ref = recomputed
                .report
                .target_ref
                .clone()
                .ok_or_else(|| DaemonError::Store("applyable audit lacks target ref".into()))?;
            let target_oid = recomputed
                .report
                .target_oid
                .as_ref()
                .ok_or_else(|| DaemonError::Store("applyable audit lacks target oid".into()))?
                .as_str()
                .to_string();
            let run_id = Uuid::new_v4();
            let new_run = NewSettlementRun {
                run_id,
                repository_identity: repository_identity.to_string(),
                canonical_repo_dir: canonical_repo_dir.clone(),
                target_ref: target_ref.clone(),
                target_oid: target_oid.clone(),
                plan_digest: plan_digest.to_string(),
                idempotency_key: idempotency_key.to_string(),
                authorization_digest: authorization_digest.to_string(),
                request_fingerprint: request_fingerprint.to_string(),
                observed_count: recomputed.report.counts.observed,
                retained_count: recomputed.report.counts.retained,
                items: recomputed
                    .eligible
                    .iter()
                    .map(|item| NewSettlementItem {
                        session_id: item
                            .inventory
                            .session_id
                            .expect("eligible item has Session owner"),
                        original_status: item.inventory.status.clone().expect("eligible status"),
                        original_updated_at: item
                            .inventory
                            .session_updated_at
                            .clone()
                            .expect("eligible updated_at"),
                        custody_id: item.inventory.custody_id,
                        custody_generation: item.inventory.generation,
                        canonical_repo_dir: item.inventory.canonical_repo_dir.clone(),
                        sandbox_root: item.inventory.sandbox_root.clone(),
                        sandbox_branch: item.inventory.sandbox_branch.clone(),
                        repository_identity: item.inventory.repository_identity.clone(),
                        source_ref: item.source_ref.clone(),
                        source_oid: item.source_oid.clone(),
                        target_oid: target_oid.clone(),
                        evidence_digest: item.evidence_digest.clone(),
                        clean_state_digest: item.clean_state_digest.clone(),
                        reserved_effects: item.inventory.reserved_effects,
                        active_effects: item.inventory.active_effects,
                        participant_count: item.inventory.participant_count,
                    })
                    .collect(),
            };
            let outcome = store
                .blocking_lock()
                .insert_source_worktree_settlement_run(&new_run)?;
            match outcome {
                InsertSettlementRunOutcome::Inserted => {
                    (run_id, canonical_repo_dir, target_ref, false)
                }
                InsertSettlementRunOutcome::Replay(receipt) => {
                    let receipt = receipt.validate_wire().map_err(DaemonError::Store)?;
                    (
                        receipt.run_id,
                        receipt.canonical_repo_dir,
                        receipt.target_ref,
                        true,
                    )
                }
            }
        }
        ApplyStart::Replay(receipt) => {
            let receipt = receipt.validate_wire().map_err(DaemonError::Store)?;
            if receipt.repository_identity != repository_identity
                || receipt.idempotency_key != idempotency_key
                || receipt.plan_digest.as_str() != plan_digest
            {
                return Err(DaemonError::InvalidParam(
                    "settlement replay authority does not match its retained receipt".into(),
                ));
            }
            (
                receipt.run_id,
                receipt.canonical_repo_dir,
                receipt.target_ref,
                true,
            )
        }
    };

    let journal = store
        .blocking_lock()
        .list_source_worktree_settlement_journal_items(run_id)?;
    if reap_orphans_after_intent {
        let candidate_ids = journal
            .iter()
            .filter(|item| phase_can_still_apply_git_effect(&item.phase))
            .map(|item| item.session_id)
            .collect::<Vec<_>>();
        let reaped = super::reaper::reap_startup_settlement_orphans_checked(&candidate_ids)?;
        if reaped > 0 {
            tracing::warn!(
                reaped,
                run_id = %run_id,
                "Reaped exact-stamped provider orphan(s) after durable settlement intent"
            );
        }
    }
    let mut settled_ids = journal
        .iter()
        .filter(|item| item.phase == SourceWorktreeSettlementPhaseV1::Settled)
        .map(|item| item.session_id)
        .collect::<Vec<_>>();
    let mut stopped = false;
    for item in &journal {
        if item.phase.is_terminal() {
            if item.phase != SourceWorktreeSettlementPhaseV1::Settled {
                stopped = true;
            }
            continue;
        }
        if stopped {
            mark_unattempted_under_root(store, run_id, item, replaying)?;
            continue;
        }
        match settle_one_item_locked(
            store,
            active,
            completed,
            run_id,
            &target_ref,
            Path::new(&canonical_repo_dir),
            sandbox_base,
            item,
            replaying,
        ) {
            Ok(true) => settled_ids.push(item.session_id),
            Ok(false) => stopped = true,
            Err(error) => {
                tracing::warn!(
                    run_id = %run_id,
                    session_id = %item.session_id,
                    error = %error,
                    "Settlement item retained after an unexpected saga failure"
                );
                stopped = true;
            }
        }
    }
    settled_ids.sort_unstable();
    settled_ids.dedup();
    let receipt = store
        .blocking_lock()
        .get_source_worktree_settlement_run(run_id)?
        .ok_or_else(|| DaemonError::Store("settlement receipt disappeared".into()))?;
    Ok(ApplyOutcome {
        receipt,
        settled_ids,
    })
}

#[allow(clippy::too_many_arguments)]
fn settle_one_item_locked(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    run_id: Uuid,
    target_ref: &str,
    canonical_repo_dir: &Path,
    sandbox_base: &Path,
    item: &SourceWorktreeSettlementJournalItem,
    replaying: bool,
) -> Result<bool> {
    let _root_guard = crate::store::sandbox_custody::lock_custody_root(item.custody_id);
    let original_root = Path::new(&item.sandbox_root);
    let quarantine_root = match git_worktree::derive_settlement_quarantine_path(
        original_root,
        run_id,
        item.session_id,
    ) {
        Ok(root) => root,
        Err(error) => {
            drop(_root_guard);
            mark_item_stopped_blocking(
                store,
                run_id,
                item,
                SourceWorktreeSettlementRefusalV1::CustodyDrift,
                "the retained sandbox root cannot derive its exact quarantine path",
                replaying,
            )?;
            tracing::warn!(session_id=%item.session_id, error=%error, "Settlement quarantine path derivation failed");
            return Ok(false);
        }
    };
    let retained_marker = item
        .before_observation
        .as_deref()
        .and_then(|raw| QuarantineRemoveAuthorityV1::parse_canonical(raw).ok())
        .filter(|marker| {
            quarantine_authority_matches_retained_item(
                marker,
                run_id,
                target_ref,
                original_root,
                &quarantine_root,
                item,
            )
        });
    let mut restored_missing_source_from_marker = false;
    if matches!(
        item.phase,
        SourceWorktreeSettlementPhaseV1::IntentCommitted
            | SourceWorktreeSettlementPhaseV1::WorktreeRemoved
            | SourceWorktreeSettlementPhaseV1::BranchRemoved
    ) && let Some(marker) = retained_marker.as_ref()
    {
        let source = git_worktree::observe_direct_ref_locked(canonical_repo_dir, &item.source_ref);
        let exact_effect_absence =
            matches!(&source, Ok(git_worktree::DirectRefObservation::Missing))
                && matches!(nofollow_path_exists(original_root), Ok(false))
                && matches!(nofollow_path_exists(&quarantine_root), Ok(false))
                && matches!(
                    worktree_paths_are_absent_and_unregistered(
                        canonical_repo_dir,
                        original_root,
                        &quarantine_root,
                    ),
                    Ok(true)
                )
                && matches!(
                    git_worktree::source_ref_has_zero_registrations_locked(
                        canonical_repo_dir,
                        &item.source_ref,
                    ),
                    Ok(true)
                );
        let restore = match source {
            Ok(git_worktree::DirectRefObservation::Missing) if !exact_effect_absence => {
                restore_missing_source_ref_from_retained_marker(
                    run_id,
                    target_ref,
                    canonical_repo_dir,
                    original_root,
                    &quarantine_root,
                    item,
                    marker,
                )
            }
            Ok(_) => Ok(false),
            Err(error) => Err(error),
        };
        match restore {
            Ok(restored) => restored_missing_source_from_marker = restored,
            Err(error) => {
                let observation = source_restore_failure_observation(canonical_repo_dir, item);
                drop(_root_guard);
                mark_source_restore_failure_blocking(store, run_id, item, observation)?;
                tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not prove exact source restoration");
                return Ok(false);
            }
        }
    }
    let active = match active.try_read() {
        Ok(active) => active,
        Err(_) => {
            let restore_error = if !restored_missing_source_from_marker {
                retained_marker.as_ref().and_then(|marker| {
                    restore_missing_source_ref_from_retained_marker(
                        run_id,
                        target_ref,
                        canonical_repo_dir,
                        original_root,
                        &quarantine_root,
                        item,
                        marker,
                    )
                    .err()
                })
            } else {
                None
            };
            let restore_failure = restore_error.map(|error| {
                (
                    error,
                    source_restore_failure_observation(canonical_repo_dir, item),
                )
            });
            drop(_root_guard);
            if let Some((error, observation)) = restore_failure {
                mark_source_restore_failure_blocking(store, run_id, item, observation)?;
                tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not preserve the source before active-map-contention terminalization");
                return Ok(false);
            }
            mark_item_stopped_blocking(
                store,
                run_id,
                item,
                SourceWorktreeSettlementRefusalV1::RuntimeStateContended,
                "active runtime map was contended before effect",
                replaying,
            )?;
            return Ok(false);
        }
    };
    if runtime_overlaps_settlement_roots(&active, item.session_id, original_root, &quarantine_root)
    {
        if !restored_missing_source_from_marker
            && let Some(marker) = retained_marker.as_ref()
            && let Err(error) = restore_missing_source_ref_from_retained_marker(
                run_id,
                target_ref,
                canonical_repo_dir,
                original_root,
                &quarantine_root,
                item,
                marker,
            )
        {
            let observation = source_restore_failure_observation(canonical_repo_dir, item);
            drop(active);
            drop(_root_guard);
            mark_source_restore_failure_blocking(store, run_id, item, observation)?;
            tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not preserve the source before runtime-drift terminalization");
            return Ok(false);
        }
        drop(active);
        drop(_root_guard);
        mark_item_stopped_blocking(
            store,
            run_id,
            item,
            SourceWorktreeSettlementRefusalV1::RuntimeOwnerActive,
            "an active provider owner or effective cwd overlaps the sandbox before settlement effect",
            replaying,
        )?;
        return Ok(false);
    }
    let completed = match completed.try_read() {
        Ok(completed) => completed,
        Err(_) => {
            let restore_error = if !restored_missing_source_from_marker {
                retained_marker.as_ref().and_then(|marker| {
                    restore_missing_source_ref_from_retained_marker(
                        run_id,
                        target_ref,
                        canonical_repo_dir,
                        original_root,
                        &quarantine_root,
                        item,
                        marker,
                    )
                    .err()
                })
            } else {
                None
            };
            let restore_failure = restore_error.map(|error| {
                (
                    error,
                    source_restore_failure_observation(canonical_repo_dir, item),
                )
            });
            drop(active);
            drop(_root_guard);
            if let Some((error, observation)) = restore_failure {
                mark_source_restore_failure_blocking(store, run_id, item, observation)?;
                tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not preserve the source before completed-map-contention terminalization");
                return Ok(false);
            }
            mark_item_stopped_blocking(
                store,
                run_id,
                item,
                SourceWorktreeSettlementRefusalV1::RuntimeStateContended,
                "completed runtime map was contended before effect",
                replaying,
            )?;
            return Ok(false);
        }
    };
    let mut store_guard = match store.try_lock() {
        Ok(store) => store,
        Err(_) => {
            let restore_error = if !restored_missing_source_from_marker {
                retained_marker.as_ref().and_then(|marker| {
                    restore_missing_source_ref_from_retained_marker(
                        run_id,
                        target_ref,
                        canonical_repo_dir,
                        original_root,
                        &quarantine_root,
                        item,
                        marker,
                    )
                    .err()
                })
            } else {
                None
            };
            let restore_failure = restore_error.map(|error| {
                (
                    error,
                    source_restore_failure_observation(canonical_repo_dir, item),
                )
            });
            drop(completed);
            drop(active);
            drop(_root_guard);
            if let Some((error, observation)) = restore_failure {
                mark_source_restore_failure_blocking(store, run_id, item, observation)?;
                tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not preserve the source before Store-contention terminalization");
                return Ok(false);
            }
            mark_item_stopped_blocking(
                store,
                run_id,
                item,
                SourceWorktreeSettlementRefusalV1::RuntimeStateContended,
                "Store was contended under the custody fence",
                replaying,
            )?;
            return Ok(false);
        }
    };

    let mut current_phase = item.phase.clone();
    let mut authority = None;
    let mut legacy_effect_receipt = false;

    if current_phase == SourceWorktreeSettlementPhaseV1::IntentCommitted {
        if let Some(raw) = item.before_observation.as_deref() {
            match QuarantineRemoveAuthorityV1::parse_canonical(raw) {
                Ok(marker)
                    if quarantine_authority_matches_retained_item(
                        &marker,
                        run_id,
                        target_ref,
                        original_root,
                        &quarantine_root,
                        item,
                    ) =>
                {
                    authority = Some(marker);
                }
                _ => {
                    store_guard.mark_source_worktree_settlement_recovery_required(
                        run_id,
                        item.session_id,
                        current_phase,
                        SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                        "intent contains malformed, noncanonical, or mismatched removal authority",
                    )?;
                    return Ok(false);
                }
            }
        }
    }

    let mut durable_tuple_matches = runtime_completed_matches(&completed, item)
        && journal_inventory_matches(&store_guard, run_id, item, original_root)?;

    if current_phase == SourceWorktreeSettlementPhaseV1::IntentCommitted {
        let original_exists = nofollow_path_exists(original_root)?;
        let quarantine_exists = nofollow_path_exists(&quarantine_root)?;

        if authority.is_some() {
            match (original_exists, quarantine_exists) {
                (false, false) => {
                    if !worktree_paths_are_absent_and_unregistered(
                        canonical_repo_dir,
                        original_root,
                        &quarantine_root,
                    )? {
                        if let Some(marker) = authority.as_ref()
                            && let Err(error) = restore_missing_source_ref_from_retained_marker(
                                run_id,
                                target_ref,
                                canonical_repo_dir,
                                original_root,
                                &quarantine_root,
                                item,
                                marker,
                            )
                        {
                            store_guard.mark_source_worktree_settlement_recovery_required(
                                run_id,
                                item.session_id,
                                current_phase,
                                SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                                source_restore_failure_observation(canonical_repo_dir, item),
                            )?;
                            tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not preserve the source before registration-residue terminalization");
                            return Ok(false);
                        }
                        store_guard.mark_source_worktree_settlement_recovery_required(
                            run_id,
                            item.session_id,
                            current_phase,
                            SourceWorktreeSettlementRefusalV1::WorktreeResidue.as_str(),
                            "authorized removal left a worktree registration behind",
                        )?;
                        return Ok(false);
                    }
                    match git_worktree::observe_direct_ref_locked(
                        canonical_repo_dir,
                        &item.source_ref,
                    )? {
                        git_worktree::DirectRefObservation::Missing => {
                            store_guard.advance_source_worktree_settlement_item(
                                run_id,
                                item.session_id,
                                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                                SourceWorktreeSettlementPhaseV1::WorktreeRemoved,
                                None,
                                Some("canonical quarantine authority plus exact dual-path, registration, and source-ref absence prove both interrupted Git effects completed"),
                                None,
                            )?;
                            current_phase = SourceWorktreeSettlementPhaseV1::WorktreeRemoved;
                        }
                        _ => {
                            store_guard.mark_source_worktree_settlement_recovery_required(
                                run_id,
                                item.session_id,
                                current_phase,
                                SourceWorktreeSettlementRefusalV1::SourceRefDrift.as_str(),
                                "the quarantine sentinel is absent but the source ref is not exactly absent",
                            )?;
                            return Ok(false);
                        }
                    }
                }
                (false, true) => {}
                _ => {
                    if let Some(marker) = authority.as_ref()
                        && let Err(error) = restore_missing_source_ref_from_retained_marker(
                            run_id,
                            target_ref,
                            canonical_repo_dir,
                            original_root,
                            &quarantine_root,
                            item,
                            marker,
                        )
                    {
                        store_guard.mark_source_worktree_settlement_recovery_required(
                            run_id,
                            item.session_id,
                            current_phase,
                            SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                            source_restore_failure_observation(canonical_repo_dir, item),
                        )?;
                        tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not preserve the source before path-collision terminalization");
                        return Ok(false);
                    }
                    store_guard.mark_source_worktree_settlement_recovery_required(
                        run_id,
                        item.session_id,
                        current_phase,
                        SourceWorktreeSettlementRefusalV1::WorktreeResidue.as_str(),
                        "an authorized quarantine has an original-path recreation or path collision",
                    )?;
                    return Ok(false);
                }
            }
        } else {
            match (original_exists, quarantine_exists) {
                (true, false) => {
                    if !durable_tuple_matches
                        || original_root != sandbox_base.join(item.session_id.to_string())
                        || !root_is_exact_sandbox(original_root, sandbox_base, item.session_id)
                    {
                        stop_item_locked(
                            &store_guard,
                            run_id,
                            item,
                            current_phase,
                            SourceWorktreeSettlementRefusalV1::CustodyDrift,
                            "Session, custody, or exact sandbox identity drifted before quarantine",
                            replaying,
                            false,
                        )?;
                        return Ok(false);
                    }
                    let preflight = (|| -> Result<()> {
                        let observation = git_worktree::observe_worktree_locked(
                            canonical_repo_dir,
                            original_root,
                        )?;
                        if observation.root_is_symlink
                            || !observation.root_exists
                            || !observation.registered
                            || observation.registered_branch.as_deref()
                                != Some(item.source_ref.as_str())
                            || observation.head_ref.as_deref() != Some(item.source_ref.as_str())
                            || observation.registered_head.as_deref()
                                != Some(item.source_oid.as_str())
                            || observation.head_oid.as_deref() != Some(item.source_oid.as_str())
                            || !observation.clean
                            || observation.clean_state_digest != item.clean_state_digest
                            || git_worktree::resolve_ref_locked(
                                canonical_repo_dir,
                                &item.source_ref,
                            )?
                            .as_deref()
                                != Some(item.source_oid.as_str())
                            || !git_worktree::source_ref_is_registered_only_at_locked(
                                canonical_repo_dir,
                                &item.source_ref,
                                original_root,
                                &item.source_oid,
                            )?
                            || git_worktree::source_ref_has_symref_dependents_locked(
                                canonical_repo_dir,
                                &item.source_ref,
                            )?
                        {
                            return Err(DaemonError::Process(
                                "original worktree or source ref drifted before quarantine".into(),
                            ));
                        }
                        observe_preserving_target(
                            canonical_repo_dir,
                            target_ref,
                            &item.repository_identity,
                            &item.source_oid,
                        )?;
                        Ok(())
                    })();
                    if let Err(error) = preflight {
                        stop_item_locked(
                            &store_guard,
                            run_id,
                            item,
                            current_phase,
                            SourceWorktreeSettlementRefusalV1::CustodyDrift,
                            "original worktree failed its final pre-quarantine proof",
                            replaying,
                            false,
                        )?;
                        tracing::warn!(session_id=%item.session_id, error=%error, "Settlement pre-quarantine proof failed");
                        return Ok(false);
                    }
                    let path_proof = match git_worktree::prepare_settlement_quarantine_path(
                        original_root,
                        run_id,
                        item.session_id,
                    ) {
                        Ok(proof) => proof,
                        Err(error) => {
                            stop_item_locked(
                                &store_guard,
                                run_id,
                                item,
                                current_phase,
                                SourceWorktreeSettlementRefusalV1::WorktreeRemoveFailed,
                                "the deterministic quarantine could not be prepared",
                                replaying,
                                false,
                            )?;
                            tracing::warn!(session_id=%item.session_id, error=%error, "Settlement quarantine preparation failed");
                            return Ok(false);
                        }
                    };
                    if let Err(error) = git_worktree::move_worktree_to_quarantine_non_force_locked(
                        canonical_repo_dir,
                        &path_proof,
                        &item.source_ref,
                        &item.source_oid,
                    ) {
                        store_guard.mark_source_worktree_settlement_recovery_required(
                            run_id,
                            item.session_id,
                            current_phase,
                            SourceWorktreeSettlementRefusalV1::WorktreeResidue.as_str(),
                            "the quarantine move was attempted but its exact postcondition was not proved",
                        )?;
                        tracing::warn!(session_id=%item.session_id, error=%error, "Settlement quarantine move retained candidate");
                        return Ok(false);
                    }
                }
                (false, true) => {
                    if let Err(error) = git_worktree::prove_moved_worktree_exact_locked(
                        canonical_repo_dir,
                        original_root,
                        &quarantine_root,
                        &item.source_ref,
                        &item.source_oid,
                    ) {
                        if let Err(repair_error) =
                            git_worktree::repair_moved_worktree_if_exact_locked(
                                canonical_repo_dir,
                                original_root,
                                &quarantine_root,
                                &item.source_ref,
                                &item.source_oid,
                            )
                        {
                            store_guard.mark_source_worktree_settlement_recovery_required(
                                run_id,
                                item.session_id,
                                current_phase,
                                SourceWorktreeSettlementRefusalV1::WorktreeResidue.as_str(),
                                "the interrupted quarantine move is ambiguous and was not repaired",
                            )?;
                            tracing::warn!(session_id=%item.session_id, error=%error, repair_error=%repair_error, "Settlement quarantine repair retained candidate");
                            return Ok(false);
                        }
                    }
                }
                (true, true) => {
                    store_guard.mark_source_worktree_settlement_recovery_required(
                        run_id,
                        item.session_id,
                        current_phase,
                        SourceWorktreeSettlementRefusalV1::WorktreeResidue.as_str(),
                        "both original and quarantine paths exist after durable intent",
                    )?;
                    return Ok(false);
                }
                (false, false) => {
                    store_guard.mark_source_worktree_settlement_recovery_required(
                        run_id,
                        item.session_id,
                        current_phase,
                        SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                        "both worktree paths are absent without canonical durable removal authority",
                    )?;
                    return Ok(false);
                }
            }
        }

        if current_phase == SourceWorktreeSettlementPhaseV1::IntentCommitted {
            durable_tuple_matches = runtime_completed_matches(&completed, item)
                && journal_inventory_matches(&store_guard, run_id, item, original_root)?;
            if !durable_tuple_matches
                || runtime_overlaps_settlement_roots(
                    &active,
                    item.session_id,
                    original_root,
                    &quarantine_root,
                )
            {
                if !restored_missing_source_from_marker
                    && let Some(retained) = authority.as_ref()
                    && let Err(error) = restore_missing_source_ref_from_retained_marker(
                        run_id,
                        target_ref,
                        canonical_repo_dir,
                        original_root,
                        &quarantine_root,
                        item,
                        retained,
                    )
                {
                    store_guard.mark_source_worktree_settlement_recovery_required(
                        run_id,
                        item.session_id,
                        current_phase,
                        SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                        source_restore_failure_observation(canonical_repo_dir, item),
                    )?;
                    tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not preserve the source before custody-drift terminalization");
                    return Ok(false);
                }
                store_guard.mark_source_worktree_settlement_recovery_required(
                    run_id,
                    item.session_id,
                    current_phase,
                    SourceWorktreeSettlementRefusalV1::CustodyDrift.as_str(),
                    "runtime or durable path authority drifted after quarantine",
                )?;
                return Ok(false);
            }

            if let Some(retained) = authority.as_ref() {
                if !restored_missing_source_from_marker
                    && matches!(
                        git_worktree::observe_direct_ref_locked(
                            canonical_repo_dir,
                            &item.source_ref,
                        )?,
                        git_worktree::DirectRefObservation::Missing
                    )
                {
                    match restore_missing_source_ref_from_retained_marker(
                        run_id,
                        target_ref,
                        canonical_repo_dir,
                        original_root,
                        &quarantine_root,
                        item,
                        retained,
                    ) {
                        Ok(restored) => restored_missing_source_from_marker = restored,
                        Err(error) => {
                            store_guard.mark_source_worktree_settlement_recovery_required(
                                run_id,
                                item.session_id,
                                current_phase,
                                SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                                source_restore_failure_observation(canonical_repo_dir, item),
                            )?;
                            tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not prove exact source restoration");
                            return Ok(false);
                        }
                    }
                }
                if restored_missing_source_from_marker
                    && let Err(error) = git_worktree::restore_source_ref_and_reattach_detached_quarantine_atomically_locked(
                        canonical_repo_dir,
                        original_root,
                        &quarantine_root,
                        &item.source_ref,
                        &item.source_oid,
                    )
                {
                    store_guard.mark_source_worktree_settlement_recovery_required(
                        run_id,
                        item.session_id,
                        current_phase,
                        SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                        "the exact source ref was restored, but the retained quarantine could not be reattached without overwriting drift",
                    )?;
                    tracing::warn!(session_id=%item.session_id, error=%error, "Exact source restoration retained quarantine drift");
                    return Ok(false);
                }
            }

            let (observed_authority, first_tree) = match prove_quarantine_remove_authority(
                run_id,
                target_ref,
                canonical_repo_dir,
                original_root,
                &quarantine_root,
                item,
                None,
            ) {
                Ok(proof) => proof,
                Err(error) => {
                    store_guard.mark_source_worktree_settlement_recovery_required(
                        run_id,
                        item.session_id,
                        current_phase,
                        SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                        "quarantined worktree failed its removal-authority proof",
                    )?;
                    tracing::warn!(session_id=%item.session_id, error=%error, "Settlement quarantine authority proof failed");
                    return Ok(false);
                }
            };
            if let Some(retained) = authority.as_ref() {
                if retained != &observed_authority {
                    store_guard.mark_source_worktree_settlement_recovery_required(
                        run_id,
                        item.session_id,
                        current_phase,
                        SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                        "quarantine facts no longer match the retained removal authority",
                    )?;
                    return Ok(false);
                }
            } else {
                if let Err(error) = store_guard.record_source_worktree_quarantine_remove_authority(
                    run_id,
                    item.session_id,
                    &observed_authority,
                ) {
                    store_guard.mark_source_worktree_settlement_recovery_required(
                        run_id,
                        item.session_id,
                        current_phase,
                        SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                        "durable quarantine removal authority could not be committed exactly",
                    )?;
                    tracing::warn!(session_id=%item.session_id, error=%error, "Settlement authority persistence retained quarantine");
                    return Ok(false);
                }
                authority = Some(observed_authority);
            }

            durable_tuple_matches = runtime_completed_matches(&completed, item)
                && journal_inventory_matches(&store_guard, run_id, item, original_root)?;
            if !durable_tuple_matches
                || runtime_overlaps_settlement_roots(
                    &active,
                    item.session_id,
                    original_root,
                    &quarantine_root,
                )
            {
                store_guard.mark_source_worktree_settlement_recovery_required(
                    run_id,
                    item.session_id,
                    current_phase,
                    SourceWorktreeSettlementRefusalV1::CustodyDrift.as_str(),
                    "runtime or durable path authority drifted after marker persistence",
                )?;
                return Ok(false);
            }
            let (final_authority, _) = match prove_quarantine_remove_authority(
                run_id,
                target_ref,
                canonical_repo_dir,
                original_root,
                &quarantine_root,
                item,
                Some(&first_tree),
            ) {
                Ok(proof) => proof,
                Err(error) => {
                    store_guard.mark_source_worktree_settlement_recovery_required(
                        run_id,
                        item.session_id,
                        current_phase,
                        SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                        "quarantine changed during its final post-marker proof",
                    )?;
                    tracing::warn!(session_id=%item.session_id, error=%error, "Settlement final quarantine proof retained candidate");
                    return Ok(false);
                }
            };
            if authority.as_ref() != Some(&final_authority) {
                store_guard.mark_source_worktree_settlement_recovery_required(
                    run_id,
                    item.session_id,
                    current_phase,
                    SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                    "the final quarantine proof does not equal its durable marker",
                )?;
                return Ok(false);
            }

            if git_worktree::source_ref_has_symref_dependents_locked(
                canonical_repo_dir,
                &item.source_ref,
            )? || !git_worktree::source_ref_is_registered_only_at_locked(
                canonical_repo_dir,
                &item.source_ref,
                &quarantine_root,
                &item.source_oid,
            )? {
                store_guard.mark_source_worktree_settlement_recovery_required(
                    run_id,
                    item.session_id,
                    current_phase,
                    SourceWorktreeSettlementRefusalV1::SourceRefInUse.as_str(),
                    "the source ref acquired a symbolic dependent or non-sentinel registration before deletion",
                )?;
                return Ok(false);
            }
            let unrelated_before =
                git_worktree::other_ref_state_digest_locked(canonical_repo_dir, &item.source_ref)?;
            let delete = git_worktree::delete_source_ref_atomically_locked(
                canonical_repo_dir,
                target_ref,
                final_authority.removal_target_oid().as_str(),
                &item.source_ref,
                &item.source_oid,
            );
            if let Err(error) = &delete {
                let restore = compensate_branch_first_source_ref(
                    canonical_repo_dir,
                    original_root,
                    &quarantine_root,
                    &item.source_ref,
                    &item.source_oid,
                );
                let source_restored = matches!(
                    git_worktree::observe_direct_ref_locked(
                        canonical_repo_dir,
                        &item.source_ref,
                    ),
                    Ok(git_worktree::DirectRefObservation::Commit(ref oid))
                        if oid == &item.source_oid
                );
                if let Err(restore_error) = restore {
                    tracing::warn!(session_id=%item.session_id, error=%restore_error, "Source-ref compensation after deletion error was refused");
                }
                store_guard.mark_source_worktree_settlement_recovery_required(
                    run_id,
                    item.session_id,
                    current_phase,
                    SourceWorktreeSettlementRefusalV1::RefDeleteFailed.as_str(),
                    if source_restored {
                        "source deletion returned an error; the exact source ref was synchronously restored"
                    } else {
                        "source deletion returned an error and exact source-ref restoration was not proved"
                    },
                )?;
                tracing::warn!(session_id=%item.session_id, error=%error, "Exact source-ref deletion failed closed");
                return Ok(false);
            }
            let deletion_completed = (|| -> Result<bool> {
                Ok(matches!(
                    git_worktree::observe_direct_ref_locked(canonical_repo_dir, &item.source_ref,)?,
                    git_worktree::DirectRefObservation::Missing
                ) && matches!(
                    git_worktree::observe_direct_ref_locked(canonical_repo_dir, target_ref)?,
                    git_worktree::DirectRefObservation::Commit(ref oid)
                        if oid == final_authority.removal_target_oid().as_str()
                ) && git_worktree::other_ref_state_digest_locked(
                    canonical_repo_dir,
                    &item.source_ref,
                )? == unrelated_before)
            })();
            if !matches!(deletion_completed, Ok(true)) {
                let restore = compensate_branch_first_source_ref(
                    canonical_repo_dir,
                    original_root,
                    &quarantine_root,
                    &item.source_ref,
                    &item.source_oid,
                );
                if let Err(error) = restore {
                    tracing::warn!(session_id=%item.session_id, error=%error, "Source-ref compensation after deletion drift was refused");
                }
                if let Err(error) = deletion_completed {
                    tracing::warn!(session_id=%item.session_id, error=%error, "Source-ref deletion postcondition was unreadable");
                }
                store_guard.mark_source_worktree_settlement_recovery_required(
                    run_id,
                    item.session_id,
                    current_phase,
                    SourceWorktreeSettlementRefusalV1::RefDeleteFailed.as_str(),
                    "source deletion did not preserve the exact target and unrelated-ref postconditions",
                )?;
                return Ok(false);
            }

            #[cfg(test)]
            SETTLEMENT_BEFORE_DANGLING_REMOVE_TEST_HOOK.with(|slot| {
                if let Some(hook) = slot.borrow_mut().take() {
                    hook();
                }
            });
            let remove = git_worktree::remove_worktree_after_source_ref_delete_non_force_locked(
                canonical_repo_dir,
                original_root,
                &quarantine_root,
                &item.source_ref,
                &item.source_oid,
            );
            #[cfg(test)]
            SETTLEMENT_AFTER_DANGLING_REMOVE_TEST_HOOK.with(|slot| {
                if let Some(hook) = slot.borrow_mut().take() {
                    hook();
                }
            });
            let removal_completed = worktree_paths_are_absent_and_unregistered(
                canonical_repo_dir,
                original_root,
                &quarantine_root,
            )
            .and_then(|paths_absent| {
                Ok(paths_absent
                    && git_worktree::source_ref_has_zero_registrations_locked(
                        canonical_repo_dir,
                        &item.source_ref,
                    )?
                    && matches!(
                        git_worktree::observe_direct_ref_locked(
                            canonical_repo_dir,
                            &item.source_ref,
                        )?,
                        git_worktree::DirectRefObservation::Missing
                    )
                    && matches!(
                        git_worktree::observe_direct_ref_locked(canonical_repo_dir, target_ref)?,
                        git_worktree::DirectRefObservation::Commit(ref oid)
                            if oid == final_authority.removal_target_oid().as_str()
                    )
                    && git_worktree::other_ref_state_digest_locked(
                        canonical_repo_dir,
                        &item.source_ref,
                    )? == unrelated_before)
            });
            if remove.is_err() || !matches!(removal_completed, Ok(true)) {
                let restore = compensate_branch_first_source_ref(
                    canonical_repo_dir,
                    original_root,
                    &quarantine_root,
                    &item.source_ref,
                    &item.source_oid,
                );
                let source_restored = matches!(
                    git_worktree::observe_direct_ref_locked(
                        canonical_repo_dir,
                        &item.source_ref,
                    ),
                    Ok(git_worktree::DirectRefObservation::Commit(ref oid))
                        if oid == &item.source_oid
                );
                store_guard.mark_source_worktree_settlement_recovery_required(
                    run_id,
                    item.session_id,
                    current_phase,
                    SourceWorktreeSettlementRefusalV1::WorktreeResidue.as_str(),
                    if source_restored {
                        "dangling quarantine removal failed; the exact source ref was restored"
                    } else {
                        "dangling quarantine removal failed and exact source-ref restoration was not proved"
                    },
                )?;
                if let Err(error) = remove {
                    tracing::warn!(session_id=%item.session_id, error=%error, "Non-force dangling quarantine removal retained residue");
                }
                if let Err(error) = restore {
                    tracing::warn!(session_id=%item.session_id, error=%error, "Exact source-ref compensation after removal failure was refused");
                }
                if let Err(error) = removal_completed {
                    tracing::warn!(session_id=%item.session_id, error=%error, "Dangling quarantine removal postcondition was unreadable");
                }
                return Ok(false);
            }
            store_guard.advance_source_worktree_settlement_item(
                run_id,
                item.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementPhaseV1::WorktreeRemoved,
                None,
                Some("canonical quarantine authority consumed branch-first; the source ref, both paths, and all registrations are absent"),
                None,
            )?;
            current_phase = SourceWorktreeSettlementPhaseV1::WorktreeRemoved;
        }
    }

    if matches!(
        current_phase,
        SourceWorktreeSettlementPhaseV1::WorktreeRemoved
            | SourceWorktreeSettlementPhaseV1::BranchRemoved
    ) && authority.is_none()
    {
        match retained_effect_authority(run_id, target_ref, original_root, &quarantine_root, item) {
            Ok(RetainedEffectAuthority::Marker(marker)) => authority = Some(marker),
            Ok(RetainedEffectAuthority::Legacy) => legacy_effect_receipt = true,
            Err(error) => {
                store_guard.mark_source_worktree_settlement_recovery_required(
                    run_id,
                    item.session_id,
                    current_phase,
                    SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                    "effect phase contains no authentic marker or exact legacy receipt",
                )?;
                tracing::warn!(session_id=%item.session_id, error=%error, "Settlement retained effect authority is invalid");
                return Ok(false);
            }
        }
    }

    let effect_phase_drift = matches!(
        current_phase,
        SourceWorktreeSettlementPhaseV1::WorktreeRemoved
            | SourceWorktreeSettlementPhaseV1::BranchRemoved
    ) && {
        durable_tuple_matches = runtime_completed_matches(&completed, item)
            && journal_inventory_matches(&store_guard, run_id, item, original_root)?;
        !durable_tuple_matches
            || runtime_overlaps_settlement_roots(
                &active,
                item.session_id,
                original_root,
                &quarantine_root,
            )
            || !worktree_paths_are_absent_and_unregistered(
                canonical_repo_dir,
                original_root,
                &quarantine_root,
            )?
            || !git_worktree::source_ref_has_zero_registrations_locked(
                canonical_repo_dir,
                &item.source_ref,
            )?
            || !matches!(
                git_worktree::observe_direct_ref_locked(canonical_repo_dir, &item.source_ref)?,
                git_worktree::DirectRefObservation::Missing
            )
    };
    if effect_phase_drift {
        if let Some(marker) = authority.as_ref()
            && let Err(error) = restore_missing_source_ref_from_retained_marker(
                run_id,
                target_ref,
                canonical_repo_dir,
                original_root,
                &quarantine_root,
                item,
                marker,
            )
        {
            store_guard.mark_source_worktree_settlement_recovery_required(
                run_id,
                item.session_id,
                current_phase,
                SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                source_restore_failure_observation(canonical_repo_dir, item),
            )?;
            tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not preserve the source before effect-phase drift terminalization");
            return Ok(false);
        }
        store_guard.mark_source_worktree_settlement_recovery_required(
            run_id,
            item.session_id,
            current_phase,
            SourceWorktreeSettlementRefusalV1::WorktreeResidue.as_str(),
            "original/quarantine path, runtime, custody, registration, or source-ref state drifted after removal",
        )?;
        return Ok(false);
    }

    if current_phase == SourceWorktreeSettlementPhaseV1::WorktreeRemoved {
        let preservation_proved = match authority.as_ref() {
            // The canonical marker is the durable proof that its recorded
            // target object preserved the source before either Git effect.
            // Once both effects and paths are exactly absent, later target
            // movement or object pruning cannot make branch restoration safe.
            Some(_) => Ok(true),
            None if legacy_effect_receipt => observe_preserving_target(
                canonical_repo_dir,
                target_ref,
                &item.repository_identity,
                &item.source_oid,
            )
            .map(|_| true),
            None => Ok(false),
        };
        if !matches!(preservation_proved, Ok(true)) {
            store_guard.mark_source_worktree_settlement_recovery_required(
                run_id,
                item.session_id,
                current_phase,
                SourceWorktreeSettlementRefusalV1::TargetDrift.as_str(),
                "the recorded target object no longer proves preservation after branch-first removal",
            )?;
            return Ok(false);
        }
        store_guard.advance_source_worktree_settlement_item(
            run_id,
            item.session_id,
            SourceWorktreeSettlementPhaseV1::WorktreeRemoved,
            SourceWorktreeSettlementPhaseV1::BranchRemoved,
            None,
            Some("branch-first source deletion and dangling-quarantine removal are both exactly absent"),
            None,
        )?;
        current_phase = SourceWorktreeSettlementPhaseV1::BranchRemoved;
    }

    let branch_phase_drift = current_phase == SourceWorktreeSettlementPhaseV1::BranchRemoved
        && (!worktree_paths_are_absent_and_unregistered(
            canonical_repo_dir,
            original_root,
            &quarantine_root,
        )? || !git_worktree::source_ref_has_zero_registrations_locked(
            canonical_repo_dir,
            &item.source_ref,
        )? || !matches!(
            git_worktree::observe_direct_ref_locked(canonical_repo_dir, &item.source_ref),
            Ok(git_worktree::DirectRefObservation::Missing)
        ));
    if branch_phase_drift {
        if let Some(marker) = authority.as_ref()
            && let Err(error) = restore_missing_source_ref_from_retained_marker(
                run_id,
                target_ref,
                canonical_repo_dir,
                original_root,
                &quarantine_root,
                item,
                marker,
            )
        {
            store_guard.mark_source_worktree_settlement_recovery_required(
                run_id,
                item.session_id,
                current_phase,
                SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                source_restore_failure_observation(canonical_repo_dir, item),
            )?;
            tracing::warn!(session_id=%item.session_id, error=%error, "Settlement branch-first recovery could not preserve the source before finalization-race terminalization");
            return Ok(false);
        }
        store_guard.mark_source_worktree_settlement_recovery_required(
            run_id,
            item.session_id,
            current_phase,
            SourceWorktreeSettlementRefusalV1::ExternalRegistrationRace.as_str(),
            "original/quarantine path, registration, or source ref reappeared before finalization",
        )?;
        return Ok(false);
    }

    if let Err(error) =
        store_guard.finalize_source_worktree_settlement_item(run_id, item.session_id)
    {
        let current_phase = store_guard
            .get_source_worktree_settlement_run(run_id)?
            .and_then(|receipt| {
                receipt
                    .items
                    .into_iter()
                    .find(|current| current.session_id == item.session_id)
            })
            .map(|current| current.phase)
            .ok_or_else(|| {
                DaemonError::Store("settlement item disappeared after finalize".into())
            })?;
        if current_phase == SourceWorktreeSettlementPhaseV1::Settled {
            tracing::warn!(session_id=%item.session_id, error=%error, "Database settlement committed before acknowledgement was lost");
            return Ok(true);
        }
        if current_phase != SourceWorktreeSettlementPhaseV1::BranchRemoved {
            return Err(DaemonError::Store(format!(
                "settlement finalize failed from unexpected phase {current_phase:?}: {error}"
            )));
        }
        if retryable_settlement_database_error(&error) {
            store_guard
                .record_source_worktree_settlement_database_retry(run_id, item.session_id)?;
            tracing::warn!(session_id=%item.session_id, error=%error, "Database settlement remains retryable");
            return Err(error);
        }
        store_guard.mark_source_worktree_settlement_recovery_required(
            run_id,
            item.session_id,
            SourceWorktreeSettlementPhaseV1::BranchRemoved,
            SourceWorktreeSettlementRefusalV1::DatabaseSettlementFailed.as_str(),
            "database settlement failed a permanent Session/custody/item fence",
        )?;
        tracing::warn!(session_id=%item.session_id, error=%error, "Database settlement retained a durable recovery fence");
        return Ok(false);
    }
    Ok(true)
}

const LEGACY_WORKTREE_REMOVAL_OBSERVATIONS: [&str; 2] = [
    "clean exact worktree observed",
    "authorized worktree effect observed after interrupted phase persistence",
];

enum RetainedEffectAuthority {
    Marker(QuarantineRemoveAuthorityV1),
    Legacy,
}

fn retained_effect_authority(
    run_id: Uuid,
    target_ref: &str,
    original_root: &Path,
    quarantine_root: &Path,
    item: &SourceWorktreeSettlementJournalItem,
) -> Result<RetainedEffectAuthority> {
    let Some(raw) = item.before_observation.as_deref() else {
        return Ok(RetainedEffectAuthority::Legacy);
    };
    if let Ok(marker) = QuarantineRemoveAuthorityV1::parse_canonical(raw) {
        if quarantine_authority_matches_retained_item(
            &marker,
            run_id,
            target_ref,
            original_root,
            quarantine_root,
            item,
        ) {
            return Ok(RetainedEffectAuthority::Marker(marker));
        }
        return Err(DaemonError::Store(
            "effect-phase marker does not match its retained item".into(),
        ));
    }
    if LEGACY_WORKTREE_REMOVAL_OBSERVATIONS.contains(&raw) {
        return Ok(RetainedEffectAuthority::Legacy);
    }
    Err(DaemonError::Store(
        "effect phase contains noncanonical removal authority".into(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn quarantine_authority_matches_retained_item(
    marker: &QuarantineRemoveAuthorityV1,
    run_id: Uuid,
    target_ref: &str,
    original_root: &Path,
    quarantine_root: &Path,
    item: &SourceWorktreeSettlementJournalItem,
) -> bool {
    let Some(original) = original_root.to_str() else {
        return false;
    };
    let Some(quarantine) = quarantine_root.to_str() else {
        return false;
    };
    marker.run_id() == run_id
        && marker.session_id() == item.session_id
        && marker.custody_id() == item.custody_id
        && marker.custody_generation() == item.custody_generation
        && marker.original_path_digest()
            == &quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::OriginalPath,
                original,
            )
        && marker.quarantine_path_digest()
            == &quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::QuarantinePath,
                quarantine,
            )
        && marker.repository_identity_digest()
            == &quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::RepositoryIdentity,
                &item.repository_identity,
            )
        && marker.canonical_repo_dir_digest()
            == &quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::CanonicalRepoDir,
                &item.canonical_repo_dir,
            )
        && marker.source_ref_digest()
            == &quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::SourceRef,
                &item.source_ref,
            )
        && marker.target_ref_digest()
            == &quarantine_remove_authority_field_digest(
                QuarantineRemoveAuthorityDigestDomainV1::TargetRef,
                target_ref,
            )
        && marker.source_oid().as_str() == item.source_oid
        && marker.journal_target_oid().as_str() == item.target_oid
        && marker.journal_evidence_digest().as_str() == item.evidence_digest
        && marker.journal_clean_digest().as_str() == item.clean_state_digest
        && marker.removal_clean_digest().as_str() == item.clean_state_digest
}

#[allow(clippy::too_many_arguments)]
fn restore_missing_source_ref_from_retained_marker(
    run_id: Uuid,
    target_ref: &str,
    canonical_repo_dir: &Path,
    original_root: &Path,
    quarantine_root: &Path,
    item: &SourceWorktreeSettlementJournalItem,
    marker: &QuarantineRemoveAuthorityV1,
) -> Result<bool> {
    if !quarantine_authority_matches_retained_item(
        marker,
        run_id,
        target_ref,
        original_root,
        quarantine_root,
        item,
    ) {
        return Err(DaemonError::Store(
            "retained branch-first recovery marker does not match its journal item".into(),
        ));
    }
    if !matches!(
        git_worktree::observe_direct_ref_locked(canonical_repo_dir, &item.source_ref)?,
        git_worktree::DirectRefObservation::Missing
    ) {
        return Ok(false);
    }
    git_worktree::restore_source_ref_if_missing_atomically_locked(
        canonical_repo_dir,
        &item.source_ref,
        &item.source_oid,
    )?;
    if !matches!(
        git_worktree::observe_direct_ref_locked(canonical_repo_dir, &item.source_ref)?,
        git_worktree::DirectRefObservation::Commit(ref oid) if oid == &item.source_oid
    ) {
        return Err(DaemonError::Process(
            "retained marker source restoration did not leave the exact direct commit ref".into(),
        ));
    }
    Ok(true)
}

fn source_restore_failure_observation(
    canonical_repo_dir: &Path,
    item: &SourceWorktreeSettlementJournalItem,
) -> &'static str {
    match git_worktree::observe_direct_ref_locked(canonical_repo_dir, &item.source_ref) {
        Ok(git_worktree::DirectRefObservation::Missing) => {
            "retained marker authorized exact source restoration, but the compare-create was refused and the source remains missing"
        }
        Ok(git_worktree::DirectRefObservation::Commit(ref oid)) if oid == &item.source_oid => {
            "exact source restoration was observed but its required ref-lock acknowledgement was not proved"
        }
        _ => {
            "retained marker authorized exact source restoration, but a different, symbolic, non-commit, or unreadable source state was preserved"
        }
    }
}

// `/proc` inventories can race a descriptor close. Retry one wholly independent
// proof, never partial evidence, while keeping the destructive path bounded.
const QUARANTINE_AUTHORITY_PROOF_MAX_ATTEMPTS: usize = 2;

#[cfg(test)]
thread_local! {
    static SETTLEMENT_BEFORE_DANGLING_REMOVE_TEST_HOOK:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static SETTLEMENT_AFTER_DANGLING_REMOVE_TEST_HOOK:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static QUARANTINE_AUTHORITY_PROOF_FAILURES: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static QUARANTINE_AUTHORITY_PROOF_ATTEMPTS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
fn set_before_dangling_remove_test_hook(hook: impl FnOnce() + 'static) {
    SETTLEMENT_BEFORE_DANGLING_REMOVE_TEST_HOOK.with(|slot| {
        assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
    });
}

#[cfg(test)]
fn set_after_dangling_remove_test_hook(hook: impl FnOnce() + 'static) {
    SETTLEMENT_AFTER_DANGLING_REMOVE_TEST_HOOK.with(|slot| {
        assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
    });
}

#[cfg(test)]
fn fail_next_quarantine_authority_proofs(count: usize) {
    QUARANTINE_AUTHORITY_PROOF_FAILURES.with(|remaining| remaining.set(count));
    QUARANTINE_AUTHORITY_PROOF_ATTEMPTS.with(|attempts| attempts.set(0));
}

#[cfg(test)]
fn quarantine_authority_proof_attempt_count() -> usize {
    QUARANTINE_AUTHORITY_PROOF_ATTEMPTS.with(std::cell::Cell::get)
}

#[allow(clippy::too_many_arguments)]
fn prove_quarantine_remove_authority(
    run_id: Uuid,
    target_ref: &str,
    canonical_repo_dir: &Path,
    original_root: &Path,
    quarantine_root: &Path,
    item: &SourceWorktreeSettlementJournalItem,
    expected_tree: Option<&git_worktree::QuarantineTreeProof>,
) -> Result<(
    QuarantineRemoveAuthorityV1,
    git_worktree::QuarantineTreeProof,
)> {
    let mut last_error = None;
    for _ in 0..QUARANTINE_AUTHORITY_PROOF_MAX_ATTEMPTS {
        match prove_quarantine_remove_authority_once(
            run_id,
            target_ref,
            canonical_repo_dir,
            original_root,
            quarantine_root,
            item,
            expected_tree,
        ) {
            Ok(proof) => return Ok(proof),
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Err(DaemonError::Process(
            "quarantine authority proof attempt bound is zero".into(),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn prove_quarantine_remove_authority_once(
    run_id: Uuid,
    target_ref: &str,
    canonical_repo_dir: &Path,
    original_root: &Path,
    quarantine_root: &Path,
    item: &SourceWorktreeSettlementJournalItem,
    expected_tree: Option<&git_worktree::QuarantineTreeProof>,
) -> Result<(
    QuarantineRemoveAuthorityV1,
    git_worktree::QuarantineTreeProof,
)> {
    #[cfg(test)]
    {
        QUARANTINE_AUTHORITY_PROOF_ATTEMPTS.with(|attempts| {
            attempts.set(attempts.get().saturating_add(1));
        });
        let injected = QUARANTINE_AUTHORITY_PROOF_FAILURES.with(|remaining| {
            let count = remaining.get();
            remaining.set(count.saturating_sub(1));
            count > 0
        });
        if injected {
            return Err(DaemonError::Process(
                "injected complete quarantine authority proof failure".into(),
            ));
        }
    }
    let admin = git_worktree::prove_moved_worktree_exact_locked(
        canonical_repo_dir,
        original_root,
        quarantine_root,
        &item.source_ref,
        &item.source_oid,
    )?;
    if admin.repository_identity.to_str() != Some(item.repository_identity.as_str()) {
        return Err(DaemonError::Process(
            "quarantine Git common-directory identity drifted".into(),
        ));
    }
    let observation = git_worktree::observe_worktree_locked(canonical_repo_dir, quarantine_root)?;
    if observation.root_is_symlink
        || !observation.root_exists
        || !observation.registered
        || !observation.clean
        || observation.clean_state_digest != item.clean_state_digest
        || observation.registered_branch.as_deref() != Some(item.source_ref.as_str())
        || observation.head_ref.as_deref() != Some(item.source_ref.as_str())
        || observation.registered_head.as_deref() != Some(item.source_oid.as_str())
        || observation.head_oid.as_deref() != Some(item.source_oid.as_str())
        || git_worktree::resolve_ref_locked(canonical_repo_dir, &item.source_ref)?.as_deref()
            != Some(item.source_oid.as_str())
        || !git_worktree::source_ref_is_registered_only_at_locked(
            canonical_repo_dir,
            &item.source_ref,
            quarantine_root,
            &item.source_oid,
        )?
        || git_worktree::source_ref_has_symref_dependents_locked(
            canonical_repo_dir,
            &item.source_ref,
        )?
    {
        return Err(DaemonError::Process(
            "quarantine cleanliness, HEAD, source ref, or sole registration drifted".into(),
        ));
    }
    let target = observe_preserving_target(
        canonical_repo_dir,
        target_ref,
        &item.repository_identity,
        &item.source_oid,
    )?;
    let tree = match expected_tree {
        Some(expected) => git_worktree::reprove_quarantine_tree_unchanged(expected)?,
        None => git_worktree::prove_quarantine_tree_safe(quarantine_root)?,
    };
    let holder = super::reaper::prove_quarantine_has_no_untrusted_same_uid_holders(&tree)?;
    let original = original_root
        .to_str()
        .ok_or_else(|| DaemonError::Store("settlement original path is not UTF-8".into()))?;
    let quarantine = quarantine_root
        .to_str()
        .ok_or_else(|| DaemonError::Store("settlement quarantine path is not UTF-8".into()))?;
    let admin_dir = admin
        .admin_directory
        .to_str()
        .ok_or_else(|| DaemonError::Store("settlement Git admin path is not UTF-8".into()))?;
    let marker = QuarantineRemoveAuthorityV1::new(QuarantineRemoveAuthorityFactsV1 {
        run_id,
        session_id: item.session_id,
        custody_id: item.custody_id,
        custody_generation: item.custody_generation,
        original_path: original,
        quarantine_path: quarantine,
        repository_identity: &item.repository_identity,
        canonical_repo_dir: &item.canonical_repo_dir,
        source_ref: &item.source_ref,
        target_ref,
        admin_dir,
        admin_id: &admin.admin_id,
        source_oid: SourceWorktreeGitOidV1::parse(item.source_oid.clone())
            .map_err(DaemonError::Store)?,
        journal_target_oid: SourceWorktreeGitOidV1::parse(item.target_oid.clone())
            .map_err(DaemonError::Store)?,
        removal_target_oid: SourceWorktreeGitOidV1::parse(target.target_oid)
            .map_err(DaemonError::Store)?,
        journal_evidence_digest: Sha256Digest::parse(item.evidence_digest.clone())
            .map_err(DaemonError::Store)?,
        journal_clean_digest: Sha256Digest::parse(item.clean_state_digest.clone())
            .map_err(DaemonError::Store)?,
        removal_clean_digest: Sha256Digest::parse(observation.clean_state_digest)
            .map_err(DaemonError::Store)?,
        root_device: admin.root_identity.device(),
        root_inode: admin.root_identity.inode(),
        stable_tree_digest: Sha256Digest::parse(tree.tree_digest().to_string())
            .map_err(DaemonError::Store)?,
        holder_evidence_digest: Sha256Digest::parse(holder.evidence_digest().to_string())
            .map_err(DaemonError::Store)?,
        holder_fixed_point_passes: holder.fixed_point_passes(),
        trusted_platform_exemption_count: holder.trusted_platform_exemptions(),
        trusted_platform_exemption_digest: Sha256Digest::parse(
            holder.trusted_platform_exemptions_digest().to_string(),
        )
        .map_err(DaemonError::Store)?,
    })?;
    Ok((marker, tree))
}

fn observe_preserving_target(
    canonical_repo_dir: &Path,
    expected_target_ref: &str,
    expected_repository_identity: &str,
    source_oid: &str,
) -> Result<git_worktree::RepositoryTargetObservation> {
    let target = git_worktree::observe_repository_target_locked(canonical_repo_dir)?;
    if target.target_ref != expected_target_ref
        || target.repository_identity != expected_repository_identity
        || !git_worktree::is_ancestor_locked(canonical_repo_dir, source_oid, &target.target_oid)?
    {
        return Err(DaemonError::Process(
            "target ref or ancestry preservation proof drifted".into(),
        ));
    }
    Ok(target)
}

fn nofollow_path_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(DaemonError::Process(format!(
            "settlement path identity is unreadable: {error}"
        ))),
    }
}

fn worktree_paths_are_absent_and_unregistered(
    canonical_repo_dir: &Path,
    original_root: &Path,
    quarantine_root: &Path,
) -> Result<bool> {
    for root in [original_root, quarantine_root] {
        if nofollow_path_exists(root)? {
            return Ok(false);
        }
        if git_worktree::observe_worktree_locked(canonical_repo_dir, root)?.registered {
            return Ok(false);
        }
    }
    Ok(true)
}

fn compensate_branch_first_source_ref(
    canonical_repo_dir: &Path,
    original_root: &Path,
    quarantine_root: &Path,
    source_ref: &str,
    source_oid: &str,
) -> Result<()> {
    git_worktree::restore_source_ref_if_missing_atomically_locked(
        canonical_repo_dir,
        source_ref,
        source_oid,
    )?;
    if !matches!(
        git_worktree::observe_direct_ref_locked(canonical_repo_dir, source_ref)?,
        git_worktree::DirectRefObservation::Commit(ref oid) if oid == source_oid
    ) {
        return Err(DaemonError::Process(
            "branch-first source compensation did not restore the exact direct ref".into(),
        ));
    }
    if nofollow_path_exists(quarantine_root)? {
        return git_worktree::restore_source_ref_and_reattach_detached_quarantine_atomically_locked(
            canonical_repo_dir,
            original_root,
            quarantine_root,
            source_ref,
            source_oid,
        )
        .map(|_| ());
    }
    Ok(())
}

fn runtime_overlaps_settlement_roots(
    active: &HashMap<Uuid, super::types::TrackedSession>,
    owner_session_id: Uuid,
    original_root: &Path,
    quarantine_root: &Path,
) -> bool {
    if active.contains_key(&owner_session_id) {
        return true;
    }
    let canonical_original = std::fs::canonicalize(original_root).ok();
    let canonical_quarantine = std::fs::canonicalize(quarantine_root).ok();
    active.values().any(|tracked| {
        [
            Some(tracked.session.working_dir.as_path()),
            tracked.session.sandbox_root.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|path| {
            let canonical_path = std::fs::canonicalize(path).ok();
            path_is_within_precomputed_root(
                path,
                canonical_path.as_deref(),
                original_root,
                canonical_original.as_deref(),
            ) || path_is_within_precomputed_root(
                path,
                canonical_path.as_deref(),
                quarantine_root,
                canonical_quarantine.as_deref(),
            )
        })
    })
}

#[allow(clippy::too_many_arguments)]
fn stop_item_locked(
    store: &crate::store::Store,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
    phase: SourceWorktreeSettlementPhaseV1,
    code: SourceWorktreeSettlementRefusalV1,
    observation: &str,
    replaying: bool,
    effect_started: bool,
) -> Result<()> {
    if phase == SourceWorktreeSettlementPhaseV1::IntentCommitted && !replaying && !effect_started {
        store.mark_source_worktree_settlement_refused(
            run_id,
            item.session_id,
            phase,
            code.as_str(),
            observation,
        )
    } else {
        store.mark_source_worktree_settlement_recovery_required(
            run_id,
            item.session_id,
            phase,
            code.as_str(),
            observation,
        )
    }
}

fn retryable_settlement_database_error(error: &DaemonError) -> bool {
    matches!(
        error,
        DaemonError::Database(rusqlite::Error::SqliteFailure(failure, _))
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy
                    | rusqlite::ErrorCode::DatabaseLocked
                    | rusqlite::ErrorCode::OperationInterrupted
                    | rusqlite::ErrorCode::SystemIoFailure
                    | rusqlite::ErrorCode::DiskFull
                    | rusqlite::ErrorCode::CannotOpen
                    | rusqlite::ErrorCode::FileLockingProtocolFailed
                    | rusqlite::ErrorCode::SchemaChanged
                    | rusqlite::ErrorCode::OutOfMemory
            )
    ) || matches!(error, DaemonError::Store(message) if message.starts_with("injected source-worktree settlement finalization fault"))
}

fn runtime_completed_matches(
    completed: &HashMap<Uuid, super::types::CompletedSession>,
    item: &SourceWorktreeSettlementJournalItem,
) -> bool {
    completed.get(&item.session_id).is_none_or(|completed| {
        let persisted_updated_at = chrono::DateTime::parse_from_rfc3339(&item.original_updated_at)
            .ok()
            .map(|timestamp| timestamp.with_timezone(&chrono::Utc));
        format!("{:?}", completed.session.status) == item.original_status
            && persisted_updated_at.as_ref() == Some(&completed.session.updated_at)
            && completed.session.sandbox_root.as_deref() == Some(Path::new(&item.sandbox_root))
            && completed.session.sandbox_branch.as_deref() == Some(item.sandbox_branch.as_str())
    })
}

fn journal_inventory_matches(
    store: &crate::store::Store,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
    original_root: &Path,
) -> Result<bool> {
    let aliases = [SourceWorktreeInventoryQuarantineAlias {
        run_id,
        session_id: item.session_id,
        custody_id: item.custody_id,
        custody_generation: item.custody_generation,
        original_path: original_root.to_path_buf(),
    }];
    let inventory = store.source_worktree_inventory_with_path_aliases(
        &item.repository_identity,
        SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
        &aliases,
    )?;
    Ok(inventory.iter().any(|row| {
        row.custody_id == item.custody_id
            && row.owner_session_id == Some(item.session_id)
            && row.session_id == Some(item.session_id)
            && row.generation == item.custody_generation
            && row.validation_state == "verified"
            && row.validated_generation == item.custody_generation
            && row.reserved_effects == item.reserved_effects
            && row.active_effects == item.active_effects
            && row.scheduled_dependency_count == 0
            && row.scheduled_dependency_digest == SOURCE_WORKTREE_EMPTY_DEPENDENCY_DIGEST
            && row.session_path_dependency_count == 0
            && row.session_path_dependency_digest
                == SOURCE_WORKTREE_EMPTY_SESSION_PATH_DEPENDENCY_DIGEST
            && row.participant_count == item.participant_count
            && row.status.as_deref() == Some(item.original_status.as_str())
            && row.session_updated_at.as_deref() == Some(item.original_updated_at.as_str())
            && row.canonical_repo_dir == item.canonical_repo_dir
            && row.repository_identity == item.repository_identity
            && row.sandbox_root == item.sandbox_root
            && row.sandbox_branch == item.sandbox_branch
            && row.session_working_dir.as_deref() == Some(item.canonical_repo_dir.as_str())
            && row.session_sandbox_kind.as_deref() == Some("GitWorktree")
            && row.session_sandbox_root.as_deref() == Some(item.sandbox_root.as_str())
            && row.session_sandbox_branch.as_deref() == Some(item.sandbox_branch.as_str())
            && row.session_cleanup_state.as_deref() == Some("Live")
    }))
}

fn root_is_exact_sandbox(root: &Path, sandbox_base: &Path, session_id: Uuid) -> bool {
    if std::fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return false;
    }
    let Ok(base) = std::fs::canonicalize(sandbox_base) else {
        return false;
    };
    let Ok(root_canonical) = std::fs::canonicalize(root) else {
        return false;
    };
    root == sandbox_base.join(session_id.to_string())
        && root_canonical.parent() == Some(base.as_path())
}

fn mark_item_stopped_blocking(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
    code: SourceWorktreeSettlementRefusalV1,
    observation: &str,
    replaying: bool,
) -> Result<()> {
    let store = store.blocking_lock();
    if item.phase == SourceWorktreeSettlementPhaseV1::IntentCommitted && !replaying {
        store.mark_source_worktree_settlement_refused(
            run_id,
            item.session_id,
            item.phase.clone(),
            code.as_str(),
            observation,
        )
    } else {
        store.mark_source_worktree_settlement_recovery_required(
            run_id,
            item.session_id,
            item.phase.clone(),
            code.as_str(),
            observation,
        )
    }
}

fn mark_source_restore_failure_blocking(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
    observation: &'static str,
) -> Result<()> {
    store
        .blocking_lock()
        .mark_source_worktree_settlement_recovery_required(
            run_id,
            item.session_id,
            item.phase.clone(),
            SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
            observation,
        )
}

fn mark_unattempted_under_root(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
    replaying: bool,
) -> Result<()> {
    let _root_guard = crate::store::sandbox_custody::lock_custody_root(item.custody_id);
    let store = store.try_lock().map_err(|_| {
        DaemonError::Store("Store was contended while closing a later settlement intent".into())
    })?;
    match item.phase {
        SourceWorktreeSettlementPhaseV1::IntentCommitted if !replaying => {
            store.mark_source_worktree_settlement_unattempted(
                run_id,
                item.session_id,
                "not attempted after an earlier item stopped the fresh batch",
            )
        }
        SourceWorktreeSettlementPhaseV1::IntentCommitted => store
            .mark_source_worktree_settlement_recovery_required(
                run_id,
                item.session_id,
                item.phase.clone(),
                SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                "retained intent followed an earlier terminal stop and cannot prove no prior effect",
            ),
        SourceWorktreeSettlementPhaseV1::WorktreeRemoved
        | SourceWorktreeSettlementPhaseV1::BranchRemoved => store
            .mark_source_worktree_settlement_recovery_required(
                run_id,
                item.session_id,
                item.phase.clone(),
                SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                "effect-started item followed an earlier terminal stop in the retained batch",
            ),
        _ => Ok(()),
    }
}

fn hash_bytes(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::parse(format!("sha256:{:x}", Sha256::digest(bytes)))
        .expect("SHA-256 formatter is canonical")
}

fn collect_runtime_path_inputs(
    active: &HashMap<Uuid, super::types::TrackedSession>,
) -> Vec<RuntimePathInput> {
    active
        .values()
        .flat_map(|tracked| {
            let mut paths = vec![RuntimePathInput {
                session_id: tracked.session.id,
                field: "working_dir",
                raw: tracked.session.working_dir.clone(),
            }];
            if let Some(root) = tracked.session.sandbox_root.clone() {
                paths.push(RuntimePathInput {
                    session_id: tracked.session.id,
                    field: "sandbox_root",
                    raw: root,
                });
            }
            paths
        })
        .collect()
}

fn path_is_within_precomputed_root(
    path: &Path,
    canonical_path: Option<&Path>,
    root: &Path,
    canonical_root: Option<&Path>,
) -> bool {
    (path.is_absolute() && root.is_absolute() && path.starts_with(root))
        || canonical_path
            .zip(canonical_root)
            .is_some_and(|(path, root)| path.starts_with(root))
}

fn runtime_dependency_digest(dependencies: &[&RuntimePathObservation]) -> String {
    let mut ordered = dependencies.to_vec();
    ordered.sort_by(|left, right| {
        left.session_id
            .cmp(&right.session_id)
            .then_with(|| left.field.cmp(right.field))
            .then_with(|| left.raw.cmp(&right.raw))
            .then_with(|| left.canonical.cmp(&right.canonical))
    });
    let mut digest = Sha256::new();
    digest.update(b"rsi-source-worktree-runtime-cwd-dependencies-v1\0");
    for dependency in ordered {
        for field in [
            dependency.session_id.to_string(),
            dependency.field.to_string(),
            dependency.raw.to_string_lossy().into_owned(),
            dependency
                .canonical
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ] {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field.as_bytes());
        }
    }
    format!("sha256:{:x}", digest.finalize())
}

fn build_audit(
    repository_identity: &str,
    sandbox_base: &Path,
    inventory: Vec<SourceWorktreeInventoryRow>,
    active_ids: &HashSet<Uuid>,
    active_cwds: &[RuntimePathInput],
) -> Result<AuditSnapshot> {
    let canonical_repo_dir = unique_repository_dir(repository_identity, &inventory)?;
    let origin = PathBuf::from(&canonical_repo_dir);
    git_worktree::with_repository_mutation(&origin, || {
        build_audit_locked(
            repository_identity,
            &canonical_repo_dir,
            sandbox_base,
            inventory,
            active_ids,
            active_cwds,
        )
    })
}

fn build_audit_locked(
    repository_identity: &str,
    canonical_repo_dir: &str,
    sandbox_base: &Path,
    inventory: Vec<SourceWorktreeInventoryRow>,
    active_ids: &HashSet<Uuid>,
    active_cwds: &[RuntimePathInput],
) -> Result<AuditSnapshot> {
    if inventory.len() > SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS {
        return Ok(refused_bound_report(
            repository_identity,
            canonical_repo_dir,
        ));
    }

    let active_cwds = active_cwds
        .iter()
        .map(|path| RuntimePathObservation {
            session_id: path.session_id,
            field: path.field,
            raw: path.raw.clone(),
            canonical: std::fs::canonicalize(&path.raw).ok(),
        })
        .collect::<Vec<_>>();

    let origin = Path::new(canonical_repo_dir);
    let target_result = git_worktree::observe_repository_target_locked(origin);
    let (target_ref, target_oid, mut refusal) = match target_result {
        Ok(target)
            if target.repository_identity == repository_identity
                && target.canonical_repo_dir == origin =>
        {
            (Some(target.target_ref), Some(target.target_oid), None)
        }
        Ok(_) => (
            None,
            None,
            Some("target_ambiguity: repository identity mapping drifted".to_string()),
        ),
        Err(_) => (
            None,
            None,
            Some("target_unavailable: symbolic local HEAD could not be resolved".to_string()),
        ),
    };

    let candidate_target_collision = target_ref.as_ref().is_some_and(|target_ref| {
        inventory.iter().any(|row| {
            row.sandbox_branch
                .strip_prefix("refs/heads/")
                .unwrap_or(&row.sandbox_branch)
                .eq(target_ref.strip_prefix("refs/heads/").unwrap_or(target_ref))
        })
    });
    if candidate_target_collision {
        refusal = Some("target_ambiguity: target is also a sandbox source ref".to_string());
    }

    let canonical_base = std::fs::canonicalize(sandbox_base).ok();
    let mut items = Vec::with_capacity(inventory.len());
    let mut eligible = Vec::new();
    for row in inventory {
        let session_id = row
            .session_id
            .or(row.owner_session_id)
            .unwrap_or(row.custody_id);
        let status = row.status.as_deref().unwrap_or("Unknown");
        let raw_updated_at = row
            .session_updated_at
            .as_deref()
            .unwrap_or("1970-01-01T00:00:00.000000000Z");
        let canonical_updated_at = chrono::DateTime::parse_from_rfc3339(raw_updated_at)
            .ok()
            .map(|timestamp| {
                timestamp
                    .with_timezone(&chrono::Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            });
        let updated_at = canonical_updated_at
            .as_deref()
            .unwrap_or("1970-01-01T00:00:00.000000000Z");
        let session_kind = row.session_kind.as_deref().unwrap_or("Unknown");
        let source_ref = source_ref_for_branch(&row.sandbox_branch);
        let mut source_oid = None;
        let mut clean_state_digest = None;
        let mut proof = SourceWorktreeProofV1::None;
        let mut diagnostic = None;
        let mut git_evidence = CanonicalGitEvidence::default();

        let disposition = classify_inventory_row_locked(
            origin,
            sandbox_base,
            canonical_base.as_deref(),
            &row,
            canonical_updated_at.is_some(),
            session_id,
            active_ids,
            &active_cwds,
            target_ref.as_deref(),
            target_oid.as_deref(),
            refusal.is_some(),
            &source_ref,
            &mut source_oid,
            &mut clean_state_digest,
            &mut proof,
            &mut diagnostic,
            &mut git_evidence,
        );

        let evidence = CanonicalEvidence {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity,
            canonical_repo_dir,
            target_ref: target_ref.as_deref(),
            target_oid: target_oid.as_deref(),
            session_id: session_id.to_string(),
            linked_session_id: row.session_id.map(|id| id.to_string()),
            session_status: status,
            session_updated_at: updated_at,
            session_updated_at_raw: raw_updated_at,
            session_kind,
            session_working_dir: row.session_working_dir.as_deref(),
            session_sandbox_kind: row.session_sandbox_kind.as_deref(),
            session_sandbox_root: row.session_sandbox_root.as_deref(),
            session_sandbox_branch: row.session_sandbox_branch.as_deref(),
            session_cleanup_state: row.session_cleanup_state.as_deref(),
            pending_archive: row.pending_archive,
            custody_id: row.custody_id.to_string(),
            custody_generation: row.generation,
            validated_generation: row.validated_generation,
            validation_state: &row.validation_state,
            owner_session_id: row.owner_session_id.map(|id| id.to_string()),
            participant_count: row.participant_count,
            reserved_effects: row.reserved_effects,
            active_effects: row.active_effects,
            scheduled_dependency_count: row.scheduled_dependency_count,
            scheduled_dependency_digest: &row.scheduled_dependency_digest,
            session_path_dependency_count: row.session_path_dependency_count,
            session_path_dependency_digest: &row.session_path_dependency_digest,
            sandbox_root: &row.sandbox_root,
            sandbox_branch: &row.sandbox_branch,
            source_commit: &row.source_commit,
            source_ref: &source_ref,
            source_oid: source_oid.as_deref(),
            clean_state_digest: clean_state_digest.as_deref(),
            proof: &proof,
            disposition: &disposition,
            git: &git_evidence,
        };
        let evidence_digest = hash_canonical(&evidence)?;
        let source_oid_typed = source_oid
            .as_ref()
            .map(|oid| SourceWorktreeGitOidV1::parse(oid.clone()).map_err(DaemonError::Store))
            .transpose()?;
        let clean_digest_typed = clean_state_digest
            .as_ref()
            .map(|digest| Sha256Digest::parse(digest.clone()).map_err(DaemonError::Store))
            .transpose()?;

        if disposition.is_eligible() {
            eligible.push(EligibleSettlementItem {
                inventory: row.clone(),
                source_ref: source_ref.clone(),
                source_oid: source_oid.clone().expect("eligible source oid"),
                clean_state_digest: clean_state_digest
                    .clone()
                    .expect("eligible clean-state digest"),
                evidence_digest: evidence_digest.as_str().to_string(),
            });
        }
        items.push(SourceWorktreeAuditItemV1 {
            session_id,
            status: status.to_string(),
            updated_at: updated_at.to_string(),
            custody_id: row.custody_id,
            custody_generation: row.generation,
            scheduled_dependency_count: row.scheduled_dependency_count.min(u32::MAX as u64) as u32,
            scheduled_dependency_digest: Sha256Digest::parse(
                row.scheduled_dependency_digest.clone(),
            )
            .map_err(DaemonError::Store)?,
            session_path_dependency_count: row.session_path_dependency_count.min(u32::MAX as u64)
                as u32,
            session_path_dependency_digest: Sha256Digest::parse(
                row.session_path_dependency_digest.clone(),
            )
            .map_err(DaemonError::Store)?,
            sandbox_root: row.sandbox_root,
            source_ref,
            source_oid: source_oid_typed,
            clean_state_digest: clean_digest_typed,
            proof,
            evidence_digest,
            disposition,
            diagnostic,
        });
    }
    items.sort_by_key(|item| item.session_id);
    eligible.sort_by_key(|item| {
        item.inventory
            .session_id
            .unwrap_or(item.inventory.custody_id)
    });

    let plan_digest = hash_canonical(&CanonicalAuditPlan {
        schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
        policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
        repository_identity,
        canonical_repo_dir,
        target_ref: target_ref.as_deref(),
        target_oid: target_oid.as_deref(),
        refusal: refusal.as_deref(),
        items: &items,
    })?;
    let eligible_count = eligible.len() as u32;
    let observed = items.len() as u32;
    let applyable = refusal.is_none() && eligible_count > 0;
    let authorization_phrase =
        applyable.then(|| format!("APPLY {repository_identity} {}", plan_digest.as_str()));
    let report = SourceWorktreeCohortAuditV1 {
        schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
        policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
        repository_identity: repository_identity.to_string(),
        canonical_repo_dir: canonical_repo_dir.to_string(),
        target_ref,
        target_oid: target_oid
            .map(SourceWorktreeGitOidV1::parse)
            .transpose()
            .map_err(DaemonError::Store)?,
        plan_digest,
        authorization_phrase,
        writes: 0,
        applyable,
        counts: SourceWorktreeSettlementCountsV1 {
            observed,
            eligible: eligible_count,
            retained: observed.saturating_sub(eligible_count),
            ..Default::default()
        },
        items,
        run_id: None,
        refusal,
    }
    .validate_wire()
    .map_err(DaemonError::Store)?;
    Ok(AuditSnapshot { report, eligible })
}

#[allow(clippy::too_many_arguments)]
fn classify_inventory_row_locked(
    origin: &Path,
    sandbox_base: &Path,
    canonical_base: Option<&Path>,
    row: &SourceWorktreeInventoryRow,
    timestamp_valid: bool,
    session_id: Uuid,
    active_ids: &HashSet<Uuid>,
    active_cwds: &[RuntimePathObservation],
    target_ref: Option<&str>,
    target_oid: Option<&str>,
    cohort_refused: bool,
    source_ref: &str,
    source_oid: &mut Option<String>,
    clean_state_digest: &mut Option<String>,
    proof: &mut SourceWorktreeProofV1,
    diagnostic: &mut Option<String>,
    git_evidence: &mut CanonicalGitEvidence,
) -> SourceWorktreeDispositionV1 {
    use SourceWorktreeDispositionV1 as D;

    let root = Path::new(&row.sandbox_root);
    let canonical_root = std::fs::canonicalize(root).ok();
    let runtime_dependencies = active_cwds
        .iter()
        .filter(|dependency| {
            path_is_within_precomputed_root(
                &dependency.raw,
                dependency.canonical.as_deref(),
                root,
                canonical_root.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    git_evidence.runtime_dependency_count =
        runtime_dependencies.len().min(u32::MAX as usize) as u32;
    git_evidence.runtime_dependency_digest = runtime_dependency_digest(&runtime_dependencies);
    git_evidence.runtime_active =
        active_ids.contains(&session_id) || !runtime_dependencies.is_empty();
    if git_evidence.runtime_active {
        if !runtime_dependencies.is_empty() {
            *diagnostic =
                Some("an active provider effective cwd overlaps this sandbox root".into());
        }
        return D::ActiveOwner;
    }
    if !timestamp_valid {
        return D::CustodyUnverified;
    }
    if !matches!(
        row.status.as_deref(),
        Some("Completed" | "Failed" | "Interrupted" | "Archived")
    ) {
        return D::NonterminalStatus;
    }
    if !matches!(
        row.session_kind.as_deref(),
        Some(
            "Standard"
                | "TaskRabbit"
                | "Bug"
                | "Story"
                | "Task"
                | "Feature"
                | "Refactor"
                | "Research"
        )
    ) {
        return D::NonterminalStatus;
    }
    if row.owner_session_id != Some(session_id) || row.session_id != Some(session_id) {
        return D::CustodyUnverified;
    }
    if row.validation_state != "verified" {
        return D::CustodyUnverified;
    }
    if row.generation == 0 || row.validated_generation != row.generation {
        return D::CustodyGenerationDrift;
    }
    if row.participant_count != 1 {
        return D::SharedParticipant;
    }
    if row.reserved_effects != 0 {
        return D::ReservedEffect;
    }
    if row.active_effects != 0 {
        return D::ActiveEffect;
    }
    if row.scheduled_dependency_count != 0 {
        if row.scheduled_dependency_count == u64::MAX {
            *diagnostic = Some(
                "scheduled dependency enumeration exceeded its bounded scan; candidate retained"
                    .into(),
            );
        }
        return D::ScheduledDependency;
    }
    if row.session_path_dependency_count != 0 {
        if row.session_path_dependency_count == u64::MAX {
            *diagnostic = Some(
                "Session path dependency enumeration exceeded its bounded scan; candidate retained"
                    .into(),
            );
        }
        return D::SessionPathDependency;
    }
    if row.repository_identity.is_empty()
        || row.canonical_repo_dir != origin.to_string_lossy()
        || row.session_working_dir.as_deref() != Some(row.canonical_repo_dir.as_str())
        || row.session_sandbox_kind.as_deref() != Some("GitWorktree")
        || row.session_sandbox_root.as_deref() != Some(row.sandbox_root.as_str())
        || row.session_sandbox_branch.as_deref() != Some(row.sandbox_branch.as_str())
        || row.session_cleanup_state.as_deref() != Some("Live")
    {
        return D::CustodyUnverified;
    }
    if !valid_source_branch(&row.sandbox_branch) {
        return D::InvalidSourceRef;
    }
    if matches!(row.sandbox_branch.as_str(), "main" | "rolling") || target_ref == Some(source_ref) {
        return D::ProtectedRef;
    }
    let Ok(metadata) = std::fs::symlink_metadata(root) else {
        git_evidence.root_exists = Some(false);
        return D::MissingRoot;
    };
    git_evidence.root_exists = Some(true);
    git_evidence.root_is_symlink = Some(metadata.file_type().is_symlink());
    if metadata.file_type().is_symlink() {
        return D::SymlinkIdentity;
    }
    let expected_root = sandbox_base.join(session_id.to_string());
    git_evidence.expected_root_match = Some(root == expected_root);
    git_evidence.canonical_base = canonical_base.map(|path| path.to_string_lossy().into_owned());
    git_evidence.canonical_root = canonical_root
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    git_evidence.canonical_identity_match = Some(
        root == expected_root
            && canonical_base.is_some()
            && canonical_root
                .as_deref()
                .zip(canonical_base)
                .is_some_and(|(root, base)| root.starts_with(base) && root.parent() == Some(base)),
    );
    if root != expected_root
        || canonical_base.is_none()
        || canonical_root
            .as_deref()
            .zip(canonical_base)
            .is_none_or(|(root, base)| !root.starts_with(base) || root.parent() != Some(base))
    {
        return D::OutsideSandboxBase;
    }
    let observation = match git_worktree::observe_worktree_locked(origin, root) {
        Ok(observation) => observation,
        Err(_) => {
            git_evidence.observation_outcome = "error";
            *diagnostic = Some("worktree observation failed".into());
            return D::WorktreeUnregistered;
        }
    };
    git_evidence.observation_outcome = "observed";
    git_evidence.root_exists = Some(observation.root_exists);
    git_evidence.root_is_symlink = Some(observation.root_is_symlink);
    git_evidence.registered = Some(observation.registered);
    git_evidence.registered_head = observation.registered_head.clone();
    git_evidence.registered_branch = observation.registered_branch.clone();
    git_evidence.head_oid = observation.head_oid.clone();
    git_evidence.head_ref = observation.head_ref.clone();
    git_evidence.clean = Some(observation.clean);
    git_evidence.clean_state_digest = Some(observation.clean_state_digest.clone());
    *clean_state_digest = Some(observation.clean_state_digest.clone());
    if observation.root_is_symlink {
        return D::SymlinkIdentity;
    }
    if !observation.registered {
        return D::WorktreeUnregistered;
    }
    let resolved_source = match git_worktree::observe_direct_ref_locked(origin, source_ref) {
        Ok(git_worktree::DirectRefObservation::Commit(oid)) => {
            git_evidence.source_resolution_outcome = "direct_commit";
            oid
        }
        Ok(git_worktree::DirectRefObservation::Missing) => {
            git_evidence.source_resolution_outcome = "missing";
            return D::MissingSourceRef;
        }
        Ok(git_worktree::DirectRefObservation::Symbolic) => {
            git_evidence.source_resolution_outcome = "symbolic";
            return D::MissingSourceRef;
        }
        Ok(git_worktree::DirectRefObservation::NonCommit) => {
            git_evidence.source_resolution_outcome = "non_commit";
            return D::MissingSourceRef;
        }
        Err(_) => {
            git_evidence.source_resolution_outcome = "error";
            return D::MissingSourceRef;
        }
    };
    if observation.registered_branch.as_deref() != Some(source_ref)
        || observation.head_ref.as_deref() != Some(source_ref)
    {
        return D::SourceRefMismatch;
    }
    *source_oid = Some(resolved_source.clone());
    match git_worktree::other_ref_state_digest_locked(origin, source_ref) {
        Ok(digest) => git_evidence.unrelated_ref_digest = Some(digest),
        Err(_) => {
            *diagnostic = Some("unrelated ref snapshot failed".into());
            return D::TargetUnavailable;
        }
    }
    match git_worktree::source_ref_has_symref_dependents_locked(origin, source_ref) {
        Ok(has_dependents) => {
            git_evidence.source_has_symref_dependents = Some(has_dependents);
            if has_dependents {
                *diagnostic = Some("another symbolic ref depends on the source ref".into());
                return D::SourceRefMismatch;
            }
        }
        Err(_) => {
            *diagnostic = Some("symbolic ref dependency observation failed".into());
            return D::TargetUnavailable;
        }
    }
    if SourceWorktreeGitOidV1::parse(resolved_source.clone()).is_err() {
        return D::SourceRefMismatch;
    }
    if observation.registered_head.as_deref() != Some(resolved_source.as_str())
        || observation.head_oid.as_deref() != Some(resolved_source.as_str())
    {
        return D::HeadMismatch;
    }
    if !observation.clean {
        return D::DirtyWorktree;
    }
    if cohort_refused || target_oid.is_none() {
        return D::TargetUnavailable;
    }
    match git_worktree::is_ancestor_locked(origin, &resolved_source, target_oid.unwrap()) {
        Ok(true) => {
            *proof = SourceWorktreeProofV1::IntegratedAncestor;
            D::EligibleIntegratedAncestor
        }
        Ok(false) => D::RetainedNonAncestor,
        Err(_) => D::TargetUnavailable,
    }
}

fn unique_repository_dir(
    repository_identity: &str,
    inventory: &[SourceWorktreeInventoryRow],
) -> Result<String> {
    if inventory.is_empty() {
        return Err(DaemonError::InvalidParam(
            "repository cohort no longer has Live custody roots".into(),
        ));
    }
    let dirs = inventory
        .iter()
        .map(|row| row.canonical_repo_dir.as_str())
        .collect::<BTreeSet<_>>();
    if dirs.len() != 1
        || inventory
            .iter()
            .any(|row| row.repository_identity != repository_identity)
    {
        return Err(DaemonError::InvalidParam(
            "repository identity maps to inconsistent custody paths".into(),
        ));
    }
    Ok((*dirs.first().expect("one canonical path")).to_string())
}

fn durable_receipt_only_audit(
    receipt: &SourceWorktreeSettlementRunV1,
) -> Result<SourceWorktreeCohortAuditV1> {
    let refusal = "repository cohort has no Live custody roots; showing its latest durable receipt"
        .to_string();
    let plan_digest = hash_canonical(&CanonicalAuditPlan {
        schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
        policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
        repository_identity: &receipt.repository_identity,
        canonical_repo_dir: &receipt.canonical_repo_dir,
        target_ref: None,
        target_oid: None,
        refusal: Some(&refusal),
        items: &[],
    })?;
    SourceWorktreeCohortAuditV1 {
        schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
        policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
        repository_identity: receipt.repository_identity.clone(),
        canonical_repo_dir: receipt.canonical_repo_dir.clone(),
        target_ref: None,
        target_oid: None,
        plan_digest,
        authorization_phrase: None,
        writes: 0,
        applyable: false,
        counts: SourceWorktreeSettlementCountsV1::default(),
        items: Vec::new(),
        run_id: Some(receipt.run_id),
        refusal: Some(refusal),
    }
    .validate_wire()
    .map_err(DaemonError::Store)
}

fn refused_bound_report(repository_identity: &str, canonical_repo_dir: &str) -> AuditSnapshot {
    let refusal = "bound_exceeded: cohort has more than 256 Live roots".to_string();
    let plan_digest = hash_canonical(&CanonicalAuditPlan {
        schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
        policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
        repository_identity,
        canonical_repo_dir,
        target_ref: None,
        target_oid: None,
        refusal: Some(&refusal),
        items: &[],
    })
    .expect("bounded canonical refusal");
    AuditSnapshot {
        report: SourceWorktreeCohortAuditV1 {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: repository_identity.to_string(),
            canonical_repo_dir: canonical_repo_dir.to_string(),
            target_ref: None,
            target_oid: None,
            plan_digest,
            authorization_phrase: None,
            writes: 0,
            applyable: false,
            counts: SourceWorktreeSettlementCountsV1::default(),
            items: Vec::new(),
            run_id: None,
            refusal: Some(refusal),
        },
        eligible: Vec::new(),
    }
}

fn source_ref_for_branch(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{branch}")
    }
}

fn valid_source_branch(branch: &str) -> bool {
    validate_source_ref(&source_ref_for_branch(branch)).is_ok()
}

fn hash_canonical(value: &impl Serialize) -> Result<Sha256Digest> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| DaemonError::Store(format!("canonical settlement JSON: {error}")))?;
    Sha256Digest::parse(format!("sha256:{:x}", Sha256::digest(bytes))).map_err(DaemonError::Store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
    use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionStatus};
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run fixture Git command");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("Git fixture output is UTF-8")
            .trim()
            .to_string()
    }

    fn create_private_sandbox_base(path: &Path) {
        std::fs::create_dir_all(path).expect("sandbox base directory");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base permissions");
    }

    fn startup_owned_sleep(session_id: Uuid) -> crate::session::reaper::StartupReaperChild {
        let mut command = Command::new("sleep");
        command
            // Fixed-point aggregate tests can outlive the former 30-second child;
            // StartupReaperChild still guarantees kill-and-wait cleanup on every path.
            .arg("300")
            .env_remove(rsi_common::identity::ENV_SESSION_ID)
            .env_remove(rsi_common::identity::ENV_MODEL_INVOCATION_ID)
            .env_remove(rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE)
            .env_remove(rsi_common::identity::ENV_SOCKET)
            .env(rsi_common::identity::ENV_SESSION_ID, session_id.to_string())
            .env(
                rsi_common::identity::ENV_SOCKET,
                rsi_common::identity::default_socket_path(),
            );
        let child = command.spawn().expect("spawn owned startup sleep");
        let child = crate::session::reaper::StartupReaperChild::unregistered(child);
        let expected = format!("{}={session_id}", rsi_common::identity::ENV_SESSION_ID);
        child.wait_for_environment(&expected);
        child
    }

    struct IntegratedFixture {
        _directory: tempfile::TempDir,
        quarantine_holder_proc: crate::session::reaper::SyntheticQuarantineHolderProc,
        repository: PathBuf,
        sandbox_base: PathBuf,
        allocation: crate::sandbox::SandboxAllocation,
        target: git_worktree::RepositoryTargetObservation,
        store: crate::store::Store,
        session_id: Uuid,
        custody_id: Uuid,
        branch: String,
        source_oid: String,
    }

    impl IntegratedFixture {
        fn new(label: &str) -> Self {
            let directory = tempfile::tempdir().expect("settlement fixture directory");
            let quarantine_holder_proc =
                crate::session::reaper::SyntheticQuarantineHolderProc::new();
            let repository = directory.path().join("repository");
            let sandbox_base = directory.path().join("sandboxes");
            std::fs::create_dir_all(&repository).expect("repository directory");
            create_private_sandbox_base(&sandbox_base);
            git(&repository, &["init", "-q", "-b", "main"]);
            git(
                &repository,
                &["config", "user.email", "fixture@example.test"],
            );
            git(&repository, &["config", "user.name", "Settlement Fixture"]);
            std::fs::write(repository.join("tracked"), "base\n").expect("base file");
            git(&repository, &["add", "tracked"]);
            git(&repository, &["commit", "-qm", "base"]);
            let source_oid = git(&repository, &["rev-parse", "HEAD"]);
            let session_id = Uuid::new_v4();
            let custody_id = Uuid::new_v4();
            let branch = format!("rsi/{label}/{session_id}");
            let allocation = git_worktree::allocate(
                &sandbox_base,
                session_id,
                &repository,
                &source_oid,
                Some(&branch),
            )
            .expect("allocate candidate worktree");
            std::fs::write(repository.join("tracked"), "base\nintegrated\n")
                .expect("target integration file");
            git(&repository, &["add", "tracked"]);
            git(&repository, &["commit", "-qm", "integrate source"]);
            let target = git_worktree::with_repository_mutation(&repository, || {
                git_worktree::observe_repository_target_locked(&repository)
            })
            .expect("observe repository target");
            let mut store = crate::store::Store::open_in_memory().expect("settlement Store");
            let mut session = crate::store::tests::make_test_session();
            session.id = session_id;
            session.status = SessionStatus::Completed;
            session.working_dir = target.canonical_repo_dir.clone();
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(allocation.root.clone());
            session.sandbox_branch = Some(branch.clone());
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            store
                .insert_session_with_custody(
                    &session,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id,
                        canonical_repo_dir: target
                            .canonical_repo_dir
                            .to_string_lossy()
                            .into_owned(),
                        sandbox_root: allocation.root.to_string_lossy().into_owned(),
                        sandbox_branch: branch.clone(),
                        repository_identity: target.repository_identity.clone(),
                        source_commit: source_oid.clone(),
                        cause: CustodyCause::FreshLaunch,
                    }),
                )
                .expect("seed custody-backed terminal Session");
            Self {
                _directory: directory,
                quarantine_holder_proc,
                repository,
                sandbox_base,
                allocation,
                target,
                store,
                session_id,
                custody_id,
                branch,
                source_oid,
            }
        }

        fn with_quarantine_holder_test_proc<T>(&self, body: impl FnOnce() -> T) -> T {
            crate::session::reaper::with_quarantine_holder_test_proc(
                self.quarantine_holder_proc.proc_root(),
                self.quarantine_holder_proc.uid(),
                body,
            )
        }

        fn inventory(&self) -> Vec<SourceWorktreeInventoryRow> {
            self.store
                .source_worktree_inventory(
                    &self.target.repository_identity,
                    SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
                )
                .expect("load settlement fixture inventory")
        }

        fn audit(&self) -> AuditSnapshot {
            let inventory = self.inventory();
            git_worktree::with_repository_mutation(&self.repository, || {
                build_audit_locked(
                    &self.target.repository_identity,
                    self.target
                        .canonical_repo_dir
                        .to_str()
                        .expect("UTF-8 canonical repository"),
                    &self.sandbox_base,
                    inventory,
                    &HashSet::new(),
                    &[],
                )
            })
            .expect("audit integrated settlement fixture")
        }

        fn insert_synthetic_custody(
            &mut self,
            label: &str,
            repository_identity: String,
            canonical_repo_dir: String,
        ) -> Uuid {
            let session_id = Uuid::new_v4();
            let custody_id = Uuid::new_v4();
            let root = self.sandbox_base.join(session_id.to_string());
            let branch = format!("rsi/{label}/{session_id}");
            let mut session = crate::store::tests::make_test_session();
            session.id = session_id;
            session.status = SessionStatus::Completed;
            session.working_dir = PathBuf::from(&canonical_repo_dir);
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(root.clone());
            session.sandbox_branch = Some(branch.clone());
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            self.store
                .insert_session_with_custody(
                    &session,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id,
                        canonical_repo_dir,
                        sandbox_root: root.to_string_lossy().into_owned(),
                        sandbox_branch: branch,
                        repository_identity,
                        source_commit: self.source_oid.clone(),
                        cause: CustodyCause::FreshLaunch,
                    }),
                )
                .expect("insert synthetic custody root");
            session_id
        }

        fn run_from_audit(&self, audit: &AuditSnapshot, key: &str) -> NewSettlementRun {
            let eligible = audit.eligible.first().expect("eligible fixture item");
            NewSettlementRun {
                run_id: Uuid::new_v4(),
                repository_identity: self.target.repository_identity.clone(),
                canonical_repo_dir: self
                    .target
                    .canonical_repo_dir
                    .to_string_lossy()
                    .into_owned(),
                target_ref: audit.report.target_ref.clone().expect("target ref"),
                target_oid: audit
                    .report
                    .target_oid
                    .as_ref()
                    .expect("target oid")
                    .as_str()
                    .to_string(),
                plan_digest: audit.report.plan_digest.as_str().to_string(),
                idempotency_key: key.into(),
                authorization_digest: hash_bytes(
                    audit
                        .report
                        .authorization_phrase
                        .as_deref()
                        .expect("authorization phrase")
                        .as_bytes(),
                )
                .as_str()
                .to_string(),
                request_fingerprint: hash_bytes(key.as_bytes()).as_str().to_string(),
                observed_count: audit.report.counts.observed,
                retained_count: audit.report.counts.retained,
                items: vec![NewSettlementItem {
                    session_id: self.session_id,
                    original_status: eligible.inventory.status.clone().expect("status"),
                    original_updated_at: eligible
                        .inventory
                        .session_updated_at
                        .clone()
                        .expect("updated_at"),
                    custody_id: self.custody_id,
                    custody_generation: eligible.inventory.generation,
                    canonical_repo_dir: eligible.inventory.canonical_repo_dir.clone(),
                    sandbox_root: eligible.inventory.sandbox_root.clone(),
                    sandbox_branch: eligible.inventory.sandbox_branch.clone(),
                    repository_identity: eligible.inventory.repository_identity.clone(),
                    source_ref: eligible.source_ref.clone(),
                    source_oid: eligible.source_oid.clone(),
                    target_oid: audit
                        .report
                        .target_oid
                        .as_ref()
                        .expect("target oid")
                        .as_str()
                        .to_string(),
                    evidence_digest: eligible.evidence_digest.clone(),
                    clean_state_digest: eligible.clean_state_digest.clone(),
                    reserved_effects: eligible.inventory.reserved_effects,
                    active_effects: eligible.inventory.active_effects,
                    participant_count: eligible.inventory.participant_count,
                }],
            }
        }

        fn commit_intent(&mut self, key: &str) -> NewSettlementRun {
            let audit = self.audit();
            let run = self.run_from_audit(&audit, key);
            assert!(matches!(
                self.store
                    .insert_source_worktree_settlement_run(&run)
                    .expect("commit fixture settlement intent"),
                InsertSettlementRunOutcome::Inserted
            ));
            run
        }

        fn journal_item(&self, run_id: Uuid) -> SourceWorktreeSettlementJournalItem {
            self.store
                .list_source_worktree_settlement_journal_items(run_id)
                .expect("load fixture settlement item")
                .into_iter()
                .next()
                .expect("fixture settlement item")
        }

        fn quarantine_root(&self, run_id: Uuid) -> PathBuf {
            git_worktree::derive_settlement_quarantine_path(
                &self.allocation.root,
                run_id,
                self.session_id,
            )
            .expect("derive fixture quarantine")
        }

        fn move_to_quarantine(&self, run: &NewSettlementRun) -> PathBuf {
            let quarantine_root = self.quarantine_root(run.run_id);
            git_worktree::with_repository_mutation(&self.repository, || {
                let proof = git_worktree::prepare_settlement_quarantine_path(
                    &self.allocation.root,
                    run.run_id,
                    self.session_id,
                )?;
                git_worktree::move_worktree_to_quarantine_non_force_locked(
                    &self.repository,
                    &proof,
                    &run.items[0].source_ref,
                    &run.items[0].source_oid,
                )?;
                Ok(())
            })
            .expect("move fixture worktree to quarantine");
            quarantine_root
        }

        fn build_quarantine_marker(
            &self,
            run: &NewSettlementRun,
            quarantine_root: &Path,
        ) -> QuarantineRemoveAuthorityV1 {
            let item = self.journal_item(run.run_id);
            self.with_quarantine_holder_test_proc(|| {
                git_worktree::with_repository_mutation(&self.repository, || {
                    prove_quarantine_remove_authority(
                        run.run_id,
                        &run.target_ref,
                        &self.target.canonical_repo_dir,
                        &self.allocation.root,
                        quarantine_root,
                        &item,
                        None,
                    )
                    .map(|(marker, _)| marker)
                })
            })
            .expect("build fixture quarantine marker")
        }

        fn persist_quarantine_marker(
            &mut self,
            run: &NewSettlementRun,
            quarantine_root: &Path,
        ) -> QuarantineRemoveAuthorityV1 {
            let marker = self.build_quarantine_marker(run, quarantine_root);
            self.store
                .record_source_worktree_quarantine_remove_authority(
                    run.run_id,
                    self.session_id,
                    &marker,
                )
                .expect("persist fixture quarantine marker");
            marker
        }

        fn remove_quarantine(&self, run: &NewSettlementRun, quarantine_root: &Path) {
            git_worktree::with_repository_mutation(&self.repository, || {
                git_worktree::remove_worktree_non_force_locked(
                    &self.repository,
                    &self.allocation.root,
                    quarantine_root,
                    &run.items[0].source_ref,
                    &run.items[0].source_oid,
                )
            })
            .expect("remove fixture quarantine");
        }

        fn remove_quarantine_after_source_delete(
            &self,
            run: &NewSettlementRun,
            quarantine_root: &Path,
        ) {
            git_worktree::with_repository_mutation(&self.repository, || {
                git_worktree::remove_worktree_after_source_ref_delete_non_force_locked(
                    &self.repository,
                    &self.allocation.root,
                    quarantine_root,
                    &run.items[0].source_ref,
                    &run.items[0].source_oid,
                )
            })
            .expect("remove fixture dangling quarantine");
        }

        fn delete_source_ref(&self, run: &NewSettlementRun) {
            git_worktree::with_repository_mutation(&self.repository, || {
                git_worktree::delete_source_ref_atomically_locked(
                    &self.repository,
                    &run.target_ref,
                    &run.target_oid,
                    &run.items[0].source_ref,
                    &run.items[0].source_oid,
                )
            })
            .expect("delete fixture source ref");
        }

        fn detach_quarantine_at_source(&self, run: &NewSettlementRun, quarantine_root: &Path) {
            git(
                quarantine_root,
                &["update-ref", "--no-deref", "HEAD", &run.items[0].source_oid],
            );
        }

        fn advance_to_worktree_removed(&self, run: &NewSettlementRun, before: Option<&str>) {
            self.store
                .advance_source_worktree_settlement_item(
                    run.run_id,
                    self.session_id,
                    SourceWorktreeSettlementPhaseV1::IntentCommitted,
                    SourceWorktreeSettlementPhaseV1::WorktreeRemoved,
                    before,
                    Some("fixture crash after worktree removal"),
                    None,
                )
                .expect("advance fixture to worktree removed");
        }

        fn advance_to_branch_removed(&self, run: &NewSettlementRun) {
            self.store
                .advance_source_worktree_settlement_item(
                    run.run_id,
                    self.session_id,
                    SourceWorktreeSettlementPhaseV1::WorktreeRemoved,
                    SourceWorktreeSettlementPhaseV1::BranchRemoved,
                    None,
                    Some("fixture crash after first branch-first phase CAS"),
                    None,
                )
                .expect("advance fixture to branch removed");
        }

        fn replay_run(
            &mut self,
            run_id: Uuid,
        ) -> (ApplyOutcome, Arc<tokio::sync::Mutex<crate::store::Store>>) {
            let receipt = self
                .store
                .get_source_worktree_settlement_run(run_id)
                .expect("read fixture replay receipt")
                .expect("fixture replay receipt");
            let idempotency_key = receipt.idempotency_key.clone();
            let plan_digest = receipt.plan_digest.as_str().to_string();
            let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
                &mut self.store,
                crate::store::Store::open_in_memory().expect("replacement fixture Store"),
            )));
            let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let outcome = self
                .with_quarantine_holder_test_proc(|| {
                    git_worktree::with_repository_mutation(&self.repository, || {
                        apply_locked(
                            ApplyStart::Replay(receipt),
                            &self.target.repository_identity,
                            &idempotency_key,
                            &plan_digest,
                            "",
                            "",
                            &self.sandbox_base,
                            &store,
                            &active,
                            &completed,
                        )
                    })
                })
                .expect("replay fixture settlement");
            (outcome, store)
        }
    }

    #[test]
    fn source_ref_policy_accepts_only_rsi_local_branches() {
        assert!(valid_source_branch("rsi/506dc38e"));
        assert!(valid_source_branch("refs/heads/rsi/506dc38e"));
        for value in [
            "main",
            "rolling",
            "refs/remotes/origin/rsi/x",
            "rsi/../main",
            "rsi//x",
            "rsi/.x",
            "rsi/x.lock",
            "rsi/x@{1}",
        ] {
            assert!(!valid_source_branch(value), "{value}");
        }
    }

    #[test]
    fn no_live_roots_audit_exposes_latest_durable_receipt_without_apply_authority() {
        let mut fixture = IntegratedFixture::new("receipt-only-audit");
        let run = fixture.commit_intent("receipt-only-audit");
        let receipt = fixture
            .store
            .get_source_worktree_settlement_run(run.run_id)
            .expect("read durable receipt")
            .expect("durable receipt");

        let audit = durable_receipt_only_audit(&receipt).expect("receipt-only audit");
        assert_eq!(audit.run_id, Some(run.run_id));
        assert_eq!(
            audit.repository_identity,
            fixture.target.repository_identity
        );
        assert_eq!(audit.canonical_repo_dir, receipt.canonical_repo_dir);
        assert_eq!(audit.counts, SourceWorktreeSettlementCountsV1::default());
        assert!(!audit.applyable);
        assert!(audit.authorization_phrase.is_none());
        assert!(audit.items.is_empty());
        assert!(
            audit
                .refusal
                .as_deref()
                .is_some_and(|value| value.contains("no Live custody roots"))
        );
    }

    #[tokio::test]
    async fn zero_live_receipt_recovery_does_not_require_repository_path() {
        let mut fixture = IntegratedFixture::new("missing-repository-receipt-only");
        let run = fixture.commit_intent("missing-repository-receipt-only");
        fixture
            .store
            .tombstone_custody_root(fixture.custody_id, 1, CustodyCause::Purge)
            .expect("remove Live custody from receipt-only fixture");
        std::fs::remove_dir_all(&fixture.repository).expect("remove canonical repository path");
        let repository_identity = fixture.target.repository_identity.clone();
        let socket_path = fixture._directory.path().join("daemon.sock");
        let sandbox_base = fixture.sandbox_base.clone();
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            fixture.store,
            false,
            socket_path,
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
            sandbox_base,
        )
        .expect("construct receipt-only manager");

        let audit = manager
            .audit_source_worktree_cohort(repository_identity)
            .await
            .expect("recover receipt without repository path");
        assert_eq!(audit.run_id, Some(run.run_id));
        assert!(!audit.applyable);
        assert!(audit.items.is_empty());
    }

    #[test]
    fn live_unverified_inventory_is_retained_instead_of_failing_to_decode() {
        let fixture = IntegratedFixture::new("live-unverified-inventory");
        fixture
            .store
            .conn
            .execute(
                "UPDATE sandbox_custody_roots
                    SET validation_state='unverified',validated_generation=NULL,
                        validated_at=NULL,validation_error_code=NULL
                  WHERE custody_id=?1",
                [fixture.custody_id.to_string()],
            )
            .expect("mark live root unverified");
        let inventory = fixture.inventory();
        assert_eq!(inventory[0].validated_generation, 0);
        let audit = git_worktree::with_repository_mutation(&fixture.repository, || {
            build_audit_locked(
                &fixture.target.repository_identity,
                fixture
                    .target
                    .canonical_repo_dir
                    .to_str()
                    .expect("UTF-8 repository"),
                &fixture.sandbox_base,
                inventory,
                &HashSet::new(),
                &[],
            )
        })
        .expect("audit unverified inventory");
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::CustodyUnverified
        );
        audit
            .report
            .validate_wire()
            .expect("retained unverified inventory is wire-valid");
    }

    #[test]
    fn canonical_hash_is_stable_and_domain_separated() {
        #[derive(Serialize)]
        struct Value<'a> {
            schema_version: u32,
            value: &'a str,
        }
        let first = hash_canonical(&Value {
            schema_version: 1,
            value: "one",
        })
        .unwrap();
        let repeated = hash_canonical(&Value {
            schema_version: 1,
            value: "one",
        })
        .unwrap();
        let changed = hash_canonical(&Value {
            schema_version: 2,
            value: "one",
        })
        .unwrap();
        assert_eq!(first, repeated);
        assert_ne!(first, changed);
    }

    #[test]
    fn enabled_scheduled_dependencies_retain_every_watch_direction() {
        let fixture = IntegratedFixture::new("scheduled-dependencies");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let descendant = fixture.allocation.root.join("nested/dispatch");
        std::fs::create_dir_all(&descendant).expect("scheduled descendant fixture");
        let alias = fixture.sandbox_base.join("scheduled-alias");
        std::os::unix::fs::symlink(&descendant, &alias).expect("scheduled symlink descendant");
        let rows = [
            (
                "resume",
                "resume".to_string(),
                Some(fixture.session_id.to_string()),
                None,
            ),
            (
                "program-guard",
                "resume".to_string(),
                Some(fixture.session_id.to_string()),
                None,
            ),
            (
                "watched-subject",
                format!("on_terminal:{}", fixture.session_id),
                Some(Uuid::new_v4().to_string()),
                None,
            ),
            (
                "sandbox-working-dir",
                "fresh".to_string(),
                None,
                Some(format!("{}/.", fixture.allocation.root.to_string_lossy())),
            ),
            (
                "sandbox-descendant",
                "fresh".to_string(),
                None,
                Some(descendant.to_string_lossy().into_owned()),
            ),
            (
                "sandbox-symlink-descendant",
                "fresh".to_string(),
                None,
                Some(alias.to_string_lossy().into_owned()),
            ),
        ];
        for (name, wake_mode, wake_session_id, working_dir) in rows {
            fixture
                .store
                .conn
                .execute(
                    "INSERT INTO scheduled_jobs (
                        id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,
                        working_dir,provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id
                     ) VALUES (?1,?2,'dependency','{}',NULL,?3,1,?4,NULL,NULL,NULL,?3,?3,?5,?6)",
                    rusqlite::params![
                        Uuid::new_v4().to_string(),
                        name,
                        now,
                        working_dir,
                        wake_mode,
                        wake_session_id,
                    ],
                )
                .expect("insert enabled scheduled dependency");
        }

        let audit = fixture.audit();
        assert!(!audit.report.applyable);
        assert_eq!(audit.report.authorization_phrase, None);
        assert_eq!(audit.report.counts.eligible, 0);
        assert_eq!(audit.report.items[0].scheduled_dependency_count, 6);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::ScheduledDependency
        );
        assert!(fixture.allocation.root.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &format!("refs/heads/{}", fixture.branch)]
            ),
            fixture.source_oid
        );
    }

    #[test]
    fn scheduled_dependency_scan_streams_past_ten_thousand_unrelated_rows() {
        let mut fixture = IntegratedFixture::new("scheduled-dependency-cardinality");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let outside = fixture
            .sandbox_base
            .parent()
            .expect("fixture parent")
            .join("unrelated-scheduled-root");
        std::fs::create_dir_all(&outside).expect("unrelated scheduled root");
        let outside = outside.to_string_lossy().into_owned();
        let transaction = fixture
            .store
            .conn
            .transaction()
            .expect("scheduled cardinality transaction");
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO scheduled_jobs (
                        id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,
                        working_dir,provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id
                     ) VALUES (?1,?2,'unrelated','{}',NULL,?3,1,?4,NULL,NULL,NULL,?3,?3,'fresh',NULL)",
                )
                .expect("prepare scheduled cardinality insert");
            for index in 1..=10_001_u128 {
                insert
                    .execute(rusqlite::params![
                        Uuid::from_u128(index).to_string(),
                        format!("unrelated-{index}"),
                        &now,
                        &outside,
                    ])
                    .expect("insert unrelated scheduled job");
            }
        }
        transaction
            .commit()
            .expect("commit scheduled cardinality rows");

        let unrelated = fixture.audit();
        assert!(unrelated.report.applyable);
        assert_eq!(
            unrelated.report.items[0].disposition,
            SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        );
        assert_eq!(unrelated.report.items[0].scheduled_dependency_count, 0);

        fixture
            .store
            .conn
            .execute(
                "INSERT INTO scheduled_jobs (
                    id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,
                    working_dir,provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id
                 ) VALUES (?1,'matching-tail','matching','{}',NULL,?2,1,?3,NULL,NULL,NULL,?2,?2,'fresh',NULL)",
                rusqlite::params![
                    Uuid::from_u128(u128::MAX).to_string(),
                    now,
                    fixture.allocation.root.to_string_lossy().into_owned(),
                ],
            )
            .expect("insert scheduled dependency beyond former boundary");
        let matching_tail = fixture.audit();
        assert!(!matching_tail.report.applyable);
        assert_eq!(
            matching_tail.report.items[0].disposition,
            SourceWorktreeDispositionV1::ScheduledDependency
        );
        assert_eq!(matching_tail.report.items[0].scheduled_dependency_count, 1);
    }

    #[test]
    fn scheduled_dependency_scan_fails_closed_at_its_total_work_bound() {
        let mut fixture = IntegratedFixture::new("scheduled-dependency-total-bound");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let outside = fixture
            .sandbox_base
            .parent()
            .expect("fixture parent")
            .join("scheduled-bound-outside")
            .to_string_lossy()
            .into_owned();
        let transaction = fixture
            .store
            .conn
            .transaction()
            .expect("scheduled bound transaction");
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO scheduled_jobs (
                        id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,
                        working_dir,provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id
                     ) VALUES (?1,?2,'bounded','{}',NULL,?3,1,?4,NULL,NULL,NULL,?3,?3,'fresh',NULL)",
                )
                .expect("prepare scheduled bound insert");
            for index in
                1..=crate::store::cohort_settlement::SETTLEMENT_DEPENDENCY_SCAN_MAX_RECORDS + 1
            {
                insert
                    .execute(rusqlite::params![
                        Uuid::from_u128(index as u128).to_string(),
                        format!("bounded-{index}"),
                        &now,
                        &outside,
                    ])
                    .expect("insert scheduled bound row");
            }
        }
        transaction.commit().expect("commit scheduled bound rows");

        let audit = fixture.audit();
        assert!(!audit.report.applyable);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::ScheduledDependency
        );
        assert_eq!(audit.report.items[0].scheduled_dependency_count, u32::MAX);
        assert!(
            audit.report.items[0]
                .diagnostic
                .as_deref()
                .is_some_and(|value| value.contains("bounded scan"))
        );
    }

    #[test]
    fn scheduled_dependency_scan_observes_invalid_initial_keys_and_owner_ids() {
        for case in ["empty", "null", "uppercase-owner"] {
            let fixture = IntegratedFixture::new(&format!("scheduled-invalid-key-{case}"));
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            let job_id = match case {
                "empty" => Some(String::new()),
                "null" => None,
                "uppercase-owner" => Some(Uuid::new_v4().to_string()),
                _ => unreachable!(),
            };
            let wake_owner = if case == "uppercase-owner" {
                fixture.session_id.to_string().to_uppercase()
            } else {
                fixture.session_id.to_string()
            };
            fixture
                .store
                .conn
                .execute(
                    "INSERT INTO scheduled_jobs (
                        id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,
                        working_dir,provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id
                     ) VALUES (?1,'invalid-key','invalid-key','{}',NULL,?2,1,NULL,NULL,NULL,NULL,?2,?2,'resume',?3)",
                    rusqlite::params![job_id, now, wake_owner],
                )
                .expect("insert hostile scheduled dependency key");

            let audit = fixture.audit();
            assert!(!audit.report.applyable, "{case}");
            assert_eq!(
                audit.report.items[0].scheduled_dependency_count,
                u32::MAX,
                "{case} must fail the scheduled dependency scan closed"
            );
        }
    }

    #[test]
    fn list_allows_257_cohorts_while_audit_refuses_257_roots_explicitly() {
        let mut list_fixture = IntegratedFixture::new("list-257");
        for index in 0..256 {
            list_fixture.insert_synthetic_custody(
                &format!("list-{index}"),
                format!("repo:list-257:{index}"),
                format!("/tmp/list-257-repository-{index}"),
            );
        }
        let summaries = list_fixture
            .store
            .list_source_worktree_cohorts()
            .expect("list 257 bounded cohort summaries");
        assert_eq!(summaries.len(), 257);
        for summary in summaries {
            summary.validate_wire().expect("wire-valid cohort summary");
        }

        let mut audit_fixture = IntegratedFixture::new("audit-257");
        let repository_identity = audit_fixture.target.repository_identity.clone();
        let canonical_repo_dir = audit_fixture
            .target
            .canonical_repo_dir
            .to_string_lossy()
            .into_owned();
        for index in 0..256 {
            audit_fixture.insert_synthetic_custody(
                &format!("audit-{index}"),
                repository_identity.clone(),
                canonical_repo_dir.clone(),
            );
        }
        let audit = audit_fixture.audit();
        assert!(!audit.report.applyable);
        assert_eq!(audit.report.items.len(), 0);
        assert_eq!(
            audit.report.refusal.as_deref(),
            Some("bound_exceeded: cohort has more than 256 Live roots")
        );
    }

    #[test]
    fn runtime_dependency_evidence_checks_working_dir_and_sandbox_root_independently() {
        let fixture = IntegratedFixture::new("runtime-crossed-fields");
        let inventory = fixture.inventory();
        let outside = fixture
            .sandbox_base
            .parent()
            .expect("fixture parent")
            .join("outside");
        std::fs::create_dir_all(&outside).expect("outside runtime path");
        let inside = fixture.allocation.root.join("nested-runtime");
        std::fs::create_dir_all(&inside).expect("inside runtime path");
        let active_id = Uuid::new_v4();

        let audit = git_worktree::with_repository_mutation(&fixture.repository, || {
            build_audit_locked(
                &fixture.target.repository_identity,
                fixture
                    .target
                    .canonical_repo_dir
                    .to_str()
                    .expect("UTF-8 repository"),
                &fixture.sandbox_base,
                inventory.clone(),
                &HashSet::new(),
                &[
                    RuntimePathInput {
                        session_id: active_id,
                        field: "working_dir",
                        raw: outside.clone(),
                    },
                    RuntimePathInput {
                        session_id: active_id,
                        field: "sandbox_root",
                        raw: inside.clone(),
                    },
                ],
            )
        })
        .expect("audit sandbox-root dependency");
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::ActiveOwner
        );
        let sandbox_digest = audit.report.items[0].evidence_digest.clone();

        let audit = git_worktree::with_repository_mutation(&fixture.repository, || {
            build_audit_locked(
                &fixture.target.repository_identity,
                fixture
                    .target
                    .canonical_repo_dir
                    .to_str()
                    .expect("UTF-8 repository"),
                &fixture.sandbox_base,
                inventory,
                &HashSet::new(),
                &[
                    RuntimePathInput {
                        session_id: active_id,
                        field: "working_dir",
                        raw: inside,
                    },
                    RuntimePathInput {
                        session_id: active_id,
                        field: "sandbox_root",
                        raw: outside,
                    },
                ],
            )
        })
        .expect("audit working-dir dependency");
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::ActiveOwner
        );
        assert_ne!(sandbox_digest, audit.report.items[0].evidence_digest);
    }

    #[test]
    fn durable_session_dependency_checks_crossed_path_fields_independently() {
        let fixture = IntegratedFixture::new("durable-crossed-fields");
        let alias_id = Uuid::new_v4();
        let inside = fixture.allocation.root.join("durable-alias");
        let outside = fixture
            .sandbox_base
            .parent()
            .expect("fixture parent")
            .join("outside-alias");
        std::fs::create_dir_all(&inside).expect("inside durable alias path");
        std::fs::create_dir_all(&outside).expect("outside durable alias path");
        let mut alias = crate::store::tests::make_test_session();
        alias.id = alias_id;
        alias.status = SessionStatus::Completed;
        alias.working_dir = inside.clone();
        fixture
            .store
            .insert_session(&alias)
            .expect("insert ordinary crossed-field alias");
        let verified_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET freshness='verified',effective_cwd=canonical_repo_dir,
                        validated_at=?1,error_code=NULL,updated_at=?1
                  WHERE session_id=?2",
                rusqlite::params![&verified_at, alias_id.to_string()],
            )
            .expect("publish ordinary crossed-field execution authority");
        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET sandbox_root=?1 WHERE id=?2",
                rusqlite::params![outside.to_string_lossy().into_owned(), alias_id.to_string()],
            )
            .expect("install outside sandbox-root spelling without changing ordinary projection");

        let working_dir_match = fixture.audit();
        assert_eq!(
            working_dir_match.report.items[0].disposition,
            SourceWorktreeDispositionV1::SessionPathDependency
        );
        assert_eq!(
            working_dir_match.report.items[0].session_path_dependency_count,
            1
        );

        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET working_dir=?1,sandbox_root=?2 WHERE id=?3",
                rusqlite::params![
                    outside.to_string_lossy().into_owned(),
                    inside.to_string_lossy().into_owned(),
                    alias_id.to_string(),
                ],
            )
            .expect("swap durable crossed-field alias");
        let sandbox_root_match = fixture.audit();
        assert_eq!(
            sandbox_root_match.report.items[0].disposition,
            SourceWorktreeDispositionV1::SessionPathDependency
        );
        assert_eq!(
            sandbox_root_match.report.items[0].session_path_dependency_count,
            1
        );
        assert_ne!(
            working_dir_match.report.items[0].session_path_dependency_digest,
            sandbox_root_match.report.items[0].session_path_dependency_digest
        );
        assert_ne!(
            working_dir_match.report.items[0].evidence_digest,
            sandbox_root_match.report.items[0].evidence_digest
        );

        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET working_dir=?1,sandbox_root=?1 WHERE id=?2",
                rusqlite::params![outside.to_string_lossy().into_owned(), alias_id.to_string()],
            )
            .expect("move both Session path fields outside candidate");
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections SET effective_cwd=?1,updated_at=?2
                  WHERE session_id=?3",
                rusqlite::params![
                    inside.to_string_lossy().into_owned(),
                    &verified_at,
                    alias_id.to_string(),
                ],
            )
            .expect("install projection-only candidate dependency");
        let projection_match = fixture.audit();
        assert_eq!(
            projection_match.report.items[0].disposition,
            SourceWorktreeDispositionV1::SessionPathDependency
        );
        assert_eq!(
            projection_match.report.items[0].session_path_dependency_count,
            1
        );
        assert_ne!(
            sandbox_root_match.report.items[0].session_path_dependency_digest,
            projection_match.report.items[0].session_path_dependency_digest
        );

        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET execution_state='historical_transferred',freshness='verified',
                        effective_cwd=NULL,custody_id=NULL,custody_generation=NULL,
                        validated_at=?1,error_code=NULL,updated_at=?1
                  WHERE session_id=?2",
                rusqlite::params![verified_at, alias_id.to_string()],
            )
            .expect("install malformed rootless transferred projection");
        let transferred = fixture.audit();
        assert!(!transferred.report.applyable);
        assert_eq!(
            transferred.report.items[0].session_path_dependency_count,
            u32::MAX,
            "a near-miss inert projection tuple must fail the bounded scan closed"
        );
    }

    #[test]
    fn durable_live_projection_effective_cwd_retains_candidate() {
        let mut fixture = IntegratedFixture::new("durable-live-projection-cwd");
        let inside = fixture.allocation.root.join("live-projection-provider");
        std::fs::create_dir_all(&inside).expect("create live projection cwd");
        let live_id = fixture.insert_synthetic_custody(
            "foreign-live-projection",
            "repo:foreign-live-projection".into(),
            "/tmp/foreign-live-projection-repository".into(),
        );
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET effective_cwd=?1,freshness='verified',validated_at=?2,
                        error_code=NULL,updated_at=?2
                  WHERE session_id=?3",
                rusqlite::params![
                    inside.to_string_lossy().into_owned(),
                    now,
                    live_id.to_string(),
                ],
            )
            .expect("install live projection-only candidate dependency");

        let audit = fixture.audit();
        assert!(!audit.report.applyable);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::SessionPathDependency
        );
        assert_eq!(audit.report.items[0].session_path_dependency_count, 1);
    }

    #[test]
    fn malformed_session_kind_and_custody_only_projection_fail_closed() {
        let mut fixture = IntegratedFixture::new("malformed-session-dependency");
        let outside = fixture
            .sandbox_base
            .parent()
            .expect("fixture parent")
            .join("malformed-session-outside");
        std::fs::create_dir_all(&outside).expect("create outside path");
        let alias_id = Uuid::new_v4();
        let mut alias = crate::store::tests::make_test_session();
        alias.id = alias_id;
        alias.status = SessionStatus::Completed;
        alias.working_dir = outside;
        fixture
            .store
            .insert_session(&alias)
            .expect("insert ordinary alias");
        fixture
            .store
            .publish_startup_ordinary(alias_id)
            .expect("publish ordinary alias");

        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET session_kind='HostileLeaf' WHERE id=?1",
                [alias_id.to_string()],
            )
            .expect("install unknown leaf kind");
        let unknown_kind = fixture.audit();
        assert!(!unknown_kind.report.applyable);
        assert_eq!(
            unknown_kind.report.items[0].session_path_dependency_count,
            u32::MAX,
            "an unknown or NULL leaf kind must remain visible and fail closed"
        );

        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET session_kind='Standard' WHERE id=?1",
                [alias_id.to_string()],
            )
            .expect("restore known leaf kind");
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET custody_id=?1,custody_generation=1
                  WHERE session_id=?2",
                rusqlite::params![fixture.custody_id.to_string(), alias_id.to_string()],
            )
            .expect("install custody-only projection alias");
        let custody_only = fixture.audit();
        assert!(!custody_only.report.applyable);
        assert_eq!(
            custody_only.report.items[0].disposition,
            SourceWorktreeDispositionV1::SessionPathDependency
        );
        assert_eq!(
            custody_only.report.items[0].session_path_dependency_count,
            1
        );

        fixture
            .store
            .conn
            .execute_batch("PRAGMA foreign_keys=OFF;")
            .expect("open uppercase custody fixture seam");
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET custody_id=?1
                  WHERE session_id=?2",
                rusqlite::params![
                    fixture.custody_id.to_string().to_uppercase(),
                    alias_id.to_string()
                ],
            )
            .expect("install uppercase custody-only projection alias");
        fixture
            .store
            .conn
            .execute_batch("PRAGMA foreign_keys=ON;")
            .expect("close uppercase custody fixture seam");
        let uppercase_custody = fixture.audit();
        assert!(!uppercase_custody.report.applyable);
        assert_eq!(
            uppercase_custody.report.items[0].session_path_dependency_count,
            u32::MAX,
            "noncanonical custody identity must fail the dependency scan closed"
        );
    }

    #[test]
    fn terminal_cleanup_failed_and_quarantined_projections_are_inert_dependencies() {
        use rsi_common::types::SandboxCustodyErrorCodeV1;

        let mut fixture = IntegratedFixture::new("terminal-inert-projections");
        let inside = fixture.allocation.root.join("historical-provider-cwd");
        std::fs::create_dir_all(&inside).expect("create historical provider cwd");

        let mut rootless_failed = crate::store::tests::make_test_session();
        rootless_failed.id = Uuid::new_v4();
        rootless_failed.status = SessionStatus::Failed;
        rootless_failed.working_dir = inside.clone();
        rootless_failed.sandbox_kind = Some(SandboxKind::GitWorktree);
        rootless_failed.sandbox_cleanup_state = Some(SandboxCleanupState::Failed);
        fixture
            .store
            .insert_session(&rootless_failed)
            .expect("insert rootless cleanup-failed history");
        fixture
            .store
            .publish_startup_rootless_failed(rootless_failed.id)
            .expect("publish verified rootless cleanup-failed projection");

        let mut rootless_purged = crate::store::tests::make_test_session();
        rootless_purged.id = Uuid::new_v4();
        rootless_purged.status = SessionStatus::Completed;
        rootless_purged.working_dir = inside.clone();
        rootless_purged.sandbox_kind = Some(SandboxKind::GitWorktree);
        rootless_purged.sandbox_cleanup_state = Some(SandboxCleanupState::Purged);
        fixture
            .store
            .insert_session(&rootless_purged)
            .expect("insert rootless purged history");
        fixture
            .store
            .publish_startup_rootless_purged(rootless_purged.id)
            .expect("publish verified rootless purged projection");

        let quarantined_id = fixture.insert_synthetic_custody(
            "quarantined-history",
            "repo:quarantined-history".into(),
            inside.to_string_lossy().into_owned(),
        );
        let quarantined_custody = fixture
            .store
            .conn
            .query_row(
                "SELECT sandbox_custody_id FROM sessions WHERE id=?1",
                [quarantined_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map(|value| Uuid::parse_str(&value).expect("canonical custody id"))
            .expect("load quarantined custody id");
        fixture
            .store
            .record_custody_cleanup_failure(
                quarantined_custody,
                1,
                SandboxCustodyErrorCodeV1::CleanupFailed,
            )
            .expect("publish cleanup failure before quarantine");
        fixture
            .store
            .quarantine_startup_terminal_root(
                quarantined_custody,
                SandboxCustodyErrorCodeV1::RootIdentityMismatch,
            )
            .expect("quarantine ownerless failed aggregate");
        fixture
            .store
            .publish_startup_terminal_root(quarantined_custody)
            .expect("publish quarantined projection");

        let audit = fixture.audit();
        assert!(audit.report.applyable);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        );
        assert_eq!(audit.report.items[0].session_path_dependency_count, 0);

        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET status='Running' WHERE id=?1",
                [rootless_purged.id.to_string()],
            )
            .expect("install active-like Purged near miss");
        let active_purged = fixture.audit();
        assert_eq!(
            active_purged.report.items[0].disposition,
            SourceWorktreeDispositionV1::SessionPathDependency,
            "a Purged projection paired with an active-like Session remains a dependency"
        );
        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [rootless_purged.id.to_string()],
            )
            .expect("restore terminal Purged status");

        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET custody_id=?1,custody_generation=1
                  WHERE session_id=?2",
                rusqlite::params![
                    fixture.custody_id.to_string(),
                    rootless_failed.id.to_string(),
                ],
            )
            .expect("install malformed cleanup-failed projection shape");
        let malformed_failed = fixture.audit();
        assert!(!malformed_failed.report.applyable);
        assert_eq!(
            malformed_failed.report.items[0].session_path_dependency_count,
            u32::MAX,
            "malformed recognized state must fail the bounded scan closed"
        );
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET custody_id=NULL,custody_generation=NULL
                  WHERE session_id=?1",
                [rootless_failed.id.to_string()],
            )
            .expect("restore cleanup-failed projection shape");

        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET custody_id=?1,custody_generation=1
                  WHERE session_id=?2",
                rusqlite::params![fixture.custody_id.to_string(), quarantined_id.to_string(),],
            )
            .expect("install malformed quarantined projection shape");
        let malformed_quarantined = fixture.audit();
        assert!(!malformed_quarantined.report.applyable);
        assert_eq!(
            malformed_quarantined.report.items[0].session_path_dependency_count,
            u32::MAX,
            "malformed quarantined state must fail the bounded scan closed"
        );
    }

    #[test]
    fn durable_session_scan_streams_past_ten_thousand_unrelated_rows() {
        let mut fixture = IntegratedFixture::new("session-dependency-cardinality");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let outside = fixture
            .sandbox_base
            .parent()
            .expect("fixture parent")
            .join("unrelated-session-root");
        std::fs::create_dir_all(&outside).expect("unrelated Session root");
        let outside = outside.to_string_lossy().into_owned();
        let transaction = fixture
            .store
            .conn
            .transaction()
            .expect("Session cardinality transaction");
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at)
                     VALUES (?1,'unrelated cardinality fixture',?2,'Completed',?3,?3)",
                )
                .expect("prepare Session cardinality insert");
            for index in 1..=10_001_u128 {
                insert
                    .execute(rusqlite::params![
                        Uuid::from_u128(index).to_string(),
                        &outside,
                        &now,
                    ])
                    .expect("insert unrelated Session row");
            }
        }
        transaction
            .commit()
            .expect("commit Session cardinality rows");

        let unrelated = fixture.audit();
        assert!(unrelated.report.applyable);
        assert_eq!(
            unrelated.report.items[0].disposition,
            SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        );
        assert_eq!(unrelated.report.items[0].session_path_dependency_count, 0);

        let matching_id = Uuid::from_u128(u128::MAX);
        fixture
            .store
            .conn
            .execute(
                "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at)
                 VALUES (?1,'matching tail fixture',?2,'Completed',?3,?3)",
                rusqlite::params![
                    matching_id.to_string(),
                    fixture.allocation.root.to_string_lossy().into_owned(),
                    &now,
                ],
            )
            .expect("insert Session dependency beyond former boundary");
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET freshness='verified',effective_cwd=canonical_repo_dir,
                        validated_at=?1,error_code=NULL,updated_at=?1
                  WHERE session_id=?2",
                rusqlite::params![now, matching_id.to_string()],
            )
            .expect("publish matching tail execution authority");

        let matching_tail = fixture.audit();
        assert!(!matching_tail.report.applyable);
        assert_eq!(
            matching_tail.report.items[0].disposition,
            SourceWorktreeDispositionV1::SessionPathDependency
        );
        assert_eq!(
            matching_tail.report.items[0].session_path_dependency_count,
            1
        );
    }

    #[test]
    fn durable_session_scan_fails_closed_at_its_total_work_bound() {
        let mut fixture = IntegratedFixture::new("session-dependency-total-bound");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let outside = fixture
            .sandbox_base
            .parent()
            .expect("fixture parent")
            .join("session-bound-outside")
            .to_string_lossy()
            .into_owned();
        let transaction = fixture
            .store
            .conn
            .transaction()
            .expect("Session bound transaction");
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at)
                     VALUES (?1,'bounded Session dependency',?2,'Completed',?3,?3)",
                )
                .expect("prepare Session bound insert");
            for index in
                1..=crate::store::cohort_settlement::SETTLEMENT_DEPENDENCY_SCAN_MAX_RECORDS + 1
            {
                insert
                    .execute(rusqlite::params![
                        Uuid::from_u128(index as u128).to_string(),
                        &outside,
                        &now,
                    ])
                    .expect("insert Session bound row");
            }
        }
        transaction.commit().expect("commit Session bound rows");

        let audit = fixture.audit();
        assert!(!audit.report.applyable);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::SessionPathDependency
        );
        assert_eq!(
            audit.report.items[0].session_path_dependency_count,
            u32::MAX
        );
        assert!(
            audit.report.items[0]
                .diagnostic
                .as_deref()
                .is_some_and(|value| value.contains("bounded scan"))
        );
    }

    #[test]
    fn session_keyset_scans_observe_empty_and_null_legacy_ids() {
        for case in ["empty", "null"] {
            let mut fixture = IntegratedFixture::new(&format!("invalid-session-key-{case}"));
            let baseline = fixture.audit();
            let run = fixture.run_from_audit(&baseline, &format!("invalid-session-key-{case}"));
            fixture
                .store
                .insert_source_worktree_settlement_run(&run)
                .expect("insert startup journal before hostile Session key");

            let alias_id = Uuid::new_v4();
            let mut alias = crate::store::tests::make_test_session();
            alias.id = alias_id;
            alias.status = SessionStatus::Starting;
            alias.working_dir = fixture
                .sandbox_base
                .parent()
                .expect("fixture parent")
                .join(format!("invalid-session-key-outside-{case}"));
            fixture
                .store
                .insert_session(&alias)
                .expect("insert Session before corrupting its key");

            fixture
                .store
                .conn
                .execute_batch("PRAGMA foreign_keys=OFF;")
                .expect("open hostile Session key seam");
            let hostile_id = (case == "empty").then(String::new);
            fixture
                .store
                .conn
                .execute(
                    "UPDATE sessions SET id=?1 WHERE id=?2",
                    rusqlite::params![hostile_id, alias_id.to_string()],
                )
                .expect("install hostile Session key");
            fixture
                .store
                .conn
                .execute_batch("PRAGMA foreign_keys=ON;")
                .expect("close hostile Session key seam");

            let audit = fixture.audit();
            assert!(!audit.report.applyable, "{case}");
            assert_eq!(
                audit.report.items[0].session_path_dependency_count,
                u32::MAX,
                "{case} key must fail the durable dependency scan closed"
            );
            let error = fixture
                .store
                .source_worktree_startup_orphan_candidates(&fixture.sandbox_base)
                .expect_err("invalid Session key must block startup process authority");
            assert!(
                error.to_string().contains("Session")
                    || error.to_string().contains("storage class"),
                "{error}"
            );
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &format!("refs/heads/{}", fixture.branch)]
                ),
                fixture.source_oid
            );
        }
    }

    #[test]
    fn durable_journal_path_fence_binds_raw_canonical_descendants_and_controls() {
        let mut fixture = IntegratedFixture::new("journal-path-admission");
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "journal-path-admission");
        let run_id = run.run_id;
        fixture
            .store
            .insert_source_worktree_settlement_run(&run)
            .expect("insert durable settlement intent");
        let descendant = fixture.allocation.root.join("future/dispatch");
        assert!(
            fixture
                .store
                .path_is_inside_settlement_root(&descendant)
                .expect("query descendant journal fence")
        );
        let alias = fixture.sandbox_base.join("journal-admission-alias");
        std::os::unix::fs::symlink(&fixture.allocation.root, &alias)
            .expect("journal admission symlink");
        assert!(
            fixture
                .store
                .path_is_inside_settlement_root(&alias.join("future"))
                .expect("query canonical journal fence")
        );

        fixture
            .store
            .mark_source_worktree_settlement_refused(
                run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementRefusalV1::AuditDrift.as_str(),
                "proved pre-effect refusal",
            )
            .expect("close intent as a V95 no-effect refusal");
        assert!(
            !fixture
                .store
                .path_is_inside_settlement_root(&descendant)
                .expect("query refused control")
        );
    }

    #[test]
    fn startup_orphan_candidates_cover_owner_and_every_path_identity() {
        let mut fixture = IntegratedFixture::new("startup-orphan-candidates");
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "startup-orphan-candidates");
        fixture
            .store
            .insert_source_worktree_settlement_run(&run)
            .expect("insert startup candidate journal");

        let outside = fixture
            .sandbox_base
            .parent()
            .expect("fixture parent")
            .join("startup-orphan-outside");
        std::fs::create_dir_all(&outside).expect("startup orphan outside path");
        let descendant = fixture.allocation.root.join("nested/provider");
        std::fs::create_dir_all(&descendant).expect("startup orphan descendant path");
        let symlink = fixture.sandbox_base.join("startup-orphan-symlink");
        std::os::unix::fs::symlink(&descendant, &symlink).expect("startup orphan symlink path");

        let exact_id = Uuid::new_v4();
        let mut exact = crate::store::tests::make_test_session();
        exact.id = exact_id;
        exact.status = SessionStatus::Starting;
        exact.working_dir = fixture.allocation.root.clone();
        fixture
            .store
            .insert_session(&exact)
            .expect("insert exact startup alias");

        let sandbox_id = Uuid::new_v4();
        let mut sandbox_alias = crate::store::tests::make_test_session();
        sandbox_alias.id = sandbox_id;
        sandbox_alias.status = SessionStatus::Running;
        sandbox_alias.working_dir = outside.clone();
        fixture
            .store
            .insert_session(&sandbox_alias)
            .expect("insert sandbox-root startup alias");
        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET sandbox_root=?1 WHERE id=?2",
                rusqlite::params![
                    descendant.to_string_lossy().into_owned(),
                    sandbox_id.to_string(),
                ],
            )
            .expect("bind sandbox-root startup alias");

        let projection_id = Uuid::new_v4();
        let mut projection_alias = crate::store::tests::make_test_session();
        projection_alias.id = projection_id;
        projection_alias.status = SessionStatus::Starting;
        projection_alias.working_dir = outside.clone();
        fixture
            .store
            .insert_session(&projection_alias)
            .expect("insert projection startup alias");
        let verified_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        fixture
            .store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET freshness='verified',effective_cwd=?1,validated_at=?2,
                        error_code=NULL,updated_at=?2
                  WHERE session_id=?3",
                rusqlite::params![
                    symlink.to_string_lossy().into_owned(),
                    verified_at,
                    projection_id.to_string(),
                ],
            )
            .expect("bind canonical projection startup alias");

        let unrelated_id = Uuid::new_v4();
        let mut unrelated = crate::store::tests::make_test_session();
        unrelated.id = unrelated_id;
        unrelated.status = SessionStatus::Starting;
        unrelated.working_dir = outside;
        fixture
            .store
            .insert_session(&unrelated)
            .expect("insert unrelated startup Session");

        let candidates = fixture
            .store
            .source_worktree_startup_orphan_candidates(&fixture.sandbox_base)
            .expect("resolve bounded startup orphan candidates");
        for expected in [fixture.session_id, exact_id, sandbox_id, projection_id] {
            assert!(
                candidates.contains(&expected),
                "startup candidate set omitted {expected}"
            );
        }
        assert!(!candidates.contains(&unrelated_id));
    }

    #[test]
    fn startup_orphan_candidates_reject_owner_root_mismatch() {
        let mut fixture = IntegratedFixture::new("startup-orphan-owner-mismatch");
        let audit = fixture.audit();
        let mut run = fixture.run_from_audit(&audit, "startup-orphan-owner-mismatch");
        run.items[0].sandbox_root = fixture
            .sandbox_base
            .join(Uuid::new_v4().to_string())
            .to_string_lossy()
            .into_owned();
        fixture
            .store
            .insert_source_worktree_settlement_run(&run)
            .expect("insert mismatched startup journal");

        let error = fixture
            .store
            .source_worktree_startup_orphan_candidates(&fixture.sandbox_base)
            .expect_err("owner/root mismatch must not authorize a process identity");
        assert!(error.to_string().contains("exact owner sandbox root"));
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &format!("refs/heads/{}", fixture.branch)]
            ),
            fixture.source_oid
        );
    }

    #[test]
    fn startup_orphan_candidates_reject_symlinked_journal_root() {
        let mut fixture = IntegratedFixture::new("startup-orphan-symlink-root");
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "startup-orphan-symlink-root");
        fixture
            .store
            .insert_source_worktree_settlement_run(&run)
            .expect("insert symlink-root startup journal");
        let quarantine_root = git_worktree::derive_settlement_quarantine_path(
            &fixture.allocation.root,
            run.run_id,
            fixture.session_id,
        )
        .expect("derive fixture quarantine");
        git_worktree::with_repository_mutation(&fixture.repository, || {
            let proof = git_worktree::prepare_settlement_quarantine_path(
                &fixture.allocation.root,
                run.run_id,
                fixture.session_id,
            )?;
            git_worktree::move_worktree_to_quarantine_non_force_locked(
                &fixture.repository,
                &proof,
                &run.items[0].source_ref,
                &fixture.source_oid,
            )?;
            git_worktree::remove_worktree_non_force_locked(
                &fixture.repository,
                &fixture.allocation.root,
                &quarantine_root,
                &run.items[0].source_ref,
                &fixture.source_oid,
            )
        })
        .expect("remove fixture worktree before hostile symlink");
        let sibling = fixture.sandbox_base.join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&sibling).expect("create symlink sibling target");
        std::os::unix::fs::symlink(&sibling, &fixture.allocation.root)
            .expect("replace journal root with symlink");

        let error = fixture
            .store
            .source_worktree_startup_orphan_candidates(&fixture.sandbox_base)
            .expect_err("symlinked journal root must not authorize a process identity");
        assert!(error.to_string().contains("symbolic link"));
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &format!("refs/heads/{}", fixture.branch)]
            ),
            fixture.source_oid
        );
    }

    #[test]
    fn startup_orphan_candidates_reject_malformed_receipt_before_process_authority() {
        let mut fixture = IntegratedFixture::new("startup-malformed-receipt");
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "startup-malformed-receipt");
        fixture
            .store
            .insert_source_worktree_settlement_run(&run)
            .expect("insert startup journal");
        fixture
            .store
            .conn
            .execute_batch(
                "DROP TRIGGER source_worktree_settlement_runs_bounds_update;
                 DROP TRIGGER source_worktree_settlement_runs_identity_immutable;",
            )
            .expect("open hostile receipt seam");
        fixture
            .store
            .conn
            .execute(
                "UPDATE source_worktree_settlement_runs
                    SET observed_count=257,eligible_count=1,retained_count=256
                  WHERE run_id=?1",
                [run.run_id.to_string()],
            )
            .expect("install malformed replay counts");

        let error = fixture
            .store
            .source_worktree_startup_orphan_candidates(&fixture.sandbox_base)
            .expect_err("malformed receipt must not return process authority");
        assert!(
            error.to_string().contains("counts")
                || error.to_string().contains("eligible")
                || error.to_string().contains("item"),
            "{error}"
        );
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &format!("refs/heads/{}", fixture.branch)]
            ),
            fixture.source_oid
        );
    }

    #[test]
    fn new_settlement_run_rejects_nil_authority_ids() {
        let mut fixture = IntegratedFixture::new("new-run-nil-identities");
        let audit = fixture.audit();

        let mut nil_run = fixture.run_from_audit(&audit, "nil-run");
        nil_run.run_id = Uuid::nil();
        assert!(
            fixture
                .store
                .insert_source_worktree_settlement_run(&nil_run)
                .is_err()
        );

        let mut nil_session = fixture.run_from_audit(&audit, "nil-session");
        nil_session.items[0].session_id = Uuid::nil();
        assert!(
            fixture
                .store
                .insert_source_worktree_settlement_run(&nil_session)
                .is_err()
        );

        let mut nil_custody = fixture.run_from_audit(&audit, "nil-custody");
        nil_custody.items[0].custody_id = Uuid::nil();
        assert!(
            fixture
                .store
                .insert_source_worktree_settlement_run(&nil_custody)
                .is_err()
        );
    }

    #[test]
    fn planned_only_journal_does_not_authorize_process_reaping() {
        let mut fixture = IntegratedFixture::new("startup-planned-control");
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "startup-planned-control");
        fixture
            .store
            .insert_source_worktree_settlement_run(&run)
            .expect("insert planned control journal");
        fixture
            .store
            .conn
            .execute_batch("DROP TRIGGER source_worktree_settlement_items_forward_phase;")
            .expect("open test-only phase seam");
        fixture
            .store
            .conn
            .execute(
                "UPDATE source_worktree_settlement_items SET phase='planned'
                  WHERE run_id=?1",
                [run.run_id.to_string()],
            )
            .expect("install planned-only compatibility row");

        let candidates = fixture
            .store
            .source_worktree_startup_orphan_candidates(&fixture.sandbox_base)
            .expect("planned-only candidate query");
        assert!(candidates.is_empty());
        let mut process = startup_owned_sleep(fixture.session_id);
        assert_eq!(
            super::super::reaper::reap_startup_settlement_orphans_checked(&candidates)
                .expect("empty candidate set is inert"),
            0
        );
        process.assert_alive("planned-only process");
    }

    #[test]
    fn scheduled_dependency_identity_and_direction_are_digest_bound() {
        let fixture = IntegratedFixture::new("scheduled-dependency-identity");
        let job_id = Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        fixture
            .store
            .conn
            .execute(
                "INSERT INTO scheduled_jobs (
                    id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,
                    working_dir,provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id
                 ) VALUES (?1,'dependency','dependency','{}',NULL,?2,1,NULL,NULL,NULL,NULL,?2,?2,'resume',?3)",
                rusqlite::params![job_id.to_string(), now, fixture.session_id.to_string()],
            )
            .expect("insert Resume dependency");
        let resume = fixture.audit();
        assert_eq!(resume.report.items[0].scheduled_dependency_count, 1);
        assert_eq!(
            resume.report.items[0].disposition,
            SourceWorktreeDispositionV1::ScheduledDependency
        );

        fixture
            .store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET wake_mode=?1,wake_session_id=?2 WHERE id=?3",
                rusqlite::params![
                    format!("on_terminal:{}", fixture.session_id),
                    Uuid::new_v4().to_string(),
                    job_id.to_string(),
                ],
            )
            .expect("replace dependency direction");
        let watch = fixture.audit();
        assert_eq!(watch.report.items[0].scheduled_dependency_count, 1);
        assert_eq!(
            watch.report.items[0].disposition,
            SourceWorktreeDispositionV1::ScheduledDependency
        );
        assert_ne!(
            resume.report.items[0].scheduled_dependency_digest,
            watch.report.items[0].scheduled_dependency_digest
        );
        assert_ne!(
            resume.report.items[0].evidence_digest,
            watch.report.items[0].evidence_digest
        );
        assert_ne!(resume.report.plan_digest, watch.report.plan_digest);
    }

    #[test]
    fn missing_and_symbolic_source_resolution_are_digest_distinct() {
        let fixture = IntegratedFixture::new("source-resolution-outcome");
        let source_ref = format!("refs/heads/{}", fixture.branch);
        git(&fixture.repository, &["update-ref", "-d", &source_ref]);
        let missing = fixture.audit();
        assert_eq!(
            missing.report.items[0].disposition,
            SourceWorktreeDispositionV1::MissingSourceRef
        );

        git(
            &fixture.repository,
            &["symbolic-ref", &source_ref, "refs/heads/main"],
        );
        let symbolic = fixture.audit();
        assert_eq!(
            symbolic.report.items[0].disposition,
            SourceWorktreeDispositionV1::MissingSourceRef
        );
        assert_ne!(
            missing.report.items[0].evidence_digest,
            symbolic.report.items[0].evidence_digest
        );
        assert_ne!(missing.report.plan_digest, symbolic.report.plan_digest);
    }

    #[test]
    fn invalid_source_refs_are_bounded_wire_valid_retained_evidence() {
        let fixture = IntegratedFixture::new("invalid-source-ref-wire");
        for branch in ["main", "feature/plain", "rsi/malformed..source"] {
            let mut inventory = fixture.inventory();
            inventory[0].sandbox_branch = branch.to_string();
            inventory[0].session_sandbox_branch = Some(branch.to_string());
            let audit = git_worktree::with_repository_mutation(&fixture.repository, || {
                build_audit_locked(
                    &fixture.target.repository_identity,
                    fixture
                        .target
                        .canonical_repo_dir
                        .to_str()
                        .expect("UTF-8 repository"),
                    &fixture.sandbox_base,
                    inventory,
                    &HashSet::new(),
                    &[],
                )
            })
            .expect("build invalid-source-ref audit");
            assert!(!audit.report.applyable);
            assert_eq!(
                audit.report.items[0].disposition,
                SourceWorktreeDispositionV1::InvalidSourceRef
            );
            assert_eq!(
                audit.report.items[0].source_ref,
                source_ref_for_branch(branch)
            );
            audit
                .report
                .validate_wire()
                .expect("bounded raw invalid source-ref evidence is wire-valid");
            assert!(fixture.allocation.root.exists());
        }

        for (branch, status, active, expected) in [
            (
                "main",
                "Completed",
                true,
                SourceWorktreeDispositionV1::ActiveOwner,
            ),
            (
                "feature/nonterminal",
                "Running",
                false,
                SourceWorktreeDispositionV1::NonterminalStatus,
            ),
        ] {
            let mut inventory = fixture.inventory();
            inventory[0].sandbox_branch = branch.to_string();
            inventory[0].session_sandbox_branch = Some(branch.to_string());
            inventory[0].status = Some(status.to_string());
            let active_ids = active
                .then_some(fixture.session_id)
                .into_iter()
                .collect::<HashSet<_>>();
            let audit = git_worktree::with_repository_mutation(&fixture.repository, || {
                build_audit_locked(
                    &fixture.target.repository_identity,
                    fixture
                        .target
                        .canonical_repo_dir
                        .to_str()
                        .expect("UTF-8 repository"),
                    &fixture.sandbox_base,
                    inventory,
                    &active_ids,
                    &[],
                )
            })
            .expect("build earlier-disposition invalid-source audit");
            assert_eq!(audit.report.items[0].disposition, expected);
            audit
                .report
                .validate_wire()
                .expect("all retained dispositions allow bounded raw source-ref evidence");
        }
    }

    #[test]
    fn unrelated_symref_dependency_retains_source_before_any_effect() {
        let fixture = IntegratedFixture::new("symref-dependent");
        let source_ref = format!("refs/heads/{}", fixture.branch);
        git(
            &fixture.repository,
            &["symbolic-ref", "refs/heads/alias", &source_ref],
        );
        let audit = fixture.audit();
        assert_eq!(audit.report.counts.eligible, 0);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::SourceRefMismatch
        );
        assert!(fixture.allocation.root.exists());
        assert_eq!(
            git(&fixture.repository, &["rev-parse", "refs/heads/alias"]),
            fixture.source_oid
        );
        assert_eq!(
            git(&fixture.repository, &["rev-parse", &source_ref]),
            fixture.source_oid
        );
    }

    #[test]
    fn ignored_content_is_retained_before_non_force_removal() {
        let fixture = IntegratedFixture::new("ignored-content");
        let excludes = fixture.sandbox_base.join("settlement-excludes");
        std::fs::write(&excludes, "ignored.secret\n").expect("global exclude fixture");
        git(
            &fixture.repository,
            &[
                "config",
                "core.excludesFile",
                excludes.to_str().expect("UTF-8 exclude path"),
            ],
        );
        std::fs::write(
            fixture.allocation.root.join("ignored.secret"),
            "unique ignored data\n",
        )
        .expect("ignored fixture data");

        let audit = fixture.audit();
        assert_eq!(audit.report.counts.eligible, 0);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::DirtyWorktree
        );
        assert!(fixture.allocation.root.join("ignored.secret").exists());
        assert!(fixture.allocation.root.exists());
    }

    #[test]
    fn canonical_evidence_binds_collapsed_session_and_git_observation_drift() {
        let fixture = IntegratedFixture::new("canonical-evidence");
        let build = |inventory| {
            git_worktree::with_repository_mutation(&fixture.repository, || {
                build_audit_locked(
                    &fixture.target.repository_identity,
                    fixture
                        .target
                        .canonical_repo_dir
                        .to_str()
                        .expect("UTF-8 repository"),
                    &fixture.sandbox_base,
                    inventory,
                    &HashSet::new(),
                    &[],
                )
            })
            .expect("build retained audit")
        };

        let mut tuple_a = fixture.inventory();
        tuple_a[0].session_sandbox_root = Some("/invalid/session-root-a".into());
        let first = build(tuple_a);
        let mut tuple_b = fixture.inventory();
        tuple_b[0].session_sandbox_root = Some("/invalid/session-root-b".into());
        let second = build(tuple_b);
        assert_eq!(
            first.report.items[0].disposition,
            SourceWorktreeDispositionV1::CustodyUnverified
        );
        assert_eq!(
            first.report.items[0].disposition,
            second.report.items[0].disposition
        );
        assert_ne!(
            first.report.items[0].evidence_digest,
            second.report.items[0].evidence_digest
        );
        assert_ne!(first.report.plan_digest, second.report.plan_digest);

        let mut first_git_digest = None;
        for branch in ["rsi/observed-a", "rsi/observed-b"] {
            git(
                &fixture.repository,
                &["branch", "-f", branch, &fixture.source_oid],
            );
            git(
                &fixture.allocation.root,
                &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
            );
            let audit = build(fixture.inventory());
            assert_eq!(
                audit.report.items[0].disposition,
                SourceWorktreeDispositionV1::SourceRefMismatch
            );
            if branch.ends_with('a') {
                first_git_digest = Some(audit.report.items[0].evidence_digest.clone());
            } else {
                assert_ne!(
                    first_git_digest.as_ref().expect("first Git evidence"),
                    &audit.report.items[0].evidence_digest
                );
            }
        }
    }

    #[test]
    fn final_database_fault_stays_branch_removed_and_exact_replay_settles() {
        let fixture = IntegratedFixture::new("database-retry");
        let audit = fixture.audit();
        let plan_digest = audit.report.plan_digest.as_str().to_string();
        let authorization = audit
            .report
            .authorization_phrase
            .as_deref()
            .expect("apply authorization");
        let authorization_digest = hash_bytes(authorization.as_bytes());
        let request_fingerprint = hash_bytes(b"database-retry-request");
        let identity = fixture.target.repository_identity.clone();
        let repository = fixture.repository.clone();
        let sandbox_base = fixture.sandbox_base.clone();
        let root = fixture.allocation.root.clone();
        let quarantine_holder_proc_root = fixture.quarantine_holder_proc.proc_root().to_path_buf();
        let quarantine_holder_proc_uid = fixture.quarantine_holder_proc.uid();
        let store = Arc::new(tokio::sync::Mutex::new(fixture.store));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));

        crate::store::cohort_settlement::fail_next_source_worktree_settlement_finalization();
        let first = crate::session::reaper::with_quarantine_holder_test_proc(
            &quarantine_holder_proc_root,
            quarantine_holder_proc_uid,
            || {
                git_worktree::with_repository_mutation(&repository, || {
                    apply_locked(
                        ApplyStart::Fresh(audit),
                        &identity,
                        "fixture:database-retry",
                        &plan_digest,
                        authorization_digest.as_str(),
                        request_fingerprint.as_str(),
                        &sandbox_base,
                        &store,
                        &active,
                        &completed,
                    )
                })
            },
        )
        .expect("retain retryable database failure receipt");
        assert!(!root.exists());
        assert_eq!(
            first.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::BranchRemoved
        );
        assert_eq!(
            first.receipt.items[0].refusal_code,
            Some(SourceWorktreeSettlementRefusalV1::DatabaseSettlementFailed)
        );
        first
            .receipt
            .clone()
            .validate_wire()
            .expect("retryable receipt is wire-valid");

        let replay = git_worktree::with_repository_mutation(&repository, || {
            apply_locked(
                ApplyStart::Replay(first.receipt),
                &identity,
                "fixture:database-retry",
                &plan_digest,
                authorization_digest.as_str(),
                request_fingerprint.as_str(),
                &sandbox_base,
                &store,
                &active,
                &completed,
            )
        })
        .expect("exact replay settles database phase");
        assert_eq!(
            replay.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled
        );
        assert_eq!(replay.receipt.items[0].refusal_code, None);
        replay
            .receipt
            .validate_wire()
            .expect("settled retry receipt is wire-valid");
    }

    #[test]
    fn integrated_ancestor_apply_is_local_exact_atomic_and_replay_safe() {
        let directory = tempfile::tempdir().expect("settlement fixture directory");
        let quarantine_holder_proc = crate::session::reaper::SyntheticQuarantineHolderProc::new();
        let repository = directory.path().join("repository");
        let remote = directory.path().join("remote.git");
        let sandbox_base = directory.path().join("sandboxes");
        std::fs::create_dir_all(&repository).expect("repository directory");
        create_private_sandbox_base(&sandbox_base);
        git(&repository, &["init", "-q", "-b", "main"]);
        git(
            &repository,
            &["config", "user.email", "settlement@example.test"],
        );
        git(&repository, &["config", "user.name", "Settlement Test"]);
        std::fs::write(repository.join("tracked"), "base\n").expect("base fixture file");
        git(&repository, &["add", "tracked"]);
        git(&repository, &["commit", "-qm", "base"]);
        let source_oid = git(&repository, &["rev-parse", "HEAD"]);
        git(&repository, &["branch", "keep/unrelated", &source_oid]);

        std::fs::create_dir_all(&remote).expect("remote directory");
        git(&remote, &["init", "--bare", "-q"]);
        git(
            &repository,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("UTF-8 remote"),
            ],
        );
        git(&repository, &["push", "-q", "origin", "main"]);

        let session_id = Uuid::new_v4();
        let branch = format!("rsi/{session_id}");
        let allocation = git_worktree::allocate(
            &sandbox_base,
            session_id,
            &repository,
            &source_oid,
            Some(&branch),
        )
        .expect("allocate candidate worktree");
        git(
            &repository,
            &["push", "-q", "origin", &format!("{branch}:{branch}")],
        );
        let remote_source_before = git(&remote, &["rev-parse", &format!("refs/heads/{branch}")]);

        std::fs::write(repository.join("tracked"), "base\nintegrated\n")
            .expect("integrating target change");
        git(&repository, &["add", "tracked"]);
        git(&repository, &["commit", "-qm", "integrate source"]);
        let target_oid_before = git(&repository, &["rev-parse", "refs/heads/main"]);
        let unrelated_before = git(&repository, &["rev-parse", "refs/heads/keep/unrelated"]);
        let target = git_worktree::with_repository_mutation(&repository, || {
            git_worktree::observe_repository_target_locked(&repository)
        })
        .expect("observe fixture repository identity");

        let mut store = crate::store::Store::open_in_memory().expect("settlement Store");
        let custody_id = Uuid::new_v4();
        let mut session = crate::store::tests::make_test_session();
        session.id = session_id;
        session.status = SessionStatus::Completed;
        session.working_dir = target.canonical_repo_dir.clone();
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(allocation.root.clone());
        session.sandbox_branch = Some(branch.clone());
        session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .insert_session_with_custody(
                &session,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id,
                    canonical_repo_dir: target.canonical_repo_dir.to_string_lossy().into_owned(),
                    sandbox_root: allocation.root.to_string_lossy().into_owned(),
                    sandbox_branch: branch.clone(),
                    repository_identity: target.repository_identity.clone(),
                    source_commit: source_oid.clone(),
                    cause: CustodyCause::FreshLaunch,
                }),
            )
            .expect("seed exact custody-backed terminal Session");

        let inventory = store
            .source_worktree_inventory(
                &target.repository_identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
            )
            .expect("load settlement inventory");
        let total_changes_before: i64 = store
            .conn
            .query_row("SELECT total_changes()", [], |row| row.get(0))
            .expect("read pre-audit total_changes");
        let active_ids = HashSet::new();
        let first = git_worktree::with_repository_mutation(&repository, || {
            build_audit_locked(
                &target.repository_identity,
                target
                    .canonical_repo_dir
                    .to_str()
                    .expect("UTF-8 canonical repository"),
                &sandbox_base,
                inventory.clone(),
                &active_ids,
                &[],
            )
        })
        .expect("first exact audit");
        let repeated = git_worktree::with_repository_mutation(&repository, || {
            build_audit_locked(
                &target.repository_identity,
                target
                    .canonical_repo_dir
                    .to_str()
                    .expect("UTF-8 canonical repository"),
                &sandbox_base,
                inventory,
                &active_ids,
                &[],
            )
        })
        .expect("repeated exact audit");
        assert_eq!(first.report, repeated.report);
        assert_eq!(first.report.writes, 0);
        assert!(first.report.applyable);
        assert_eq!(first.report.counts.eligible, 1);
        assert_eq!(
            store
                .conn
                .query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
                .expect("read post-audit total_changes"),
            total_changes_before,
            "audit must not perform a Store write"
        );

        let plan_digest = first.report.plan_digest.as_str().to_string();
        let authorization = first
            .report
            .authorization_phrase
            .clone()
            .expect("applyable audit phrase");
        let authorization_digest = hash_bytes(authorization.as_bytes());
        let idempotency_key = "fixture:integrated-ancestor";
        let request_fingerprint = hash_canonical(&CanonicalApplyFingerprint {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: &target.repository_identity,
            plan_digest: &plan_digest,
            authorization_digest: authorization_digest.as_str(),
            idempotency_key,
        })
        .expect("fixture request fingerprint");
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        git(&repository, &["update-ref", "refs/heads/main", &source_oid]);
        let stale = git_worktree::with_repository_mutation(&repository, || {
            apply_locked(
                ApplyStart::Fresh(first),
                &target.repository_identity,
                idempotency_key,
                &plan_digest,
                authorization_digest.as_str(),
                request_fingerprint.as_str(),
                &sandbox_base,
                &store,
                &active,
                &completed,
            )
        });
        assert!(stale.is_err(), "target drift must stale the exact audit");
        assert!(allocation.root.exists());
        assert_eq!(
            store
                .blocking_lock()
                .conn
                .query_row(
                    "SELECT count(*) FROM source_worktree_settlement_runs",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count stale-apply journal rows"),
            0,
            "stale digest must write no durable intent"
        );
        git(
            &repository,
            &["update-ref", "refs/heads/main", &target_oid_before],
        );
        let outcome = crate::session::reaper::with_quarantine_holder_test_proc(
            quarantine_holder_proc.proc_root(),
            quarantine_holder_proc.uid(),
            || {
                git_worktree::with_repository_mutation(&repository, || {
                    apply_locked(
                        ApplyStart::Fresh(repeated),
                        &target.repository_identity,
                        idempotency_key,
                        &plan_digest,
                        authorization_digest.as_str(),
                        request_fingerprint.as_str(),
                        &sandbox_base,
                        &store,
                        &active,
                        &completed,
                    )
                })
            },
        )
        .expect("apply exact integrated-ancestor settlement");
        assert_eq!(outcome.receipt.counts.settled, 1, "{:#?}", outcome.receipt);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled
        );
        assert!(!allocation.root.exists());
        assert_eq!(
            git(&repository, &["rev-parse", "refs/heads/main"]),
            target_oid_before
        );
        assert_eq!(
            git(&repository, &["rev-parse", "refs/heads/keep/unrelated"]),
            unrelated_before
        );
        let missing_source = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .status()
            .expect("probe deleted local source ref");
        assert!(!missing_source.success());
        assert_eq!(
            git(&remote, &["rev-parse", &format!("refs/heads/{branch}")]),
            remote_source_before,
            "settlement must not mutate a remote ref"
        );

        let store_guard = store.blocking_lock();
        let settled_session = store_guard
            .get_session(session_id)
            .expect("read settled Session")
            .expect("settled Session retained");
        assert_eq!(settled_session.status, SessionStatus::Archived);
        assert_eq!(settled_session.sandbox_kind, Some(SandboxKind::GitWorktree));
        assert_eq!(settled_session.sandbox_root, None);
        assert_eq!(settled_session.sandbox_branch, None);
        assert_eq!(
            settled_session.sandbox_cleanup_state,
            Some(SandboxCleanupState::Purged)
        );
        let custody: (String, Option<String>, i64) = store_guard
            .conn
            .query_row(
                "SELECT state,owner_session_id,count(*) OVER () FROM sandbox_custody_roots WHERE custody_id=?1",
                [custody_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read settled custody tombstone");
        assert_eq!(custody, ("purged".into(), None, 1));
        assert!(
            store_guard.unarchive_session(session_id).is_err(),
            "receipt-backed historical purge must not unarchive"
        );
        drop(store_guard);

        let replay = git_worktree::with_repository_mutation(&repository, || {
            apply_locked(
                ApplyStart::Replay(outcome.receipt.clone()),
                &target.repository_identity,
                idempotency_key,
                &plan_digest,
                authorization_digest.as_str(),
                request_fingerprint.as_str(),
                &sandbox_base,
                &store,
                &active,
                &completed,
            )
        })
        .expect("exact replay returns retained receipt");
        assert_eq!(replay.receipt, outcome.receipt);

        let store_guard = store.blocking_lock();
        assert!(
            store_guard
                .replay_source_worktree_settlement_run(
                    &target.repository_identity,
                    idempotency_key,
                    &format!("sha256:{}", "0".repeat(64)),
                )
                .is_err(),
            "changed-content idempotency reuse must conflict"
        );
        assert!(
            store_guard
                .conn
                .execute(
                    "UPDATE source_worktree_settlement_runs SET plan_digest=?1 WHERE run_id=?2",
                    rusqlite::params![
                        format!("sha256:{}", "0".repeat(64)),
                        outcome.receipt.run_id.to_string()
                    ],
                )
                .is_err(),
            "retained run identity must be immutable"
        );
        assert!(
            store_guard
                .conn
                .execute(
                    "UPDATE source_worktree_settlement_items SET phase='branch_removed' WHERE run_id=?1",
                    [outcome.receipt.run_id.to_string()],
                )
                .is_err(),
            "terminal item phase must not regress"
        );
        assert!(
            store_guard
                .conn
                .execute(
                    "DELETE FROM source_worktree_settlement_runs WHERE run_id=?1",
                    [outcome.receipt.run_id.to_string()],
                )
                .is_err(),
            "retained settlement receipt must not delete"
        );
    }

    #[test]
    fn clean_nonancestor_is_retained_without_phrase_or_writes() {
        let directory = tempfile::tempdir().expect("nonancestor fixture directory");
        let repository = directory.path().join("repository");
        let sandbox_base = directory.path().join("sandboxes");
        std::fs::create_dir_all(&repository).expect("repository directory");
        create_private_sandbox_base(&sandbox_base);
        git(&repository, &["init", "-q", "-b", "main"]);
        git(
            &repository,
            &["config", "user.email", "retained@example.test"],
        );
        git(&repository, &["config", "user.name", "Retained Test"]);
        std::fs::write(repository.join("tracked"), "base\n").expect("base fixture file");
        git(&repository, &["add", "tracked"]);
        git(&repository, &["commit", "-qm", "base"]);
        let base_oid = git(&repository, &["rev-parse", "HEAD"]);

        let session_id = Uuid::new_v4();
        let custody_id = Uuid::new_v4();
        let branch = format!("rsi/{session_id}");
        let allocation = git_worktree::allocate(
            &sandbox_base,
            session_id,
            &repository,
            &base_oid,
            Some(&branch),
        )
        .expect("allocate retained worktree");
        std::fs::write(allocation.root.join("source-only"), "unique source work\n")
            .expect("source-only fixture file");
        git(&allocation.root, &["add", "source-only"]);
        git(&allocation.root, &["commit", "-qm", "unique source"]);
        let source_oid = git(&allocation.root, &["rev-parse", "HEAD"]);

        std::fs::write(repository.join("target-only"), "independent target work\n")
            .expect("target-only fixture file");
        git(&repository, &["add", "target-only"]);
        git(&repository, &["commit", "-qm", "independent target"]);
        let target = git_worktree::with_repository_mutation(&repository, || {
            git_worktree::observe_repository_target_locked(&repository)
        })
        .expect("observe retained repository");

        let mut store = crate::store::Store::open_in_memory().expect("retained Store");
        let mut session = crate::store::tests::make_test_session();
        session.id = session_id;
        session.status = SessionStatus::Completed;
        session.working_dir = target.canonical_repo_dir.clone();
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(allocation.root.clone());
        session.sandbox_branch = Some(branch.clone());
        session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .insert_session_with_custody(
                &session,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id,
                    canonical_repo_dir: target.canonical_repo_dir.to_string_lossy().into_owned(),
                    sandbox_root: allocation.root.to_string_lossy().into_owned(),
                    sandbox_branch: branch.clone(),
                    repository_identity: target.repository_identity.clone(),
                    source_commit: source_oid.clone(),
                    cause: CustodyCause::FreshLaunch,
                }),
            )
            .expect("seed retained custody");
        let inventory = store
            .source_worktree_inventory(
                &target.repository_identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
            )
            .expect("load retained inventory");
        let writes_before: i64 = store
            .conn
            .query_row("SELECT total_changes()", [], |row| row.get(0))
            .expect("read pre-audit writes");
        let audit = git_worktree::with_repository_mutation(&repository, || {
            build_audit_locked(
                &target.repository_identity,
                target
                    .canonical_repo_dir
                    .to_str()
                    .expect("UTF-8 canonical repository"),
                &sandbox_base,
                inventory,
                &HashSet::new(),
                &[],
            )
        })
        .expect("audit nonancestor");

        assert_eq!(audit.report.writes, 0);
        assert!(!audit.report.applyable);
        assert_eq!(audit.report.authorization_phrase, None);
        assert_eq!(audit.report.counts.eligible, 0);
        assert_eq!(audit.report.counts.retained, 1);
        assert_eq!(audit.report.items.len(), 1);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::RetainedNonAncestor
        );
        assert_eq!(audit.report.items[0].proof, SourceWorktreeProofV1::None);
        assert_eq!(
            store
                .conn
                .query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
                .expect("read post-audit writes"),
            writes_before
        );
        assert!(allocation.root.exists());
        assert_eq!(git(&repository, &["rev-parse", &branch]), source_oid);
        assert_eq!(
            store
                .get_session(session_id)
                .expect("read retained Session")
                .expect("retained Session exists")
                .sandbox_cleanup_state,
            Some(SandboxCleanupState::Live)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_reaps_scheduled_alias_then_requires_recovery_before_git_effects() {
        let mut fixture = IntegratedFixture::new("startup-scheduled-alias");
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "startup-scheduled-alias");
        let run_id = run.run_id;
        fixture
            .store
            .insert_source_worktree_settlement_run(&run)
            .expect("insert alias-protected settlement intent");
        let alias_id = Uuid::new_v4();
        let mut scheduled_alias = crate::store::tests::make_test_session();
        scheduled_alias.id = alias_id;
        scheduled_alias.status = SessionStatus::Starting;
        scheduled_alias.working_dir = fixture.allocation.root.join("scheduled-fresh-alias");
        scheduled_alias.scheduled_job_id = Some(Uuid::new_v4());
        fixture
            .store
            .insert_session(&scheduled_alias)
            .expect("seed crash-left scheduled Fresh alias");
        let repository = fixture.repository.clone();
        let source_ref = format!("refs/heads/{}", fixture.branch);
        let source_oid = fixture.source_oid.clone();
        let root = fixture.allocation.root.clone();
        let sandbox_base = fixture.sandbox_base.clone();
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            fixture.store,
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
            sandbox_base,
        )
        .expect("construct scheduled alias recovery manager");
        let mut orphan = startup_owned_sleep(alias_id);

        manager
            .restore_sessions()
            .await
            .expect("reap alias and fail closed before Git");
        orphan.wait_signalled("reaped scheduled alias");
        let receipt = manager
            .get_source_worktree_settlement_run(run_id)
            .await
            .expect("read alias-protected receipt")
            .expect("alias-protected receipt retained");
        assert_eq!(
            receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert!(root.exists());
        assert_eq!(git(&repository, &["rev-parse", &source_ref]), source_oid);
        let alias = manager
            .store
            .lock()
            .await
            .get_session(alias_id)
            .expect("read crash-left alias")
            .expect("crash-left alias retained");
        assert_eq!(alias.status, SessionStatus::Failed);
    }

    #[test]
    fn fresh_apply_reaps_exact_owner_only_after_durable_intent() {
        let mut fixture = IntegratedFixture::new("fresh-apply-orphan");
        let audit = fixture.audit();
        let repository_identity = fixture.target.repository_identity.clone();
        let plan_digest = audit.report.plan_digest.as_str().to_string();
        let authorization = audit
            .report
            .authorization_phrase
            .as_deref()
            .expect("fresh apply authorization");
        let authorization_digest = hash_bytes(authorization.as_bytes());
        let idempotency_key = "fixture:fresh-apply-orphan";
        let request_fingerprint = hash_canonical(&CanonicalApplyFingerprint {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: &repository_identity,
            plan_digest: &plan_digest,
            authorization_digest: authorization_digest.as_str(),
            idempotency_key,
        })
        .expect("fresh apply request fingerprint");
        let owner_id = fixture.session_id;
        let repository = fixture.repository.clone();
        let sandbox_base = fixture.sandbox_base.clone();
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().expect("replacement fixture Store"),
        )));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let mut orphan = startup_owned_sleep(owner_id);

        let receipt = fixture
            .with_quarantine_holder_test_proc(|| {
                git_worktree::with_repository_mutation(&repository, || {
                    apply_locked_with_orphan_proof(
                        ApplyStart::Fresh(audit),
                        &repository_identity,
                        idempotency_key,
                        &plan_digest,
                        authorization_digest.as_str(),
                        request_fingerprint.as_str(),
                        &sandbox_base,
                        &store,
                        &active,
                        &completed,
                        true,
                    )
                })
            })
            .expect("fresh apply reaps journal-authorized owner")
            .receipt;
        assert_eq!(receipt.counts.settled, 1, "{receipt:#?}");
        assert_eq!(
            receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled
        );
        orphan.wait_signalled("fresh apply orphan");
    }

    #[test]
    fn replay_reaps_only_effect_capable_owner_after_an_earlier_refusal() {
        let mut fixture = IntegratedFixture::new("replay-effect-capable-orphan");
        let audit = fixture.audit();
        let refused_session_id = fixture.insert_synthetic_custody(
            "replay-refused-owner",
            fixture.target.repository_identity.clone(),
            fixture
                .target
                .canonical_repo_dir
                .to_string_lossy()
                .into_owned(),
        );
        let refused_custody_id = fixture
            .store
            .conn
            .query_row(
                "SELECT sandbox_custody_id FROM sessions WHERE id=?1",
                [refused_session_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map(|value| Uuid::parse_str(&value).expect("canonical refused custody id"))
            .expect("load refused custody id");

        let mut run = fixture.run_from_audit(&audit, "replay-effect-capable-orphan");
        let intent_item = run.items.pop().expect("intent item");
        let mut refused_item = intent_item.clone();
        refused_item.session_id = refused_session_id;
        refused_item.custody_id = refused_custody_id;
        run.items = vec![refused_item, intent_item];
        run.observed_count = 2;
        run.retained_count = 0;
        let run_id = run.run_id;
        let repository_identity = run.repository_identity.clone();
        let idempotency_key = run.idempotency_key.clone();
        let plan_digest = run.plan_digest.clone();
        let authorization_digest = run.authorization_digest.clone();
        let request_fingerprint = run.request_fingerprint.clone();
        fixture
            .store
            .insert_source_worktree_settlement_run(&run)
            .expect("insert mixed replay run");
        fixture
            .store
            .mark_source_worktree_settlement_refused(
                run_id,
                refused_session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementRefusalV1::CustodyDrift.as_str(),
                "pre-effect refused control",
            )
            .expect("refuse first replay item");
        let receipt = fixture
            .store
            .get_source_worktree_settlement_run(run_id)
            .expect("load mixed replay receipt")
            .expect("mixed replay receipt exists");
        assert_eq!(
            effect_capable_replay_session_ids(&receipt),
            vec![fixture.session_id],
            "Refused owner must not enter replay guards or active-owner blocking"
        );

        let mut refused_process = startup_owned_sleep(refused_session_id);
        let mut intent_process = startup_owned_sleep(fixture.session_id);

        let repository = fixture.repository.clone();
        let sandbox_base = fixture.sandbox_base.clone();
        let store = Arc::new(tokio::sync::Mutex::new(fixture.store));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let outcome = git_worktree::with_repository_mutation(&repository, || {
            apply_locked_with_orphan_proof(
                ApplyStart::Replay(receipt),
                &repository_identity,
                &idempotency_key,
                &plan_digest,
                &authorization_digest,
                &request_fingerprint,
                &sandbox_base,
                &store,
                &active,
                &completed,
                true,
            )
        })
        .expect("replay closes the later intent without touching refused owner");
        assert_eq!(outcome.receipt.items.len(), 2);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Refused
        );
        assert_eq!(
            outcome.receipt.items[1].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        intent_process.wait_signalled("reaped intent owner");
        refused_process.assert_alive("Refused owner is safe to relaunch and must not be signaled");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_apply_scan_proof_failure_has_zero_intent_git_or_process_effects() {
        let fixture = IntegratedFixture::new("fresh-apply-proof-failure");
        let audit = fixture.audit();
        let params = ApplySourceWorktreeCohortParams {
            repository_identity: fixture.target.repository_identity.clone(),
            plan_digest: audit.report.plan_digest.clone(),
            authorization: audit
                .report
                .authorization_phrase
                .clone()
                .expect("proof failure authorization"),
            idempotency_key: "fixture:fresh-apply-proof-failure".into(),
        };
        let owner_id = fixture.session_id;
        let repository = fixture.repository.clone();
        let root = fixture.allocation.root.clone();
        let source_ref = format!("refs/heads/{}", fixture.branch);
        let source_oid = fixture.source_oid.clone();
        let socket = fixture._directory.path().join("daemon.sock");
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            fixture.store,
            false,
            socket,
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
            fixture.sandbox_base,
        )
        .expect("construct proof failure manager");
        manager
            .restore_sessions()
            .await
            .expect("restore proof failure owner");
        let mut orphan = startup_owned_sleep(owner_id);
        let _fault = super::super::reaper::scoped_startup_settlement_scan_proof_failure(owner_id);

        let error = manager
            .apply_source_worktree_cohort(params)
            .await
            .expect_err("read-only process proof must fail before intent");
        assert!(error.to_string().contains("scan proof failure"));
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .conn
                .query_row(
                    "SELECT count(*) FROM source_worktree_settlement_runs",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .expect("count proof-failure runs"),
            0
        );
        assert!(root.exists());
        assert_eq!(git(&repository, &["rev-parse", &source_ref]), source_oid);
        orphan.assert_alive("read-only proof failure must not signal the process");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_apply_reap_failure_retains_intent_before_all_git_effects() {
        let fixture = IntegratedFixture::new("fresh-apply-reap-failure");
        let audit = fixture.audit();
        let params = ApplySourceWorktreeCohortParams {
            repository_identity: fixture.target.repository_identity.clone(),
            plan_digest: audit.report.plan_digest.clone(),
            authorization: audit
                .report
                .authorization_phrase
                .clone()
                .expect("reap failure authorization"),
            idempotency_key: "fixture:fresh-apply-reap-failure".into(),
        };
        let repository = fixture.repository.clone();
        let root = fixture.allocation.root.clone();
        let source_ref = format!("refs/heads/{}", fixture.branch);
        let source_oid = fixture.source_oid.clone();
        let socket = fixture._directory.path().join("daemon.sock");
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            fixture.store,
            false,
            socket,
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
            fixture.sandbox_base,
        )
        .expect("construct reap failure manager");
        manager
            .restore_sessions()
            .await
            .expect("restore reap failure owner");
        let _fault =
            super::super::reaper::scoped_startup_settlement_reap_failure(fixture.session_id);

        let error = manager
            .apply_source_worktree_cohort(params)
            .await
            .expect_err("post-intent reap failure must stop before Git");
        assert!(error.to_string().contains("orphan reap failure"));
        let receipt = manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT phase FROM source_worktree_settlement_items",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read retained post-intent phase");
        assert_eq!(receipt, "intent_committed");
        assert!(root.exists());
        assert_eq!(git(&repository, &["rev-parse", &source_ref]), source_oid);
    }

    #[test]
    fn quarantine_crash_boundaries_replay_to_one_exact_settlement() {
        #[derive(Clone, Copy)]
        enum CrashPoint {
            Moved,
            Marker,
            BranchDeleted,
            DetachedBranchDeleted,
            Removed,
            WorktreePhase,
            BranchPhase,
        }

        for (label, crash_point) in [
            ("moved", CrashPoint::Moved),
            ("marker", CrashPoint::Marker),
            ("branch-deleted", CrashPoint::BranchDeleted),
            ("detached-branch-deleted", CrashPoint::DetachedBranchDeleted),
            ("removed", CrashPoint::Removed),
            ("worktree-phase", CrashPoint::WorktreePhase),
            ("branch-phase", CrashPoint::BranchPhase),
        ] {
            let mut fixture = IntegratedFixture::new(&format!("crash-{label}"));
            let run = fixture.commit_intent(&format!("fixture:crash-{label}"));
            let quarantine_root = fixture.move_to_quarantine(&run);
            if !matches!(crash_point, CrashPoint::Moved) {
                fixture.persist_quarantine_marker(&run, &quarantine_root);
            }
            if matches!(
                crash_point,
                CrashPoint::BranchDeleted
                    | CrashPoint::DetachedBranchDeleted
                    | CrashPoint::Removed
                    | CrashPoint::WorktreePhase
                    | CrashPoint::BranchPhase
            ) {
                fixture.delete_source_ref(&run);
            }
            if matches!(crash_point, CrashPoint::DetachedBranchDeleted) {
                fixture.detach_quarantine_at_source(&run, &quarantine_root);
            }
            if matches!(
                crash_point,
                CrashPoint::Removed | CrashPoint::WorktreePhase | CrashPoint::BranchPhase
            ) {
                fixture.remove_quarantine_after_source_delete(&run, &quarantine_root);
            }
            if matches!(
                crash_point,
                CrashPoint::WorktreePhase | CrashPoint::BranchPhase
            ) {
                fixture.advance_to_worktree_removed(&run, None);
            }
            if matches!(crash_point, CrashPoint::BranchPhase) {
                fixture.advance_to_branch_removed(&run);
            }

            let (outcome, store) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.counts.settled, 1,
                "{label}: {:?}",
                outcome.receipt.items[0]
            );
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::Settled,
                "{label}"
            );
            QuarantineRemoveAuthorityV1::parse_canonical(
                outcome.receipt.items[0]
                    .before_observation
                    .as_deref()
                    .expect("settled crash replay retains its marker"),
            )
            .expect("crash replay marker remains canonical");
            assert!(!fixture.allocation.root.exists(), "{label}");
            assert!(!quarantine_root.exists(), "{label}");
            assert!(
                git_worktree::with_repository_mutation(&fixture.repository, || {
                    git_worktree::resolve_ref_locked(&fixture.repository, &run.items[0].source_ref)
                })
                .expect("observe crash replay source ref")
                .is_none(),
                "{label}"
            );
            let settled = store
                .blocking_lock()
                .get_session(fixture.session_id)
                .expect("read crash replay Session")
                .expect("crash replay Session retained");
            assert_eq!(settled.status, SessionStatus::Archived, "{label}");
            assert_eq!(
                settled.sandbox_cleanup_state,
                Some(SandboxCleanupState::Purged),
                "{label}"
            );
        }
    }

    #[test]
    fn missing_source_restoration_precedes_quarantine_tree_or_admin_reproof() {
        for case in [
            "partial-tree",
            "admin-residue",
            "path-absent-admin-residue",
            "original-path-recreated",
        ] {
            let mut fixture = IntegratedFixture::new(&format!("restore-before-{case}"));
            let run = fixture.commit_intent(&format!("fixture:restore-before-{case}"));
            let quarantine_root = fixture.move_to_quarantine(&run);
            fixture.persist_quarantine_marker(&run, &quarantine_root);
            let admin_dir = PathBuf::from(git(
                &quarantine_root,
                &["rev-parse", "--path-format=absolute", "--git-dir"],
            ));
            fixture.delete_source_ref(&run);

            if case == "partial-tree" {
                std::fs::remove_file(quarantine_root.join("tracked"))
                    .expect("simulate interrupted partial worktree removal");
            } else if case == "admin-residue" {
                std::fs::remove_file(admin_dir.join("HEAD"))
                    .expect("simulate interrupted administrative removal");
            } else if case == "path-absent-admin-residue" {
                std::fs::remove_dir_all(&quarantine_root)
                    .expect("simulate removed path with retained Git administration");
            } else {
                std::fs::create_dir(&fixture.allocation.root)
                    .expect("recreate original path after source deletion");
            }

            let (outcome, _) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired,
                "{case}"
            );
            assert_eq!(
                quarantine_root.exists(),
                case != "path-absent-admin-residue",
                "{case}"
            );
            if case == "path-absent-admin-residue" {
                assert!(admin_dir.exists(), "{case}");
            }
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                run.items[0].source_oid,
                "source restoration must not depend on the damaged quarantine: {case}"
            );
        }
    }

    #[test]
    fn missing_source_restoration_precedes_holder_or_registration_reproof() {
        for case in ["holder", "registration"] {
            let mut fixture = IntegratedFixture::new(&format!("restore-before-{case}"));
            let run = fixture.commit_intent(&format!("fixture:restore-before-{case}"));
            let quarantine_root = fixture.move_to_quarantine(&run);
            fixture.persist_quarantine_marker(&run, &quarantine_root);

            let mut extra = None;
            if case == "holder" {
                fixture
                    .quarantine_holder_proc
                    .add_fd_holder(&quarantine_root.join("tracked"));
            } else {
                let other_session = Uuid::new_v4();
                let other_branch = format!("rsi/restore-registration-other/{other_session}");
                let allocation = git_worktree::allocate(
                    &fixture.sandbox_base,
                    other_session,
                    &fixture.repository,
                    &fixture.source_oid,
                    Some(&other_branch),
                )
                .expect("allocate concurrent registration worktree");
                git(
                    &allocation.root,
                    &["symbolic-ref", "HEAD", &run.items[0].source_ref],
                );
                extra = Some(allocation);
            }
            fixture.delete_source_ref(&run);

            let (outcome, _) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired,
                "{case}"
            );
            assert!(quarantine_root.exists(), "{case}");
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                run.items[0].source_oid,
                "source restoration must precede destructive reproof: {case}"
            );
            drop(extra);
        }
    }

    #[test]
    fn missing_source_restore_lock_refusal_is_explicit_and_never_overwrites() {
        let mut fixture = IntegratedFixture::new("restore-lock-refusal");
        let run = fixture.commit_intent("fixture:restore-lock-refusal");
        let quarantine_root = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine_root);
        fixture.delete_source_ref(&run);
        let source_path = git(
            &fixture.repository,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                &run.items[0].source_ref,
            ],
        );
        let source_lock = PathBuf::from(format!("{source_path}.lock"));
        std::fs::create_dir_all(source_lock.parent().expect("source lock parent"))
            .expect("create exact source ref lock parent");
        std::fs::write(&source_lock, "ordinary concurrent ref lock\n")
            .expect("hold exact source ref lock");

        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert_eq!(
            outcome.receipt.items[0].after_observation.as_deref(),
            Some(
                "retained marker authorized exact source restoration, but the compare-create was refused and the source remains missing"
            )
        );
        assert!(quarantine_root.exists());
        std::fs::remove_file(source_lock).expect("release exact source ref lock");
        assert!(
            git_worktree::with_repository_mutation(&fixture.repository, || {
                git_worktree::resolve_ref_locked(&fixture.repository, &run.items[0].source_ref)
            })
            .expect("observe refused restoration source")
            .is_none()
        );
    }

    #[test]
    fn branch_first_restore_collision_preserves_drift_and_quarantine() {
        let mut fixture = IntegratedFixture::new("branch-first-restore-collision");
        let run = fixture.commit_intent("fixture:branch-first-restore-collision");
        let quarantine_root = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine_root);
        fixture.delete_source_ref(&run);

        let repository = fixture.repository.clone();
        let source_ref = run.items[0].source_ref.clone();
        let drift_oid = run.target_oid.clone();
        git_worktree::set_atomic_ref_pre_spawn_test_hook(move || {
            git(&repository, &["update-ref", &source_ref, &drift_oid]);
        });

        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert!(quarantine_root.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.target_oid,
            "exact restoration must never overwrite a concurrently recreated source"
        );
    }

    #[test]
    fn branch_first_restore_ignores_target_drift_then_full_proof_retains() {
        let mut fixture = IntegratedFixture::new("branch-first-restore-target-drift");
        let run = fixture.commit_intent("fixture:branch-first-restore-target-drift");
        let quarantine_root = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine_root);
        fixture.delete_source_ref(&run);

        let repository = fixture.repository.clone();
        let target_ref = run.target_ref.clone();
        let drift_oid = run.items[0].source_oid.clone();
        git_worktree::set_atomic_ref_pre_spawn_test_hook(move || {
            git(&repository, &["update-ref", &target_ref, &drift_oid]);
        });

        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert!(quarantine_root.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid,
            "target drift must not strand the source branch missing"
        );
        assert_eq!(
            git(&fixture.repository, &["rev-parse", &run.target_ref]),
            run.items[0].source_oid,
            "source restoration must not rewrite the drifted target"
        );
    }

    #[test]
    fn dangling_quarantine_remove_failure_restores_source_and_reattaches() {
        let mut fixture = IntegratedFixture::new("branch-first-remove-failure");
        let run = fixture.commit_intent("fixture:branch-first-remove-failure");
        let quarantine_root = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine_root);

        let injected = quarantine_root.join("late-untracked");
        set_before_dangling_remove_test_hook(move || {
            std::fs::write(injected, "retain\n").expect("inject late untracked file");
        });

        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert!(quarantine_root.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
        assert_eq!(
            git(&quarantine_root, &["symbolic-ref", "HEAD"]),
            run.items[0].source_ref,
            "failed removal compensation must reattach the exact quarantine"
        );
    }

    #[test]
    fn post_remove_source_recreation_fails_closed_without_second_delete() {
        let mut fixture = IntegratedFixture::new("branch-first-post-remove-recreation");
        let run = fixture.commit_intent("fixture:branch-first-post-remove-recreation");
        let quarantine_root = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine_root);

        let repository = fixture.repository.clone();
        let source_ref = run.items[0].source_ref.clone();
        let source_oid = run.items[0].source_oid.clone();
        set_after_dangling_remove_test_hook(move || {
            git(&repository, &["update-ref", &source_ref, &source_oid]);
        });

        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert!(!quarantine_root.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid,
            "post-remove source recreation must be retained, not deleted again"
        );
    }

    #[test]
    fn post_remove_target_drift_restores_source_without_overwrite() {
        let mut fixture = IntegratedFixture::new("branch-first-post-remove-target-drift");
        let run = fixture.commit_intent("fixture:branch-first-post-remove-target-drift");
        let quarantine_root = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine_root);

        let repository = fixture.repository.clone();
        let target_ref = run.target_ref.clone();
        let drift_oid = run.items[0].source_oid.clone();
        set_after_dangling_remove_test_hook(move || {
            git(&repository, &["update-ref", &target_ref, &drift_oid]);
        });

        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert!(!quarantine_root.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid,
            "q-absent compensation must compare-create the exact source branch"
        );
        assert_eq!(
            git(&fixture.repository, &["rev-parse", &run.target_ref]),
            run.items[0].source_oid,
            "compensation must not overwrite a drifted target ref"
        );
    }

    #[test]
    fn post_remove_path_recreation_does_not_block_exact_source_compensation() {
        let mut fixture = IntegratedFixture::new("branch-first-post-remove-path-drift");
        let run = fixture.commit_intent("fixture:branch-first-post-remove-path-drift");
        let quarantine_root = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine_root);

        let recreated_original = fixture.allocation.root.clone();
        let hook_root = recreated_original.clone();
        set_after_dangling_remove_test_hook(move || {
            std::fs::create_dir(&hook_root).expect("recreate original path after removal");
        });

        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert!(!quarantine_root.exists());
        assert!(recreated_original.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid,
            "path residue must not block the nondestructive source compare-create"
        );
    }

    #[test]
    fn canonical_marker_rejects_original_recreation_and_extra_same_ref_registration() {
        for case in ["original-recreated", "extra-registration"] {
            let mut fixture = IntegratedFixture::new(&format!("marker-hostile-{case}"));
            let run = fixture.commit_intent(&format!("fixture:marker-hostile-{case}"));
            let quarantine_root = fixture.move_to_quarantine(&run);
            fixture.persist_quarantine_marker(&run, &quarantine_root);

            let mut extra = None;
            if case == "original-recreated" {
                std::fs::create_dir(&fixture.allocation.root)
                    .expect("recreate original settlement path");
            } else {
                let other_session = Uuid::new_v4();
                let other_branch = format!("rsi/marker-hostile-other/{other_session}");
                let allocation = git_worktree::allocate(
                    &fixture.sandbox_base,
                    other_session,
                    &fixture.repository,
                    &fixture.source_oid,
                    Some(&other_branch),
                )
                .expect("allocate hostile second worktree");
                git(
                    &allocation.root,
                    &["symbolic-ref", "HEAD", &run.items[0].source_ref],
                );
                extra = Some(allocation);
            }

            let (outcome, _) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired,
                "{case}"
            );
            assert!(quarantine_root.exists(), "{case}");
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                run.items[0].source_oid,
                "{case}"
            );
            drop(extra);
        }
    }

    #[test]
    fn malformed_or_mismatched_marker_retains_quarantine_and_source_ref() {
        for case in ["noncanonical", "mismatched-run"] {
            let mut fixture = IntegratedFixture::new(&format!("marker-tamper-{case}"));
            let run = fixture.commit_intent(&format!("fixture:marker-tamper-{case}"));
            let quarantine_root = fixture.move_to_quarantine(&run);
            let marker = fixture.build_quarantine_marker(&run, &quarantine_root);
            let canonical = marker
                .to_canonical_json()
                .expect("canonical fixture marker");
            let retained = if case == "noncanonical" {
                format!(" {canonical}")
            } else {
                canonical.replacen(&run.run_id.to_string(), &Uuid::new_v4().to_string(), 1)
            };
            fixture
                .store
                .conn
                .execute(
                    "UPDATE source_worktree_settlement_items SET before_observation=?1
                      WHERE run_id=?2 AND session_id=?3",
                    rusqlite::params![
                        retained,
                        run.run_id.to_string(),
                        fixture.session_id.to_string()
                    ],
                )
                .expect("install hostile retained marker");

            let (outcome, _) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired,
                "{case}"
            );
            assert!(quarantine_root.exists(), "{case}");
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                run.items[0].source_oid,
                "{case}"
            );
        }
    }

    #[test]
    fn quarantine_alias_dependency_introduced_after_move_retains_candidate() {
        let mut fixture = IntegratedFixture::new("quarantine-alias-after-move");
        let run = fixture.commit_intent("fixture:quarantine-alias-after-move");
        let quarantine_root = fixture.move_to_quarantine(&run);
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        fixture
            .store
            .conn
            .execute(
                "INSERT INTO scheduled_jobs (
                    id,name,message,schedule_json,last_fired_at,next_fire_at,enabled,
                    working_dir,provider,model,project_id,created_at,updated_at,wake_mode,wake_session_id
                 ) VALUES (?1,'quarantine-alias','dependency','{}',NULL,?2,1,?3,NULL,NULL,NULL,?2,?2,'fresh',NULL)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    now,
                    quarantine_root.join("future-cwd").to_string_lossy().into_owned(),
                ],
            )
            .expect("insert post-move quarantine dependency");

        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert!(quarantine_root.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
    }

    #[test]
    fn persistent_quarantine_authority_failure_is_bounded_before_marker_or_removal() {
        let mut fixture = IntegratedFixture::new("quarantine-proof-failure-bound");
        let run = fixture.commit_intent("fixture:quarantine-proof-failure-bound");
        let quarantine_root = fixture.move_to_quarantine(&run);
        fail_next_quarantine_authority_proofs(QUARANTINE_AUTHORITY_PROOF_MAX_ATTEMPTS);

        let (outcome, _) = fixture.replay_run(run.run_id);

        assert_eq!(
            quarantine_authority_proof_attempt_count(),
            QUARANTINE_AUTHORITY_PROOF_MAX_ATTEMPTS
        );
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert!(outcome.receipt.items[0].before_observation.is_none());
        assert!(quarantine_root.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
    }

    #[test]
    fn legacy_worktree_phase_never_authorizes_a_new_ref_delete() {
        let mut canonical = IntegratedFixture::new("canonical-worktree-phase-source-present");
        let canonical_run =
            canonical.commit_intent("fixture:canonical-worktree-phase-source-present");
        let canonical_quarantine = canonical.move_to_quarantine(&canonical_run);
        canonical.persist_quarantine_marker(&canonical_run, &canonical_quarantine);
        canonical.remove_quarantine(&canonical_run, &canonical_quarantine);
        canonical.advance_to_worktree_removed(&canonical_run, None);
        let (canonical_outcome, _) = canonical.replay_run(canonical_run.run_id);
        assert_eq!(
            canonical_outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired,
            "an upgraded canonical WorktreeRemoved row has no q sentinel for a new deletion"
        );
        assert_eq!(
            git(
                &canonical.repository,
                &["rev-parse", &canonical_run.items[0].source_ref]
            ),
            canonical_run.items[0].source_oid
        );

        for before in [None, Some("clean exact worktree observed")] {
            let label = if before.is_some() { "prose" } else { "null" };
            let mut fixture = IntegratedFixture::new(&format!("legacy-present-{label}"));
            let run = fixture.commit_intent(&format!("fixture:legacy-present-{label}"));
            let quarantine_root = fixture.move_to_quarantine(&run);
            fixture.remove_quarantine(&run, &quarantine_root);
            fixture.advance_to_worktree_removed(&run, before);

            let (outcome, _) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired,
                "{label}"
            );
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                run.items[0].source_oid,
                "{label}"
            );
        }

        for before in [None, Some("clean exact worktree observed")] {
            let label = if before.is_some() { "prose" } else { "null" };
            let mut fixture = IntegratedFixture::new(&format!("legacy-ref-already-absent-{label}"));
            let run = fixture.commit_intent(&format!("fixture:legacy-ref-already-absent-{label}"));
            let quarantine_root = fixture.move_to_quarantine(&run);
            fixture.remove_quarantine(&run, &quarantine_root);
            fixture.delete_source_ref(&run);
            fixture.advance_to_worktree_removed(&run, before);
            let (outcome, _) = fixture.replay_run(run.run_id);
            assert_eq!(outcome.receipt.counts.settled, 1, "{label}");
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::Settled,
                "{label}"
            );
        }
    }

    #[test]
    fn settlement_session_layer_contains_no_force_prune_or_recursive_delete_fallback() {
        let source = include_str!("cohort_settlement.rs");
        let body = source
            .split_once("fn settle_one_item_locked(")
            .and_then(|(_, suffix)| suffix.split_once("const LEGACY_WORKTREE_REMOVAL_OBSERVATIONS"))
            .map(|(body, _)| body)
            .expect("settlement saga source segment");
        assert!(body.contains("remove_worktree_after_source_ref_delete_non_force_locked"));
        assert!(body.contains("compensate_branch_first_source_ref"));
        assert!(body.contains("worktree_paths_are_absent_and_unregistered"));
        for forbidden in ["--force", "worktree\", \"prune", "remove_dir_all"] {
            assert!(!body.contains(forbidden), "forbidden fallback: {forbidden}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_recovery_without_marker_preserves_source_ref_and_requires_recovery() {
        let directory = tempfile::tempdir().expect("recovery fixture directory");
        let repository = directory.path().join("repository");
        let sandbox_base = directory.path().join("sandboxes");
        std::fs::create_dir_all(&repository).expect("repository directory");
        create_private_sandbox_base(&sandbox_base);
        git(&repository, &["init", "-q", "-b", "main"]);
        git(
            &repository,
            &["config", "user.email", "recovery@example.test"],
        );
        git(&repository, &["config", "user.name", "Recovery Test"]);
        std::fs::write(repository.join("tracked"), "base\n").expect("base fixture file");
        git(&repository, &["add", "tracked"]);
        git(&repository, &["commit", "-qm", "base"]);
        let source_oid = git(&repository, &["rev-parse", "HEAD"]);

        let session_id = Uuid::new_v4();
        let custody_id = Uuid::new_v4();
        let branch = format!("rsi/{session_id}");
        let allocation = git_worktree::allocate(
            &sandbox_base,
            session_id,
            &repository,
            &source_oid,
            Some(&branch),
        )
        .expect("allocate recovery worktree");
        std::fs::write(repository.join("tracked"), "base\nintegrated\n")
            .expect("integrated target file");
        git(&repository, &["add", "tracked"]);
        git(&repository, &["commit", "-qm", "integrate source"]);
        let target = git_worktree::with_repository_mutation(&repository, || {
            git_worktree::observe_repository_target_locked(&repository)
        })
        .expect("observe recovery repository");

        let mut store =
            crate::store::Store::open(&directory.path().join("rsi.db")).expect("recovery Store");
        let mut session = crate::store::tests::make_test_session();
        session.id = session_id;
        session.status = SessionStatus::Completed;
        session.working_dir = target.canonical_repo_dir.clone();
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(allocation.root.clone());
        session.sandbox_branch = Some(branch.clone());
        session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .insert_session_with_custody(
                &session,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id,
                    canonical_repo_dir: target.canonical_repo_dir.to_string_lossy().into_owned(),
                    sandbox_root: allocation.root.to_string_lossy().into_owned(),
                    sandbox_branch: branch.clone(),
                    repository_identity: target.repository_identity.clone(),
                    source_commit: source_oid.clone(),
                    cause: CustodyCause::FreshLaunch,
                }),
            )
            .expect("seed recovery custody");

        let inventory = store
            .source_worktree_inventory(
                &target.repository_identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS + 1,
            )
            .expect("load recovery inventory");
        let audit = git_worktree::with_repository_mutation(&repository, || {
            build_audit_locked(
                &target.repository_identity,
                target
                    .canonical_repo_dir
                    .to_str()
                    .expect("UTF-8 canonical repository"),
                &sandbox_base,
                inventory,
                &HashSet::new(),
                &[],
            )
        })
        .expect("audit recovery candidate");
        assert!(audit.report.applyable);
        assert_eq!(audit.eligible.len(), 1);
        let target_ref = audit
            .report
            .target_ref
            .clone()
            .expect("recovery target ref");
        let target_oid = audit
            .report
            .target_oid
            .as_ref()
            .expect("recovery target oid")
            .as_str()
            .to_string();
        let eligible = &audit.eligible[0];
        let run_id = Uuid::new_v4();
        let run = NewSettlementRun {
            run_id,
            repository_identity: target.repository_identity.clone(),
            canonical_repo_dir: target.canonical_repo_dir.to_string_lossy().into_owned(),
            target_ref: target_ref.clone(),
            target_oid: target_oid.clone(),
            plan_digest: audit.report.plan_digest.as_str().to_string(),
            idempotency_key: "fixture:startup-recovery".into(),
            authorization_digest: hash_bytes(
                audit
                    .report
                    .authorization_phrase
                    .as_deref()
                    .expect("recovery authorization phrase")
                    .as_bytes(),
            )
            .as_str()
            .to_string(),
            request_fingerprint: hash_bytes(b"recovery request").as_str().to_string(),
            observed_count: audit.report.counts.observed,
            retained_count: audit.report.counts.retained,
            items: vec![NewSettlementItem {
                session_id,
                original_status: eligible.inventory.status.clone().expect("eligible status"),
                original_updated_at: eligible
                    .inventory
                    .session_updated_at
                    .clone()
                    .expect("eligible updated_at"),
                custody_id,
                custody_generation: eligible.inventory.generation,
                canonical_repo_dir: eligible.inventory.canonical_repo_dir.clone(),
                sandbox_root: eligible.inventory.sandbox_root.clone(),
                sandbox_branch: eligible.inventory.sandbox_branch.clone(),
                repository_identity: eligible.inventory.repository_identity.clone(),
                source_ref: eligible.source_ref.clone(),
                source_oid: eligible.source_oid.clone(),
                target_oid,
                evidence_digest: eligible.evidence_digest.clone(),
                clean_state_digest: eligible.clean_state_digest.clone(),
                reserved_effects: eligible.inventory.reserved_effects,
                active_effects: eligible.inventory.active_effects,
                participant_count: eligible.inventory.participant_count,
            }],
        };
        assert!(matches!(
            store
                .insert_source_worktree_settlement_run(&run)
                .expect("commit recovery intent"),
            InsertSettlementRunOutcome::Inserted
        ));

        let quarantine_root =
            git_worktree::derive_settlement_quarantine_path(&allocation.root, run_id, session_id)
                .expect("derive recovery fixture quarantine");
        git_worktree::with_repository_mutation(&repository, || {
            let proof = git_worktree::prepare_settlement_quarantine_path(
                &allocation.root,
                run_id,
                session_id,
            )?;
            git_worktree::move_worktree_to_quarantine_non_force_locked(
                &repository,
                &proof,
                &eligible.source_ref,
                &eligible.source_oid,
            )?;
            git_worktree::remove_worktree_non_force_locked(
                &repository,
                &allocation.root,
                &quarantine_root,
                &eligible.source_ref,
                &eligible.source_oid,
            )
        })
        .expect("simulate completed worktree-removal effect");
        assert_eq!(
            store
                .get_source_worktree_settlement_run(run_id)
                .expect("read effect-before-phase receipt")
                .expect("effect-before-phase receipt retained")
                .items[0]
                .phase,
            SourceWorktreeSettlementPhaseV1::IntentCommitted
        );
        assert!(!allocation.root.exists());
        assert!(
            git_worktree::with_repository_mutation(&repository, || {
                git_worktree::resolve_ref_locked(&repository, &eligible.source_ref)
            })
            .expect("observe retained source ref")
            .is_some()
        );

        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            store,
            false,
            directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
            sandbox_base,
        )
        .expect("construct recovery manager");
        let mut orphan = startup_owned_sleep(session_id);
        let fault = super::super::reaper::scoped_startup_settlement_reap_failure(session_id);
        manager
            .restore_sessions()
            .await
            .expect("uncertain orphan proof leaves startup recovery fenced");
        orphan.assert_alive("failed proof must not claim or mutate the orphan");
        let deferred = manager
            .get_source_worktree_settlement_run(run_id)
            .await
            .expect("read deferred receipt")
            .expect("deferred receipt retained");
        assert_eq!(
            deferred.items[0].phase,
            SourceWorktreeSettlementPhaseV1::IntentCommitted
        );
        assert!(
            git_worktree::with_repository_mutation(&repository, || {
                git_worktree::resolve_ref_locked(&repository, &eligible.source_ref)
            })
            .expect("observe source ref after deferred recovery")
            .is_some()
        );

        drop(fault);

        manager
            .restore_sessions()
            .await
            .expect("next startup reaps orphan then resumes retained intent");
        let recovery_receipt = manager
            .get_source_worktree_settlement_run(run_id)
            .await
            .expect("read recovered receipt")
            .expect("recovered receipt retained");
        assert_eq!(
            recovery_receipt.counts.recovery_required, 1,
            "{recovery_receipt:#?}"
        );
        assert_eq!(
            recovery_receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert!(
            git_worktree::with_repository_mutation(&repository, || {
                git_worktree::resolve_ref_locked(&repository, &eligible.source_ref)
            })
            .expect("observe preserved source ref")
            .is_some()
        );
        orphan.wait_signalled("reaped startup orphan");

        manager
            .recover_source_worktree_settlements()
            .await
            .expect("replay terminal recovery is inert");
        assert_eq!(
            manager
                .get_source_worktree_settlement_run(run_id)
                .await
                .expect("read replayed receipt")
                .expect("replayed receipt retained"),
            recovery_receipt
        );
        let retained = manager
            .store
            .lock()
            .await
            .get_session(session_id)
            .expect("read retained Session")
            .expect("retained Session exists");
        assert_eq!(retained.status, SessionStatus::Completed);
        assert_eq!(
            retained.sandbox_root.as_deref(),
            Some(allocation.root.as_path())
        );
        assert_eq!(retained.sandbox_branch.as_deref(), Some(branch.as_str()));
        assert_eq!(
            retained.sandbox_cleanup_state,
            Some(SandboxCleanupState::Live)
        );
    }
}
