//! Audited repository cohort settlement and retained recovery.
//!
//! Authority comes exclusively from V94 custody rows plus fresh local Git
//! observations. Audit is a zero-write operation. Apply persists a complete
//! intent before its first Git effect and advances each item monotonically.

use super::SessionManager;
use super::agent_verbs::AgentControlHandle;
use super::settlement_audit_fence::{AuditInputs, FenceDecision};
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
use crate::store::cohort_settlement::{agent_settlement_refusal, normalize_agent_settlement_error};
use crate::store::custody_lock_order::BlockingStoreLockExt;
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

/// How long the agent audit waits for the repository mutex before it answers
/// `source_settlement_busy` (#2622). Well inside the 30 s agent RPC deadline,
/// so a running settlement saga yields a typed answer, never a transport
/// timeout. The deadline itself is unchanged.
const AGENT_AUDIT_REPOSITORY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Upper bound on how long the busy answer waits for the Store to name the
/// holder (#2662). Added to [`AGENT_AUDIT_REPOSITORY_WAIT`] it stays well inside
/// the 30 s agent RPC deadline; past it the answer carries `holder_known: false`.
const AGENT_AUDIT_HOLDER_LOOKUP_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Run `read` under the Store lock acquired by a bounded `try_lock` poll;
/// `None` when the Store stayed contended for `wait` or the read failed.
fn bounded_store_read<T>(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    wait: std::time::Duration,
    read: impl FnOnce(&crate::store::Store) -> Result<T>,
) -> Option<T> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        if let Ok(guard) = store.try_lock() {
            return read(&guard).ok();
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Suggested re-ask delay carried by the busy answer.
const SOURCE_SETTLEMENT_BUSY_RETRY_AFTER_SECS: u64 = 30;

/// How long a detached apply (#2866, #2869) holds its caller for the whole
/// background task: the cohort audit that binds `plan_digest` and the durable
/// intent insert. Well inside the 30 s agent RPC deadline. Past it the caller
/// gets the typed `source_settlement_apply_pending` answer while the same task
/// keeps going; an exact retry joins it and receives the run (or the stale or
/// refused outcome) once it has one.
const AGENT_APPLY_AUDIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Suggested re-ask delay carried by the pending answer.
const SOURCE_SETTLEMENT_APPLY_PENDING_RETRY_AFTER_SECS: u64 = 5;

/// A finished-but-unclaimed failure is dropped after this long, so an apply
/// nobody retried never pins its idempotency key in the registry.
const PENDING_APPLY_FAILURE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// The typed answer for a detached apply whose audit has not finished inside
/// [`AGENT_APPLY_AUDIT_BUDGET`]. No deletion started: no root is touched
/// without a fresh audit that matches `plan_digest`.
fn source_settlement_apply_pending(
    repository_identity: &str,
    waited: std::time::Duration,
) -> DaemonError {
    DaemonError::StructuredRpc {
        rpc_code: rsi_common::rpc::INVALID_PARAMS,
        message: "source_settlement_apply_pending".into(),
        data: serde_json::json!({
            "code": "source_settlement_apply_pending",
            "disposition": "pending",
            "reason": format!(
                "the audit that binds plan_digest is still running after {}s and no run is recorded \
                 yet; no root is touched without a fresh audit that matches plan_digest. The apply \
                 continues in the daemon: retry this exact request with the same idempotency_key in \
                 about {}s to receive the run, or its stale or refused outcome",
                waited.as_secs(),
                SOURCE_SETTLEMENT_APPLY_PENDING_RETRY_AFTER_SECS
            ),
            "repository_identity": repository_identity,
            "run_recorded": false,
            "waited_secs": waited.as_secs(),
            "retry_after_secs": SOURCE_SETTLEMENT_APPLY_PENDING_RETRY_AFTER_SECS,
        }),
    }
}

/// A clonable copy of the error a pending apply ended with, so every waiter and
/// the next retry see the same typed outcome.
#[derive(Clone, Debug)]
enum PendingApplyFailure {
    InvalidParam(String),
    Structured {
        rpc_code: i32,
        message: String,
        data: serde_json::Value,
    },
    Other(String),
}

impl PendingApplyFailure {
    fn capture(error: &DaemonError) -> Self {
        match error {
            DaemonError::InvalidParam(reason) => Self::InvalidParam(reason.clone()),
            DaemonError::StructuredRpc {
                rpc_code,
                message,
                data,
            } => Self::Structured {
                rpc_code: *rpc_code,
                message: message.clone(),
                data: data.clone(),
            },
            other => Self::Other(other.to_string()),
        }
    }

    fn into_error(self) -> DaemonError {
        match self {
            Self::InvalidParam(reason) => DaemonError::InvalidParam(reason),
            Self::Structured {
                rpc_code,
                message,
                data,
            } => DaemonError::StructuredRpc {
                rpc_code,
                message,
                data,
            },
            Self::Other(reason) => DaemonError::Process(reason),
        }
    }
}

type PendingApplyOutcome =
    Option<std::result::Result<SourceWorktreeSettlementRunV1, PendingApplyFailure>>;

/// One audit-then-record task this process runs for an agent apply that has no
/// run yet (#2869).
struct PendingApply {
    fingerprint: String,
    started: std::time::Instant,
    outcome: tokio::sync::watch::Receiver<PendingApplyOutcome>,
}

type PendingApplyKey = (String, String);

fn pending_applies() -> &'static std::sync::Mutex<HashMap<PendingApplyKey, PendingApply>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<HashMap<PendingApplyKey, PendingApply>>> =
        std::sync::OnceLock::new();
    PENDING.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn remove_pending_apply(key: &PendingApplyKey) {
    pending_applies()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(key);
}

/// Join the task already running for this idempotency key (`None` sender), or
/// register a new one and hand back the sender its task publishes through.
/// `Err` is a key that already belongs to a different request.
fn join_or_register_pending_apply(
    key: &PendingApplyKey,
    fingerprint: &str,
) -> Result<(
    Option<tokio::sync::watch::Sender<PendingApplyOutcome>>,
    tokio::sync::watch::Receiver<PendingApplyOutcome>,
)> {
    let mut pending = pending_applies()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pending.retain(|_, entry| {
        !(entry.outcome.borrow().is_some() && entry.started.elapsed() > PENDING_APPLY_FAILURE_TTL)
    });
    if let Some(existing) = pending.get(key) {
        if existing.fingerprint != fingerprint {
            return Err(DaemonError::InvalidParam(
                "idempotency key already belongs to a different settlement request".into(),
            ));
        }
        return Ok((None, existing.outcome.clone()));
    }
    let (sender, receiver) = tokio::sync::watch::channel(None);
    pending.insert(
        key.clone(),
        PendingApply {
            fingerprint: fingerprint.to_string(),
            started: std::time::Instant::now(),
            outcome: receiver.clone(),
        },
    );
    Ok((Some(sender), receiver))
}

/// Wait up to `budget` for a pending apply's outcome. A failure is claimed by
/// whoever sees it first (the entry is removed, so the next request audits
/// afresh); a recorded run needs no entry, the store replays it by key.
async fn await_pending_apply(
    key: &PendingApplyKey,
    mut receiver: tokio::sync::watch::Receiver<PendingApplyOutcome>,
    budget: std::time::Duration,
) -> Result<SourceWorktreeSettlementRunV1> {
    let waited = tokio::time::timeout(budget, async {
        receiver
            .wait_for(Option::is_some)
            .await
            .map(|outcome| outcome.clone())
    })
    .await;
    match waited {
        Err(_) => Err(source_settlement_apply_pending(&key.0, budget)),
        Ok(Err(_)) => {
            remove_pending_apply(key);
            Err(DaemonError::Process(
                "detached settlement audit ended without an outcome".into(),
            ))
        }
        Ok(Ok(Some(Ok(receipt)))) => Ok(receipt),
        Ok(Ok(Some(Err(failure)))) => {
            remove_pending_apply(key);
            Err(failure.into_error())
        }
        Ok(Ok(None)) => Err(source_settlement_apply_pending(&key.0, budget)),
    }
}

/// The typed answer for an audit that could not take the repository mutex in
/// time: no plan, no digest, no authorization and no write. Names the holder
/// the repository guard itself recorded (#2663): the settlement run mutating a
/// root, or the label of another operation. The holder is never derived from
/// journal chronology, which orders runs by creation and not by lock ownership.
fn source_settlement_busy(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    repository_identity: &str,
    holder: Option<git_worktree::RepositoryHolder>,
    waited: std::time::Duration,
) -> DaemonError {
    let holder_known = holder.is_some();
    let holder_run = match &holder {
        Some(git_worktree::RepositoryHolder::SettlementRun(run_id)) => Some(*run_id),
        _ => None,
    };
    let holder_operation = match &holder {
        Some(git_worktree::RepositoryHolder::SettlementRun(_)) => Some("settlement_run"),
        Some(git_worktree::RepositoryHolder::Operation(label)) => Some(*label),
        None => None,
    };
    // The run's origin and counts are detail only: a Store that stays
    // contended past a short budget must not outlast the transport deadline
    // behind a Store holder that is itself waiting on the held repository
    // (#2662), so the answer keeps the holder's identity and drops the detail.
    let holder_wait = AGENT_AUDIT_HOLDER_LOOKUP_WAIT.min(waited);
    let detail = holder_run.and_then(|run_id| {
        bounded_store_read(store, holder_wait, |store| {
            Ok(store
                .get_source_worktree_settlement_run(run_id)?
                .and_then(|run| run.validate_wire().ok()))
        })
        .flatten()
    });
    let (holder_origin, holder_started_at, holder_counts) = match &detail {
        Some(run) => (
            serde_json::to_value(run.attribution.as_ref().map(|a| &a.origin))
                .unwrap_or(serde_json::Value::Null),
            serde_json::json!(run.created_at),
            serde_json::to_value(&run.counts).unwrap_or(serde_json::Value::Null),
        ),
        None => (
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        ),
    };
    let reason = match (&holder_run, holder_operation) {
        (Some(run_id), _) => format!(
            "settlement run {run_id} holds the repository; the audit waited {}s and wrote nothing. \
             Re-run the audit after it ends (about {}s from now); do not apply without a fresh audit",
            waited.as_secs(),
            SOURCE_SETTLEMENT_BUSY_RETRY_AFTER_SECS
        ),
        (None, Some(label)) => format!(
            "another repository operation ({label}) holds the repository; the audit waited {}s \
             and wrote nothing. Re-run the audit in about {}s",
            waited.as_secs(),
            SOURCE_SETTLEMENT_BUSY_RETRY_AFTER_SECS
        ),
        (None, None) => format!(
            "the repository is held and the holder could not be observed; the audit waited \
             {}s and wrote nothing. Re-run the audit in about {}s",
            waited.as_secs(),
            SOURCE_SETTLEMENT_BUSY_RETRY_AFTER_SECS
        ),
    };
    let holder_run_id = holder_run.map_or(serde_json::Value::Null, |id| {
        serde_json::json!(id.to_string())
    });
    DaemonError::StructuredRpc {
        rpc_code: rsi_common::rpc::INVALID_PARAMS,
        message: "source_settlement_busy".into(),
        data: serde_json::json!({
            "code": "source_settlement_busy",
            "disposition": "busy",
            "reason": reason,
            "repository_identity": repository_identity,
            "holder_run_id": holder_run_id,
            "holder_operation": holder_operation,
            "holder_origin": holder_origin,
            "holder_started_at": holder_started_at,
            "holder_counts": holder_counts,
            "holder_known": holder_known,
            "waited_secs": waited.as_secs(),
            "retry_after_secs": SOURCE_SETTLEMENT_BUSY_RETRY_AFTER_SECS,
            "writes": 0,
        }),
    }
}

enum AuditStart {
    ReceiptOnly(SourceWorktreeCohortAuditV1),
    Live(String),
}

enum ApplyStart {
    Fresh(AuditSnapshot),
    /// A run this call just recorded from its audit (#2866): the intent is
    /// durable already, so preparation only loads the journal.
    Inserted(SourceWorktreeSettlementRunV1),
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

/// A terminal `recovery_required` item that apply replay and startup recovery
/// may reopen: an old first-proof fence, the historical `root_busy` failure, or
/// a ref-delete/worktree-residue fence carrying removal authority. Mirrors the
/// store's reopen predicates (`RETRY_FENCE_SQL`), which remain authoritative.
fn item_has_revivable_fence(
    item: &rsi_common::cohort_settlement::SourceWorktreeSettlementItemV1,
) -> bool {
    if item.phase != SourceWorktreeSettlementPhaseV1::RecoveryRequired {
        return false;
    }
    if item.refusal_code == Some(SourceWorktreeSettlementRefusalV1::RecoveryProofFailed) {
        // The retained-intent observation is revivable only while no effect
        // started: the store's `RETAINED_INTENT_FENCE_SQL` also needs a NULL
        // `before_observation` (the receipt carries no effect counts).
        return match item.after_observation.as_deref() {
            Some(
                "quarantined worktree failed its removal-authority proof"
                | "settlement attempt failed: sandbox_custody:root_busy",
            ) => true,
            Some(
                "retained intent followed an earlier terminal stop and cannot prove no prior effect",
            ) => item.before_observation.is_none(),
            _ => false,
        };
    }
    item.before_observation.is_some()
        && matches!(
            item.refusal_code,
            Some(
                SourceWorktreeSettlementRefusalV1::RefDeleteFailed
                    | SourceWorktreeSettlementRefusalV1::WorktreeResidue
            )
        )
}

fn effect_capable_replay_session_ids(receipt: &SourceWorktreeSettlementRunV1) -> Vec<Uuid> {
    let mut session_ids = receipt
        .items
        .iter()
        // Reproof can reopen an old first-proof fence while holding these guards.
        .filter(|item| {
            phase_can_still_apply_git_effect(&item.phase) || item_has_revivable_fence(item)
        })
        .map(|item| item.session_id)
        .collect::<Vec<_>>();
    session_ids.sort_unstable();
    session_ids.dedup();
    session_ids
}

const OWNER_BECAME_ACTIVE: &str = "source-worktree settlement owner became active after audit";

/// The audit that binds an apply's `plan_digest` must be applyable and current.
fn check_snapshot_authorizes_apply(
    snapshot: &AuditSnapshot,
    params: &ApplySourceWorktreeCohortParams,
) -> Result<()> {
    if !snapshot.report.applyable {
        return Err(DaemonError::InvalidParam(
            snapshot
                .report
                .refusal
                .clone()
                .unwrap_or_else(|| "source-worktree cohort has no eligible roots".into()),
        ));
    }
    if snapshot.report.plan_digest != params.plan_digest {
        return Err(DaemonError::InvalidParam(
            "source-worktree audit digest is stale".into(),
        ));
    }
    Ok(())
}

/// Everything one settlement run needs after the request has been validated,
/// owned so the driver can outlive the RPC future.
struct SettlementDriver {
    sandbox_base: PathBuf,
    store: Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    event_bus: Arc<crate::bus::EventBus>,
    identity: String,
    key: String,
    plan_digest: String,
    authorization_digest: String,
    request_fingerprint: String,
    agent_caller: Option<Uuid>,
    automatic_config: Option<Arc<crate::config::RuntimeConfig>>,
}

impl SettlementDriver {
    /// Attached apply: the caller already holds the spawn guards and passed the
    /// pre-effect checks, and waits for the run's final receipt.
    async fn run(
        self,
        start: ApplyStart,
        spawn_guards: Vec<super::spawn_single_flight::SpawnGuard>,
    ) -> Result<SourceWorktreeSettlementRunV1> {
        let driver = Arc::new(self);
        let blocking = Arc::clone(&driver);
        let outcome =
            tokio::task::spawn_blocking(move || blocking.drive_blocking(start, spawn_guards, true))
                .await
                .map_err(|error| DaemonError::Process(error.to_string()))??;
        driver.finish(outcome).await
    }

    /// Detached apply (#2866): the run is recorded already. Take the spawn
    /// guards and run the pre-effect checks here, so contention on a root's
    /// guard delays the run and never the caller that received its id.
    async fn run_detached(self, start: ApplyStart) -> Result<()> {
        let (run_id, mut session_ids) = match &start {
            ApplyStart::Inserted(receipt) | ApplyStart::Replay(receipt) => {
                (receipt.run_id, effect_capable_replay_session_ids(receipt))
            }
            ApplyStart::Fresh(_) => {
                return Err(DaemonError::Store(
                    "detached apply reached an unrecorded start".into(),
                ));
            }
        };
        session_ids.sort_unstable();
        session_ids.dedup();
        let mut spawn_guards = Vec::with_capacity(session_ids.len());
        for &session_id in &session_ids {
            spawn_guards.push(super::spawn_single_flight::acquire_spawn_guard(session_id).await);
        }
        let owner_active = {
            let active = self.active.read().await;
            session_ids
                .iter()
                .any(|session_id| active.contains_key(session_id))
        };
        let precheck = if owner_active {
            Err(DaemonError::InvalidParam(OWNER_BECAME_ACTIVE.into()))
        } else {
            let candidates = session_ids.clone();
            tokio::task::spawn_blocking(move || {
                super::reaper::prove_startup_settlement_orphan_scan_readable(&candidates)
            })
            .await
            .map_err(|error| DaemonError::Process(error.to_string()))
            .and_then(|result| result)
        };
        if let Err(error) = precheck {
            // A replayed run keeps its recorded state for the next retry. A run
            // this call recorded has no effect yet: close its untouched roots
            // with the observation so the failure is on the receipt.
            if matches!(start, ApplyStart::Inserted(_)) {
                let store = Arc::clone(&self.store);
                let closing = error.to_string();
                tokio::task::spawn_blocking(move || {
                    let guard = store.blocking_lock_checked()?;
                    retain_failed_settlement_items(
                        &guard,
                        run_id,
                        None,
                        &DaemonError::InvalidParam(closing),
                    )
                })
                .await
                .map_err(|join| DaemonError::Process(join.to_string()))??;
            }
            return Err(error);
        }
        let driver = Arc::new(self);
        let blocking = Arc::clone(&driver);
        let outcome = tokio::task::spawn_blocking(move || {
            blocking.drive_blocking(start, spawn_guards, false)
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))??;
        driver.finish(outcome).await.map(|_| ())
    }

    async fn finish(&self, outcome: ApplyOutcome) -> Result<SourceWorktreeSettlementRunV1> {
        SessionManager::project_settlement(
            &self.active,
            &self.completed,
            &self.event_bus,
            &outcome.settled_ids,
        )
        .await;
        outcome.receipt.validate_wire().map_err(DaemonError::Store)
    }

    /// `register_in_flight` marks the run this process drives; the detached
    /// path registered it before returning its id.
    fn drive_blocking(
        &self,
        start: ApplyStart,
        spawn_guards: Vec<super::spawn_single_flight::SpawnGuard>,
        register_in_flight: bool,
    ) -> Result<ApplyOutcome> {
        let _spawn_guards = spawn_guards;
        let mut in_flight = match &start {
            ApplyStart::Replay(receipt) if register_in_flight => {
                Some(mark_settlement_run_in_flight(receipt.run_id))
            }
            _ => None,
        };
        let mut on_prepared = |run_id: Uuid| {
            if register_in_flight && in_flight.is_none() {
                in_flight = Some(mark_settlement_run_in_flight(run_id));
            }
        };
        let origin = PathBuf::from(match &start {
            ApplyStart::Fresh(snapshot) => snapshot.report.canonical_repo_dir.clone(),
            ApplyStart::Inserted(receipt) | ApplyStart::Replay(receipt) => {
                receipt.canonical_repo_dir.clone()
            }
        });
        // Lock order (#2081): the provider-cwd exclusion is taken per
        // root, outside the repository mutex, which is re-entered per root.
        let result = apply_with_root_scheduling(
            &origin,
            start,
            &self.identity,
            &self.key,
            &self.plan_digest,
            &self.authorization_digest,
            &self.request_fingerprint,
            &self.sandbox_base,
            &self.store,
            &self.active,
            &self.completed,
            true,
            self.agent_caller,
            self.automatic_config.as_deref(),
            Some(&mut on_prepared),
        );
        // No repository/root lock here: final journal writes may wait
        // for Store without a lock-order cycle. Only uncertain effects
        // warrant a recovery fence; lock waits stay replayable.
        let guard = self.store.blocking_lock_checked()?;
        let receipt = guard.replay_source_worktree_settlement_run(
            &self.identity,
            &self.key,
            &self.request_fingerprint,
        )?;
        if let Some(receipt) = receipt {
            if let Err(error) = &result {
                tracing::warn!(run_id=%receipt.run_id, error=%error,
                    "Settlement apply retained its failed attempt outcome");
                retain_failed_settlement_items(&guard, receipt.run_id, None, error)?;
            }
            flush_settlement_surrenders(&guard, receipt.run_id);
            let receipt = guard
                .get_source_worktree_settlement_run(receipt.run_id)?
                .ok_or_else(|| DaemonError::Store("settlement receipt disappeared".into()))?;
            let settled_ids = receipt
                .items
                .iter()
                .filter(|item| item.phase == SourceWorktreeSettlementPhaseV1::Settled)
                .map(|item| item.session_id)
                .collect();
            Ok(ApplyOutcome {
                receipt,
                settled_ids,
            })
        } else {
            result
        }
    }
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
    eligible_items: &'a [&'a SourceWorktreeAuditItemV1],
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

impl AgentControlHandle {
    pub async fn agent_list_source_worktree_cohorts(
        &self,
        caller: Uuid,
    ) -> Result<Vec<SourceWorktreeCohortSummaryV1>> {
        let store = self.store.lock().await;
        store.agent_source_worktree_projects(caller)?;
        let mut allowed = Vec::new();
        for cohort in store.list_source_worktree_cohorts()? {
            match store.authorize_agent_source_worktree_cohort(caller, &cohort.repository_identity)
            {
                Ok(()) => allowed.push(cohort.validate_wire().map_err(DaemonError::Store)?),
                Err(DaemonError::StructuredRpc { data, .. })
                    if data["code"] == "storage_control_not_in_scope" => {}
                Err(error) => return Err(error),
            }
        }
        Ok(allowed)
    }

    pub async fn agent_audit_source_worktree_cohort_view(
        &self,
        caller: Uuid,
        params: rsi_common::cohort_settlement::AgentAuditSourceWorktreeCohortParams,
    ) -> Result<serde_json::Value> {
        let audit = self
            .agent_audit_source_worktree_cohort(
                caller,
                rsi_common::cohort_settlement::AuditSourceWorktreeCohortParams {
                    repository_identity: params.repository_identity,
                },
            )
            .await?;
        Ok(audit.into_agent_view(params.fields)?)
    }

    pub(crate) async fn agent_audit_source_worktree_cohort(
        &self,
        caller: Uuid,
        params: rsi_common::cohort_settlement::AuditSourceWorktreeCohortParams,
    ) -> Result<SourceWorktreeCohortAuditV1> {
        self.agent_audit_source_worktree_cohort_within(caller, params, AGENT_AUDIT_REPOSITORY_WAIT)
            .await
    }

    /// The agent audit with an explicit repository wait (#2622). Past `wait`
    /// the audit answers `source_settlement_busy` instead of queueing behind a
    /// running settlement until the transport deadline.
    pub(crate) async fn agent_audit_source_worktree_cohort_within(
        &self,
        caller: Uuid,
        params: rsi_common::cohort_settlement::AuditSourceWorktreeCohortParams,
        wait: std::time::Duration,
    ) -> Result<SourceWorktreeCohortAuditV1> {
        params.validate().map_err(|reason| {
            agent_settlement_refusal("storage_control_invalid_request", &reason)
        })?;
        self.store
            .lock()
            .await
            .authorize_agent_source_worktree_cohort(caller, &params.repository_identity)?;
        let result = self
            .topology_manager()?
            .audit_source_worktree_cohort_as(
                params.repository_identity.clone(),
                Some(caller),
                Some(wait),
            )
            .await
            .map_err(normalize_agent_settlement_error)?;
        // Require current seat and repository admission before returning the observation.
        let store = self.store.lock().await;
        store.authorize_agent_source_worktree_cohort(caller, &params.repository_identity)?;
        // #2690: the run snapshot names roots across the cohort; the caller sees
        // only the ones inside its own coverage (current ownership).
        let mut result = result;
        store.scope_agent_retained_roots(caller, &mut result.last_run_retained_roots)?;
        Ok(result)
    }

    pub async fn agent_apply_source_worktree_cohort(
        &self,
        caller: Uuid,
        params: ApplySourceWorktreeCohortParams,
    ) -> Result<SourceWorktreeSettlementRunV1> {
        params.validate().map_err(|reason| {
            agent_settlement_refusal("storage_control_invalid_request", &reason)
        })?;
        self.store
            .lock()
            .await
            .authorize_agent_source_worktree_cohort(caller, &params.repository_identity)?;
        // #2789: an agent apply returns once the run is durably recorded and
        // keeps applying in the daemon, so a cohort of any size never holds the
        // caller's tool call past its turn.
        self.topology_manager()?
            .apply_source_worktree_cohort_as(params, Some(caller), None, true)
            .await
            .map_err(normalize_agent_settlement_error)
    }
}

/// Settlement runs this process is driving (#2789). A detached agent apply
/// registers its run once the durable intent exists, so an exact retry that
/// arrives while the run is still settling answers with the stored receipt
/// instead of starting a second driver over the same roots.
fn settlement_runs_in_flight() -> &'static std::sync::Mutex<HashSet<Uuid>> {
    static RUNS: std::sync::OnceLock<std::sync::Mutex<HashSet<Uuid>>> = std::sync::OnceLock::new();
    RUNS.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

fn settlement_run_in_flight(run_id: Uuid) -> bool {
    settlement_runs_in_flight()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&run_id)
}

/// Removes the run from the in-flight set when the driver ends, however it ends.
struct SettlementRunInFlight(Uuid);

impl Drop for SettlementRunInFlight {
    fn drop(&mut self) {
        settlement_runs_in_flight()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// Claim the run for one driver: `None` when this process already drives it.
/// The claim is built only once the insert succeeded and the set's lock is
/// released: an eager `then_some` built a claim on a refusal too, whose drop
/// re-locked the held set (a self-deadlock) and would have released the
/// running driver's claim.
fn try_mark_settlement_run_in_flight(run_id: Uuid) -> Option<SettlementRunInFlight> {
    let inserted = settlement_runs_in_flight()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(run_id);
    inserted.then(|| SettlementRunInFlight(run_id))
}

fn mark_settlement_run_in_flight(run_id: Uuid) -> SettlementRunInFlight {
    settlement_runs_in_flight()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(run_id);
    SettlementRunInFlight(run_id)
}

fn automatic_settlement_due(
    enabled: bool,
    threshold_gb: u64,
    free_bytes: u64,
    total_bytes: u64,
) -> bool {
    enabled && free_bytes < crate::sandbox::effective_min_free_bytes(threshold_gb, total_bytes)
}

#[cfg(test)]
fn settlement_disk_samples() -> &'static std::sync::Mutex<HashMap<PathBuf, (u64, u64)>> {
    static SAMPLES: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, (u64, u64)>>> =
        std::sync::OnceLock::new();
    SAMPLES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

// One filesystem observation supplies both available and total bytes.
fn settlement_disk_space(path: &Path) -> Result<(u64, u64)> {
    #[cfg(test)]
    if let Some(bytes) = settlement_disk_samples().lock().unwrap().get(path).copied() {
        return Ok(bytes);
    }

    #[cfg(unix)]
    {
        let stats = nix::sys::statvfs::statvfs(path).map_err(|e| {
            DaemonError::Process(format!("source settlement disk sample failed: {e}"))
        })?;
        let fragment_size = stats.fragment_size() as u64;
        Ok((
            (stats.blocks_available() as u64).saturating_mul(fragment_size),
            (stats.blocks() as u64).saturating_mul(fragment_size),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(DaemonError::Process(
            "automatic source settlement disk sampling unsupported on this platform".into(),
        ))
    }
}

/// What one automatic settlement pass did and cost (hub #2940): logged once per
/// pass, never per root. Evidence only; no settlement decision reads it. The CPU
/// and read figures come from a whole-process meter, so they bound the pass from
/// above (another lane running meanwhile is counted too); read them against the
/// idle baseline.
#[derive(Default)]
struct AutoSettlementTally {
    cohorts: u64,
    /// Retained automatic intents the pass resumed.
    resumed: u64,
    audited: u64,
    /// Cohorts the audit fence skipped: unchanged since an audit that found
    /// nothing to apply, inside the backoff (hub #3031).
    audit_fenced: u64,
    audit_retained: u64,
    applied: u64,
    apply_retained: u64,
    settled: u64,
    refused: u64,
    audit_wall: std::time::Duration,
    apply_wall: std::time::Duration,
}

impl AutoSettlementTally {
    fn log(&self, pass: &rsid_core::pass_cost::Cost, completed: bool) {
        let did_work = self.audited + self.resumed + self.applied > 0;
        if !did_work {
            tracing::debug!(
                cohorts = self.cohorts,
                audit_fenced = self.audit_fenced,
                completed,
                "Automatic source settlement pass found nothing to audit"
            );
            return;
        }
        tracing::info!(
            cohorts = self.cohorts,
            resumed = self.resumed,
            audited = self.audited,
            audit_fenced = self.audit_fenced,
            audit_retained = self.audit_retained,
            applied = self.applied,
            apply_retained = self.apply_retained,
            settled = self.settled,
            refused = self.refused,
            completed,
            wall_ms = pass.wall_ms(),
            audit_wall_ms = u64::try_from(self.audit_wall.as_millis()).unwrap_or(u64::MAX),
            apply_wall_ms = u64::try_from(self.apply_wall.as_millis()).unwrap_or(u64::MAX),
            process_cpu_ms = pass.cpu_ms(),
            process_read_kib = pass.read_kib(),
            process_read_calls = pass.read_calls.unwrap_or(0),
            "Automatic source settlement pass finished"
        );
    }
}

fn automatic_settlement_pressure(
    config: &crate::config::RuntimeConfig,
    base: &Path,
) -> Result<bool> {
    use std::sync::atomic::Ordering;
    if !config
        .source_worktree_auto_settlement_enabled
        .load(Ordering::Relaxed)
    {
        return Ok(false);
    }
    let (free_bytes, total_bytes) = settlement_disk_space(base)?;
    Ok(automatic_settlement_due(
        config
            .source_worktree_auto_settlement_enabled
            .load(Ordering::Relaxed),
        config.disk_pressure_min_free_gb.load(Ordering::Relaxed),
        free_bytes,
        total_bytes,
    ))
}

fn require_automatic_settlement_pressure(
    config: &crate::config::RuntimeConfig,
    base: &Path,
) -> Result<()> {
    let due = automatic_settlement_pressure(config, base).map_err(|error| {
        agent_settlement_refusal(
            "storage_control_automatic_settlement_not_due",
            &error.to_string(),
        )
    })?;
    if due {
        Ok(())
    } else {
        Err(agent_settlement_refusal(
            "storage_control_automatic_settlement_not_due",
            "automatic settlement is disabled or disk pressure has resolved",
        ))
    }
}

impl SessionManager {
    pub async fn list_source_worktree_cohorts(&self) -> Result<Vec<SourceWorktreeCohortSummaryV1>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock_checked()?;
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
        self.audit_source_worktree_cohort_as(repository_identity, None, None)
            .await
    }

    /// `repository_wait` is `Some` for a read that must answer within its
    /// transport deadline (the agent audit, #2622): past it the audit refuses
    /// `source_settlement_busy` rather than waiting on the repository mutex.
    /// `None` waits, as the operator audit and the automatic pass always did.
    async fn audit_source_worktree_cohort_as(
        &self,
        repository_identity: String,
        agent_caller: Option<Uuid>,
        repository_wait: Option<std::time::Duration>,
    ) -> Result<SourceWorktreeCohortAuditV1> {
        rsi_common::cohort_settlement::validate_identity(&repository_identity)
            .map_err(DaemonError::InvalidParam)?;
        let identity = repository_identity.clone();
        let store = Arc::clone(&self.store);
        let start = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock_checked()?;
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
        let completed = Arc::clone(&self.completed);
        let repository_identity_for_busy = repository_identity.clone();
        tokio::task::spawn_blocking(move || {
            let origin = PathBuf::from(&canonical_repo_dir);
            let audit_locked = || {
                let (mut inventory, latest) = {
                    let store = store.blocking_lock_checked()?;
                    let mut inventory = store.source_worktree_inventory(
                        &repository_identity,
                        SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
                    )?;
                    if let Some(caller) = agent_caller {
                        store.scope_agent_source_worktree_inventory(
                            caller,
                            &repository_identity,
                            &mut inventory,
                        )?;
                    }
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
                with_runtime_completed(&completed, |completed| {
                    exclude_runtime_completed_drift(&mut inventory, completed);
                })?;
                let mut report = build_audit_with_target_fetch_locked(
                    &repository_identity,
                    &canonical_repo_dir,
                    &sandbox_base,
                    inventory,
                    &active_ids,
                    &active_cwds,
                    true,
                )?
                .report;
                report.run_id = latest.as_ref().map(|receipt| receipt.run_id);
                // #2690: the persisted snapshot of what that run retained,
                // beside (never merged into) the fresh classification.
                report.last_run_retained_roots = latest
                    .map(|receipt| receipt.retained_roots)
                    .unwrap_or_default();
                report.validate_wire().map_err(DaemonError::Store)
            };
            let Some(wait) = repository_wait else {
                return git_worktree::with_repository_mutation(&origin, audit_locked);
            };
            match git_worktree::with_repository_mutation_within(
                &origin,
                wait,
                git_worktree::RepositoryHolder::Operation("settlement_audit"),
                audit_locked,
            )? {
                git_worktree::RepositoryWait::Ran(report) => Ok(report),
                git_worktree::RepositoryWait::Busy { holder } => Err(source_settlement_busy(
                    &store,
                    &repository_identity_for_busy,
                    holder,
                    wait,
                )),
            }
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))?
    }

    pub async fn list_source_worktree_settlement_runs(
        &self,
        limit: u32,
    ) -> Result<Vec<rsi_common::cohort_settlement::SourceWorktreeSettlementRunSummaryV1>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            store
                .blocking_lock_checked()?
                .list_source_worktree_settlement_runs(limit)
        })
        .await
        .map_err(|e| DaemonError::Process(e.to_string()))?
    }

    /// Daemon-owned loop on both hub and satellite. Serialized passes use the
    /// manual audited saga; no daemon-wide or cross-host deletion authority.
    pub fn start_source_worktree_auto_settlement(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                // #271/#273: independent of disk pressure.
                if let Err(error) = manager.retry_deferred_settlements().await {
                    tracing::warn!(%error, "Deferred settlement retry retained work");
                }
                if let Err(error) = manager.run_source_worktree_auto_settlement().await {
                    tracing::warn!(%error, "Automatic source settlement pass retained work");
                }
            }
        })
    }

    /// Recover every retained manual (agent or operator) run holding an
    /// unfinished item that a holder-inventory deadline deferred (#271) or a
    /// bounded wait surrendered (#273). Such a run has no other retry than an
    /// apply replay or a restart; its roots may already be quarantined, so this
    /// does not wait for disk pressure. An automatic run is resumed by the
    /// automatic pass, as before. A run this process is driving (a detached
    /// apply, #2866) is left to that driver. Recovery proves from scratch: a
    /// holder still present defers or fences the item again. Returns the runs
    /// it recovered.
    async fn retry_deferred_settlements(&self) -> Result<Vec<Uuid>> {
        let store = Arc::clone(&self.store);
        let due = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock_checked()?;
            retryable_deferred_settlement_runs(&store)
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))??;
        let mut recovered = Vec::with_capacity(due.len());
        for run_id in due {
            let Some(_in_flight) = try_mark_settlement_run_in_flight(run_id) else {
                continue;
            };
            let settled_before = self.settled_settlement_session_ids(run_id).await?;
            tracing::info!(run_id=%run_id, "Retrying a deferred settlement run");
            if let Err(error) = self
                .recover_source_worktree_settlement_run(run_id, RecoveryPass::DeferredRetry)
                .await
            {
                tracing::warn!(run_id=%run_id, %error, "Deferred settlement retry retained the run");
                continue;
            }
            let newly_settled = self
                .settled_settlement_session_ids(run_id)
                .await?
                .difference(&settled_before)
                .copied()
                .collect::<Vec<_>>();
            self.project_source_worktree_settlement(&newly_settled)
                .await;
            recovered.push(run_id);
        }
        Ok(recovered)
    }

    async fn settled_settlement_session_ids(&self, run_id: Uuid) -> Result<HashSet<Uuid>> {
        Ok(self
            .get_source_worktree_settlement_run(run_id)
            .await?
            .map(|run| {
                run.items
                    .iter()
                    .filter(|item| item.phase == SourceWorktreeSettlementPhaseV1::Settled)
                    .map(|item| item.session_id)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn run_source_worktree_auto_settlement(&self) -> Result<()> {
        let meter = rsid_core::pass_cost::ProcessMeter::start();
        let mut tally = AutoSettlementTally::default();
        let result = self
            .run_source_worktree_auto_settlement_tallied(&mut tally)
            .await;
        tally.log(&meter.finish(), result.is_ok());
        result
    }

    async fn run_source_worktree_auto_settlement_tallied(
        &self,
        tally: &mut AutoSettlementTally,
    ) -> Result<()> {
        if !automatic_settlement_pressure(&self.runtime_config, self.sandbox_allocator.base_dir())?
        {
            // A new pressure episode audits every cohort on its first tick.
            self.with_settlement_audit_fence(|fence| fence.clear());
            return Ok(());
        }
        // #2630: one pass of the persistence streak, however many repositories.
        self.settlement_persistence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .begin_pass();
        let cohorts = self.list_source_worktree_cohorts().await?;
        self.with_settlement_audit_fence(|fence| {
            fence.retain_listed(
                &cohorts
                    .iter()
                    .map(|cohort| cohort.repository_identity.as_str())
                    .collect(),
            )
        });
        for cohort in cohorts {
            tally.cohorts += 1;
            if !automatic_settlement_pressure(
                &self.runtime_config,
                self.sandbox_allocator.base_dir(),
            )? {
                break;
            }
            let store = Arc::clone(&self.store);
            let identity = cohort.repository_identity.clone();
            let retained = tokio::task::spawn_blocking(move || {
                store
                    .blocking_lock_checked()?
                    .latest_source_worktree_settlement_run(&identity)?
                    .map(|run| run.validate_wire().map_err(DaemonError::Store))
                    .transpose()
            })
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))??;
            let retained_run_id = retained.as_ref().map(|run| run.run_id);
            // A fence the automatic pass left (refused, recovery required) has no
            // waiting caller; deliver it to the project owner (#2452). While it
            // is delivered, the apply failures it causes are the same problem
            // and file no second notice.
            // Re-reading the retained run is not a new sighting (#2630): the
            // pass's own outcome below is what counts toward persistence.
            let mut fence_noticed = match &retained {
                Some(run) => self.automatic_settlement_run_has_owner(run).await,
                None => false,
            };
            // A recovery fence no resume will touch is a standing state, so
            // this pass sees it again (#2630); a refused root of the same run
            // is an event already counted by the pass that produced it.
            if let Some(run) = &retained
                && run.items.iter().all(|item| item.phase.is_terminal())
                && run
                    .items
                    .iter()
                    .any(|item| item.phase == SourceWorktreeSettlementPhaseV1::RecoveryRequired)
            {
                let mut fences = run.clone();
                fences
                    .items
                    .retain(|item| item.phase == SourceWorktreeSettlementPhaseV1::RecoveryRequired);
                fence_noticed |= self.notify_automatic_settlement_run(&fences).await;
            }
            if let Some(run) = retained
                && run.attribution.as_ref().is_some_and(|a|
                    a.origin == rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Automatic)
                && run.items.iter().any(|item| !item.phase.is_terminal())
            {
                // Resume only this retained automatic plan. A new random key
                // cannot advance roots fenced by its earlier intent. Manual
                // agent/operator plans remain owned by their original flow.
                let mut params = ApplySourceWorktreeCohortParams {
                    repository_identity: run.repository_identity,
                    plan_digest: run.plan_digest,
                    authorization: String::new(),
                    idempotency_key: run.idempotency_key,
                };
                params.authorization = params.expected_authorization();
                tally.resumed += 1;
                let resumed_at = std::time::Instant::now();
                let resumed = self.apply_source_worktree_cohort_as(params, None, Some(Arc::clone(&self.runtime_config)), false).await;
                tally.apply_wall += resumed_at.elapsed();
                match resumed {
                    Ok(receipt) => {
                        tally.settled += u64::from(receipt.counts.settled);
                        tally.refused += u64::from(receipt.counts.refused);
                        tracing::info!(run_id = %receipt.run_id, settled = receipt.counts.settled,
                            "Automatic settlement resumed its retained intent");
                        // A fence the resume did not clear is still present on
                        // this pass (#2630).
                        self.notify_automatic_settlement_run(&receipt).await;
                    }
                    Err(error) => {
                        tracing::warn!(run_id = %run.run_id, %error,
                            "Automatic settlement retained its original intent");
                        // Observes the run's fence for this pass; files the
                        // failure itself only when no run notice covers it.
                        self.notify_automatic_settlement_failed_attempt(
                            &cohort, "resume", &error,
                        )
                        .await;
                    }
                }
                continue;
            }
            if cohort.terminal_roots == 0 {
                self.with_settlement_audit_fence(|fence| fence.forget(&cohort.repository_identity));
                continue;
            }
            // Hub #3031: an unchanged cohort whose last audit found nothing to
            // apply is not audited again inside the backoff. Only the audit is
            // skipped: the resume above, the recovery-fence sighting above and
            // every apply keep their own proofs and streak.
            let fence_inputs = self
                .settlement_audit_inputs(&cohort.repository_identity, retained_run_id)
                .await;
            if self.settlement_audit_is_fenced(&cohort.repository_identity, fence_inputs.as_ref()) {
                tally.audit_fenced += 1;
                continue;
            }
            tally.audited += 1;
            let audit_started = std::time::Instant::now();
            let audited = self
                .audit_source_worktree_cohort(cohort.repository_identity.clone())
                .await;
            tally.audit_wall += audit_started.elapsed();
            let audit = match audited {
                Ok(audit) => audit,
                Err(error) => {
                    self.with_settlement_audit_fence(|fence| {
                        fence.forget(&cohort.repository_identity)
                    });
                    tally.audit_retained += 1;
                    tracing::warn!(repository = %cohort.repository_identity, %error, "Automatic settlement audit retained cohort");
                    if !fence_noticed {
                        self.notify_automatic_settlement_failure(&cohort, "audit", &error)
                            .await;
                    }
                    continue;
                }
            };
            let Some(authorization) = audit
                .authorization_phrase
                .clone()
                .filter(|_| audit.applyable)
            else {
                self.remember_unapplyable_settlement_audit(
                    &cohort.repository_identity,
                    fence_inputs,
                );
                continue;
            };
            self.with_settlement_audit_fence(|fence| fence.forget(&cohort.repository_identity));
            let params = ApplySourceWorktreeCohortParams {
                repository_identity: audit.repository_identity,
                plan_digest: audit.plan_digest,
                authorization,
                idempotency_key: format!("auto:{}", Uuid::new_v4()),
            };
            tally.applied += 1;
            let apply_started = std::time::Instant::now();
            let applied = self
                .apply_source_worktree_cohort_as(
                    params,
                    None,
                    Some(Arc::clone(&self.runtime_config)),
                    false,
                )
                .await;
            tally.apply_wall += apply_started.elapsed();
            match applied {
                Ok(receipt) => {
                    tally.settled += u64::from(receipt.counts.settled);
                    tally.refused += u64::from(receipt.counts.refused);
                    tracing::info!(run_id = %receipt.run_id, settled = receipt.counts.settled,
                    refused = receipt.counts.refused, "Automatic source settlement recorded");
                    self.notify_automatic_settlement_run(&receipt).await;
                }
                Err(error) => {
                    tally.apply_retained += 1;
                    tracing::warn!(repository = %cohort.repository_identity, %error,
                        "Automatic settlement retained work; committed intent remains recoverable");
                    if !fence_noticed {
                        self.notify_automatic_settlement_failed_attempt(&cohort, "apply", &error)
                            .await;
                    }
                }
            }
        }
        Ok(())
    }

    fn with_settlement_audit_fence<R>(
        &self,
        body: impl FnOnce(&mut super::settlement_audit_fence::AuditFence) -> R,
    ) -> R {
        let mut fence = self
            .settlement_audit_fence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        body(&mut fence)
    }

    fn settlement_audit_is_fenced(
        &self,
        repository_identity: &str,
        inputs: Option<&AuditInputs>,
    ) -> bool {
        let Some(inputs) = inputs else {
            return false;
        };
        self.with_settlement_audit_fence(|fence| {
            fence.decide(repository_identity, inputs, std::time::Instant::now())
                == FenceDecision::Fenced
        })
    }

    /// An audit that returned a verdict with nothing to apply: remember the
    /// inputs it started from. With no readable inputs nothing is remembered.
    fn remember_unapplyable_settlement_audit(
        &self,
        repository_identity: &str,
        inputs: Option<AuditInputs>,
    ) {
        let Some(inputs) = inputs else {
            self.with_settlement_audit_fence(|fence| fence.forget(repository_identity));
            return;
        };
        self.with_settlement_audit_fence(|fence| {
            fence.record_retained(repository_identity, inputs, std::time::Instant::now())
        });
    }

    /// The inputs an audit of `repository_identity` would start from, read the
    /// way the audit reads them but without the holder scan, git or the target
    /// fetch. `None` (no fence, audit as before) when the cohort has no live
    /// inventory or any of it cannot be read.
    async fn settlement_audit_inputs(
        &self,
        repository_identity: &str,
        latest_run: Option<Uuid>,
    ) -> Option<AuditInputs> {
        let (active_ids, paths) = {
            let active = self.active.read().await;
            let paths = collect_runtime_path_inputs(&active)
                .into_iter()
                .map(|input| (input.session_id, input.field, input.raw))
                .collect::<Vec<_>>();
            (active.keys().copied().collect::<HashSet<_>>(), paths)
        };
        let store = Arc::clone(&self.store);
        let completed = Arc::clone(&self.completed);
        let identity = repository_identity.to_string();
        let built = tokio::task::spawn_blocking(move || -> Result<Option<AuditInputs>> {
            let mut inventory = store.blocking_lock_checked()?.source_worktree_inventory(
                &identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
            )?;
            if inventory.is_empty() {
                return Ok(None);
            }
            with_runtime_completed(&completed, |completed| {
                exclude_runtime_completed_drift(&mut inventory, completed);
            })?;
            // The target tip the audit compares ancestry against, read from
            // the local ref (no fetch, no scan) under the same repository
            // lock the audit takes. Unreadable: no inputs, so the cohort is
            // audited.
            let target_tip = observed_settlement_target_tip(&identity, &inventory)?;
            Ok(Some(AuditInputs::new(
                inventory,
                latest_run,
                &target_tip,
                &active_ids,
                paths,
            )))
        })
        .await;
        match built {
            Ok(Ok(inputs)) => inputs,
            Ok(Err(error)) => {
                tracing::debug!(%error, "Automatic settlement audit fence inputs unreadable; auditing");
                None
            }
            Err(error) => {
                tracing::debug!(%error, "Automatic settlement audit fence inputs task failed; auditing");
                None
            }
        }
    }

    /// #2452/#2630: record one automatic pass's run and file (or update) the
    /// repository's project Issue once a root has been retained by the same
    /// refusal or fence for consecutive passes. Manual runs returned their
    /// receipt to a waiting caller, so only the automatic origin is delivered
    /// here. Delivery failure is logged and never changes the pass. Returns
    /// whether the run's fence is covered by an Issue or by the streak that
    /// leads to one.
    async fn notify_automatic_settlement_run(&self, run: &SourceWorktreeSettlementRunV1) -> bool {
        if run.attribution.as_ref().map(|a| a.origin)
            != Some(rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Automatic)
        {
            return false;
        }
        let store = Arc::clone(&self.store);
        let persistence = Arc::clone(&self.settlement_persistence);
        let run = run.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock_checked()?;
            let mut persistence = persistence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            store
                .upsert_source_worktree_settlement_notice_for_run(&run, &mut persistence)
                .map(|notice| (run.run_id, notice))
        })
        .await
        .map_err(|e| DaemonError::Process(e.to_string()))
        .and_then(|result| result);
        match outcome {
            Ok((run_id, notice)) => {
                tracing::info!(%run_id, action = ?notice.action, issue = ?notice.display_number,
                    "Automatic settlement owner notice");
                notice.action.is_covered()
            }
            Err(error) => {
                tracing::warn!(%error, "Automatic settlement owner notice deferred");
                false
            }
        }
    }

    /// Whether `run`'s fence has an owner (an Issue, or a project to file one
    /// to), without counting it as a sighting (#2630).
    async fn automatic_settlement_run_has_owner(
        &self,
        run: &SourceWorktreeSettlementRunV1,
    ) -> bool {
        if run.attribution.as_ref().map(|a| a.origin)
            != Some(rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Automatic)
        {
            return false;
        }
        let store = Arc::clone(&self.store);
        let run = run.clone();
        tokio::task::spawn_blocking(move || {
            store
                .blocking_lock_checked()?
                .settlement_run_has_owner(&run)
        })
        .await
        .map_err(|e| DaemonError::Process(e.to_string()))
        .and_then(|result| result)
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "Automatic settlement owner lookup failed");
            false
        })
    }

    /// #2465: an automatic apply/resume that failed may still have persisted a
    /// run with a recovery fence. That fence is the one problem, so re-read the
    /// repository's latest run, count it as this pass's sighting and let its
    /// (deduplicated) run notice cover the failure; only when no run notice
    /// covers it does the failure file its own Issue. Otherwise the next tick
    /// would file the run notice as a second Issue for the same retained roots.
    async fn notify_automatic_settlement_failed_attempt(
        &self,
        cohort: &SourceWorktreeCohortSummaryV1,
        stage: &'static str,
        error: &DaemonError,
    ) {
        let store = Arc::clone(&self.store);
        let identity = cohort.repository_identity.clone();
        let latest = tokio::task::spawn_blocking(move || {
            store
                .blocking_lock_checked()?
                .latest_source_worktree_settlement_run(&identity)?
                .map(|run| run.validate_wire().map_err(DaemonError::Store))
                .transpose()
        })
        .await
        .map_err(|e| DaemonError::Process(e.to_string()))
        .and_then(|result| result);
        match latest {
            Ok(Some(run)) => {
                if self.notify_automatic_settlement_run(&run).await {
                    return;
                }
            }
            Ok(None) => {}
            Err(read_error) => tracing::warn!(%read_error,
                "Automatic settlement could not re-read the run after a failed attempt"),
        }
        // A policy refusal (pressure resolved, policy disabled) is the pass
        // working as designed: its fence was observed above, no failure is.
        if settlement_policy_refused(error) {
            return;
        }
        self.notify_automatic_settlement_failure(cohort, stage, error)
            .await;
    }

    /// #2452: the same delivery for an automatic audit/apply/resume that failed
    /// before it produced a receipt. A policy refusal (pressure resolved, policy
    /// disabled) is the pass working as designed and is not reported.
    async fn notify_automatic_settlement_failure(
        &self,
        cohort: &SourceWorktreeCohortSummaryV1,
        stage: &'static str,
        error: &DaemonError,
    ) {
        if settlement_policy_refused(error) {
            return;
        }
        let store = Arc::clone(&self.store);
        let persistence = Arc::clone(&self.settlement_persistence);
        let identity = cohort.repository_identity.clone();
        let repo_dir = cohort.canonical_repo_dir.clone();
        let class = settlement_error_class(error);
        let text = error.to_string();
        let outcome = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock_checked()?;
            let mut persistence = persistence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            store.upsert_source_worktree_settlement_failure_notice(
                &identity,
                &repo_dir,
                stage,
                &class,
                &text,
                &mut persistence,
            )
        })
        .await
        .map_err(|e| DaemonError::Process(e.to_string()))
        .and_then(|result| result);
        match outcome {
            Ok(notice) => tracing::info!(repository = %cohort.repository_identity, stage,
                action = ?notice.action, issue = ?notice.display_number,
                "Automatic settlement failure owner notice"),
            Err(error) => tracing::warn!(repository = %cohort.repository_identity, stage, %error,
                "Automatic settlement failure owner notice deferred"),
        }
    }

    pub async fn get_source_worktree_settlement_run(
        &self,
        run_id: Uuid,
    ) -> Result<Option<SourceWorktreeSettlementRunV1>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock_checked()?;
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
        self.apply_source_worktree_cohort_as(params, None, None, false)
            .await
    }

    /// `detach` (#2789): answer with the run's receipt as soon as its durable
    /// intent exists, while the detached task keeps settling the roots. A
    /// replay of a run this process is already driving answers with the stored
    /// receipt and starts no second driver. Without `detach` the call waits for
    /// the whole run, as the operator surface and the automatic tick need.
    async fn apply_source_worktree_cohort_as(
        &self,
        params: ApplySourceWorktreeCohortParams,
        agent_caller: Option<Uuid>,
        automatic_config: Option<Arc<crate::config::RuntimeConfig>>,
        detach: bool,
    ) -> Result<SourceWorktreeSettlementRunV1> {
        params.validate().map_err(DaemonError::InvalidParam)?;
        if let Some(config) = &automatic_config {
            require_automatic_settlement_pressure(config, self.sandbox_allocator.base_dir())?;
        }
        if let Some(caller) = agent_caller {
            self.store
                .lock()
                .await
                .authorize_agent_source_worktree_cohort(caller, &params.repository_identity)?;
        }
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
                store
                    .blocking_lock_checked()?
                    .replay_source_worktree_settlement_run(&identity, &key, &fingerprint)
            })
            .await
            .map_err(|error| DaemonError::Process(error.to_string()))??
        }
        .map(|receipt| receipt.validate_wire().map_err(DaemonError::Store))
        .transpose()?;

        // A fully terminal receipt is returned as stored unless it still holds
        // a fence the store can reopen. Replay then falls through to the
        // ordinary apply path so an RPC replay revives it exactly as startup
        // recovery does (#2107).
        let terminal_replay = replay
            .as_ref()
            .is_some_and(|receipt| receipt.items.iter().all(|item| item.phase.is_terminal()));
        let revivable_terminal_replay = terminal_replay
            && replay
                .as_ref()
                .is_some_and(|receipt| receipt.items.iter().any(item_has_revivable_fence));
        if let Some(receipt) = replay.as_ref()
            && terminal_replay
            && !revivable_terminal_replay
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
        if detach {
            if let Some(receipt) = replay.as_ref()
                && settlement_run_in_flight(receipt.run_id)
            {
                return Ok(receipt.clone());
            }
            return self
                .apply_detached(
                    params,
                    agent_caller,
                    replay,
                    authorization_digest.as_str().to_string(),
                    request_fingerprint.as_str().to_string(),
                )
                .await;
        }

        let start = if let Some(receipt) = replay {
            ApplyStart::Replay(receipt)
        } else {
            let snapshot = self
                .source_worktree_audit_snapshot_as(&params.repository_identity, agent_caller, None)
                .await?;
            check_snapshot_authorizes_apply(&snapshot, &params)?;
            ApplyStart::Fresh(snapshot)
        };

        let mut session_ids = match &start {
            ApplyStart::Fresh(snapshot) => snapshot
                .eligible
                .iter()
                .filter_map(|item| item.inventory.session_id)
                .collect::<Vec<_>>(),
            ApplyStart::Inserted(receipt) | ApplyStart::Replay(receipt) => {
                effect_capable_replay_session_ids(receipt)
            }
        };
        session_ids.sort_unstable();
        session_ids.dedup();
        let mut spawn_guards = Vec::with_capacity(session_ids.len());
        for &session_id in &session_ids {
            spawn_guards.push(super::spawn_single_flight::acquire_spawn_guard(session_id).await);
        }
        let owner_became_active = self.any_owner_active(&session_ids).await;
        if owner_became_active {
            // Reviving an old fence needs an idle owner. A terminal replay
            // that cannot be revived now still answers with its stored receipt.
            if revivable_terminal_replay && let ApplyStart::Replay(receipt) = &start {
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
            return Err(DaemonError::InvalidParam(OWNER_BECAME_ACTIVE.into()));
        }
        let orphan_candidates = session_ids.clone();
        tokio::task::spawn_blocking(move || {
            super::reaper::prove_startup_settlement_orphan_scan_readable(&orphan_candidates)
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))??;

        let driver = self.settlement_driver_input(
            &params,
            agent_caller,
            automatic_config,
            authorization_digest.as_str().to_string(),
            request_fingerprint.as_str().to_string(),
        );
        // Dropping the RPC future detaches this task, not its effect/finalization
        // work. Each root owns its cwd exclusion through its last Git effect.
        tokio::spawn(driver.run(start, spawn_guards))
            .await
            .map_err(|error| DaemonError::Process(error.to_string()))?
    }

    async fn any_owner_active(&self, session_ids: &[Uuid]) -> bool {
        let active = self.active.read().await;
        session_ids
            .iter()
            .any(|session_id| active.contains_key(session_id))
    }

    fn settlement_driver_input(
        &self,
        params: &ApplySourceWorktreeCohortParams,
        agent_caller: Option<Uuid>,
        automatic_config: Option<Arc<crate::config::RuntimeConfig>>,
        authorization_digest: String,
        request_fingerprint: String,
    ) -> SettlementDriver {
        SettlementDriver {
            sandbox_base: self.sandbox_allocator.base_dir().to_path_buf(),
            store: Arc::clone(&self.store),
            active: Arc::clone(&self.active),
            completed: Arc::clone(&self.completed),
            event_bus: Arc::clone(&self.event_bus),
            identity: params.repository_identity.clone(),
            key: params.idempotency_key.clone(),
            plan_digest: params.plan_digest.as_str().to_string(),
            authorization_digest,
            request_fingerprint,
            agent_caller,
            automatic_config,
        }
    }

    /// Detached apply (#2789, #2866, #2869). Everything slow runs in one
    /// background task: the cohort audit that binds `plan_digest`, the durable
    /// intent insert, then the spawn guards, the active-owner and orphan checks,
    /// the repository mutex and every root. The caller waits at most
    /// [`AGENT_APPLY_AUDIT_BUDGET`] for the run's receipt; past it, it gets the
    /// typed `source_settlement_apply_pending` answer while the task continues,
    /// and an exact retry joins the task. A stale or refused plan ends the task
    /// before any effect and the retry receives that typed refusal; no root is
    /// touched without a fresh audit that matches the digest.
    async fn apply_detached(
        &self,
        params: ApplySourceWorktreeCohortParams,
        agent_caller: Option<Uuid>,
        replay: Option<SourceWorktreeSettlementRunV1>,
        authorization_digest: String,
        request_fingerprint: String,
    ) -> Result<SourceWorktreeSettlementRunV1> {
        let driver = self.settlement_driver_input(
            &params,
            agent_caller,
            None,
            authorization_digest,
            request_fingerprint.clone(),
        );
        if let Some(receipt) = replay {
            // A run already recorded: nothing to audit. Drive it in the
            // detached task unless this process is already doing so.
            let Some(in_flight) = try_mark_settlement_run_in_flight(receipt.run_id) else {
                return Ok(receipt);
            };
            drive_detached(driver, ApplyStart::Replay(receipt.clone()), in_flight);
            return Ok(receipt);
        }
        let key = (
            params.repository_identity.clone(),
            params.idempotency_key.clone(),
        );
        let (sender, receiver) = join_or_register_pending_apply(&key, &request_fingerprint)?;
        if let Some(sender) = sender {
            tokio::spawn(record_and_drive_detached(
                self.audit_context(),
                driver,
                params,
                agent_caller,
                key.clone(),
                sender,
            ));
        }
        await_pending_apply(&key, receiver, AGENT_APPLY_AUDIT_BUDGET).await
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
                        .blocking_lock_checked()?
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
                self.recover_source_worktree_settlement_run(run_id, RecoveryPass::Startup)
                    .await?;
            }
        }
        Ok(())
    }

    /// Recover one retained run: resume every unfinished item under the same
    /// per-root exclusion as apply. Startup runs it for every nonterminal run
    /// and first reopens the run's revivable fences (each restart's bounded
    /// reproof); `retry_deferred_settlements` runs it for a manual run holding
    /// a deferred or surrendered item (#271, #273) and resumes only unfinished
    /// items, so a 300 s pass never reopens a fence.
    async fn recover_source_worktree_settlement_run(
        &self,
        run_id: Uuid,
        pass: RecoveryPass,
    ) -> Result<()> {
        let (receipt, journal) = {
            let store = Arc::clone(&self.store);
            tokio::task::spawn_blocking(move || {
                let mut store = store.blocking_lock_checked()?;
                if pass == RecoveryPass::Startup {
                    store.reopen_source_worktree_quarantine_proof_fences(run_id)?;
                    store.reopen_source_worktree_completion_fences(run_id)?;
                }
                let receipt = store
                    .get_source_worktree_settlement_run(run_id)?
                    .ok_or_else(|| DaemonError::Store("recovery receipt disappeared".into()))?
                    .validate_wire()
                    .map_err(DaemonError::Store)?;
                let journal = store.list_source_worktree_settlement_journal_items(run_id)?;
                Ok::<_, DaemonError>((receipt, journal))
            })
            .await
            .map_err(|error| DaemonError::Process(error.to_string()))??
        };
        let mut session_ids = effect_capable_replay_session_ids(&receipt);
        session_ids.sort_unstable();
        session_ids.dedup();
        let mut spawn_guards = Vec::with_capacity(session_ids.len());
        for session_id in session_ids {
            spawn_guards.push(super::spawn_single_flight::acquire_spawn_guard(session_id).await);
        }
        let store = Arc::clone(&self.store);
        let active = Arc::clone(&self.active);
        let completed = Arc::clone(&self.completed);
        let sandbox_base = self.sandbox_allocator.base_dir().to_path_buf();
        let runtime_config = Arc::clone(&self.runtime_config);
        tokio::task::spawn_blocking(move || {
            let _spawn_guards = spawn_guards;
            let repository = PathBuf::from(&receipt.canonical_repo_dir);
            let mut entered_repository = false;
            let trees = journal.iter().filter(|item| pass.resumes(item)).filter_map(|item| {
                let original = Path::new(&item.sandbox_root);
                let quarantine = git_worktree::derive_settlement_quarantine_path(original, run_id, item.session_id).ok()?;
                let root = if quarantine.is_dir() { quarantine } else { original.to_path_buf() };
                git_worktree::prove_quarantine_tree_safe(&root).ok()
            }).collect::<Vec<_>>();
            let inventory = Arc::new(super::reaper::SharedQuarantineHolderInventory::capture(trees));
            let recovered = super::reaper::with_shared_quarantine_holder_inventory(inventory, || {
                let mut policy_stopped = false;
                for item in &journal {
                    if !pass.resumes(item) {
                        continue;
                    }
                    // Recovery now overlaps live RPCs. Use the same
                    // CWD-first per-root scheduler as ordinary apply.
                    settle_root_exclusion_first(&repository, &store, run_id, item, |cwd_exclusion| {
                    entered_repository = true;
                    if policy_stopped {
                        return close_item_after_policy_stop_under_exclusion(
                            cwd_exclusion, &store, &active, &completed,
                            run_id, &sandbox_base, item, true,
                        );
                    }
                    // IntentCommitted can also describe a crash after
                    // a quarantine move. Only proven untouched items
                    // need fresh initiating authority; effect-started
                    // items retain the exact recovery obligation.
                    let no_effect = match if retained_intent_can_be_reproved(item) {
                        // No effect is admitted until the guarded reproof
                        // below succeeds; it needs fresh initiating policy.
                        Ok(true)
                    } else {
                        settlement_item_has_no_effect(run_id, item)
                    } {
                        Ok(no_effect) => no_effect,
                        Err(error) => {
                            tracing::warn!(run_id=%run_id, session_id=%item.session_id, error=%error,
                                "Settlement startup probe failed; retaining this item and continuing recovery");
                            close_failed_settlement_probe(&store, run_id, item)?;
                            return Ok(false);
                        }
                    };
                    let attribution = receipt.attribution.as_ref().filter(|_| no_effect);
                    let agent_caller = attribution.and_then(|a| a.agent_session_id);
                    let automatic_config = attribution
                        .filter(|a| a.origin == rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Automatic)
                        .map(|_| runtime_config.as_ref());
                    match settle_one_item_under_exclusion(
                        cwd_exclusion,
                        &store,
                        &active,
                        &completed,
                        run_id,
                        &receipt.target_ref,
                        &repository,
                        &sandbox_base,
                        item,
                        agent_caller,
                        automatic_config,
                    ) {
                        Ok(settled) => Ok(settled),
                        Err(error) if settlement_policy_refused(&error) => {
                            mark_unattempted_under_root(&store, run_id, item, true, true)?;
                            policy_stopped = true;
                            Ok(false)
                        }
                        Err(error) => {
                            tracing::warn!(run_id=%run_id, session_id=%item.session_id, error=%error,
                                "Settlement recovery retained one failed item and continued");
                            retain_failed_settlement_item(&store, run_id, item.session_id, &error)?;
                            Ok(false)
                        }
                    }
                    })?;
                }
                git_worktree::with_repository_mutation(&repository, || {
                    entered_repository = true;
                retry_settlement_database_items(
                    &store, &active, &completed, &receipt, &sandbox_base,
                )?;
                Ok(())
                })
            });
            if let Err(error) = &recovered {
                if !entered_repository {
                    tracing::warn!(run_id=%run_id, error=%error, "Settlement repository unavailable during startup recovery");
                    return Ok::<_, DaemonError>(());
                }
                tracing::warn!(run_id=%run_id, error=%error,
                    "Settlement recovery retained unfinished items after a failed attempt");
            }
            let store = store.blocking_lock_checked()?;
            if let Err(error) = &recovered {
                retain_failed_settlement_items(&store, run_id, None, error)?;
            }
            flush_settlement_surrenders(&store, run_id);
            // Contended items remain replayable on the next pass. A
            // transient wait is not a failed recovery proof.
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
        Ok(())
    }

    /// `repository_wait` bounds the wait for the repository mutex (a detached
    /// apply, #2866): past it the audit answers `source_settlement_busy`
    /// instead of holding the caller behind another run's saga.
    async fn source_worktree_audit_snapshot_as(
        &self,
        repository_identity: &str,
        agent_caller: Option<Uuid>,
        repository_wait: Option<std::time::Duration>,
    ) -> Result<AuditSnapshot> {
        self.audit_context()
            .snapshot(repository_identity, agent_caller, repository_wait)
            .await
    }

    fn audit_context(&self) -> AuditContext {
        AuditContext {
            store: Arc::clone(&self.store),
            active: Arc::clone(&self.active),
            completed: Arc::clone(&self.completed),
            sandbox_base: self.sandbox_allocator.base_dir().to_path_buf(),
        }
    }

    async fn project_source_worktree_settlement(&self, settled_ids: &[Uuid]) {
        Self::project_settlement(&self.active, &self.completed, &self.event_bus, settled_ids).await;
    }

    async fn project_settlement(
        active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
        completed: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
        event_bus: &crate::bus::EventBus,
        settled_ids: &[Uuid],
    ) {
        if settled_ids.is_empty() {
            return;
        }
        let settled = settled_ids.iter().copied().collect::<HashSet<_>>();
        let mut projected_sessions = Vec::new();
        {
            let mut completed = completed.write().await;
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
            let mut active = active.write().await;
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
            let mut completed = completed.write().await;
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
            event_bus.publish(DaemonEvent::SessionMetadataChanged {
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
            event_bus.publish(DaemonEvent::SessionArchived {
                session_id,
                projection_id: None,
            });
        }
    }
}

/// Run a recorded settlement in a detached task that owns its in-flight mark.
fn drive_detached(driver: SettlementDriver, start: ApplyStart, in_flight: SettlementRunInFlight) {
    let run_id = match &start {
        ApplyStart::Inserted(receipt) | ApplyStart::Replay(receipt) => receipt.run_id,
        ApplyStart::Fresh(_) => return,
    };
    tokio::spawn(async move {
        let _in_flight = in_flight;
        if let Err(error) = driver.run_detached(start).await {
            tracing::warn!(run_id = %run_id, %error,
                "Detached settlement apply ended with an error");
        }
    });
}

/// A replayed receipt needs a driver unless it is fully terminal with no fence
/// the store could reopen.
fn replay_needs_driver(receipt: &SourceWorktreeSettlementRunV1) -> bool {
    !receipt.items.iter().all(|item| item.phase.is_terminal())
        || receipt.items.iter().any(item_has_revivable_fence)
}

/// Audit the cohort and record the durable intent for a detached apply (#2869),
/// or find the run another request recorded for this idempotency key since the
/// caller's replay lookup.
async fn record_detached_run(
    audit: &AuditContext,
    driver: &SettlementDriver,
    params: &ApplySourceWorktreeCohortParams,
    agent_caller: Option<Uuid>,
) -> Result<ApplyStart> {
    let replay = {
        let store = Arc::clone(&audit.store);
        let identity = driver.identity.clone();
        let key = driver.key.clone();
        let fingerprint = driver.request_fingerprint.clone();
        tokio::task::spawn_blocking(move || {
            store
                .blocking_lock_checked()?
                .replay_source_worktree_settlement_run(&identity, &key, &fingerprint)
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))??
    };
    if let Some(receipt) = replay {
        return Ok(ApplyStart::Replay(
            receipt.validate_wire().map_err(DaemonError::Store)?,
        ));
    }
    let snapshot = audit
        .snapshot(
            &params.repository_identity,
            agent_caller,
            Some(AGENT_AUDIT_REPOSITORY_WAIT),
        )
        .await?;
    check_snapshot_authorizes_apply(&snapshot, params)?;
    let store = Arc::clone(&audit.store);
    let sandbox_base = audit.sandbox_base.clone();
    let identity = driver.identity.clone();
    let key = driver.key.clone();
    let plan_digest = driver.plan_digest.clone();
    let authorization = driver.authorization_digest.clone();
    let fingerprint = driver.request_fingerprint.clone();
    tokio::task::spawn_blocking(move || {
        let target_ref = snapshot
            .report
            .target_ref
            .clone()
            .ok_or_else(|| DaemonError::Store("applyable audit lacks target ref".into()))?;
        let target_oid = snapshot
            .report
            .target_oid
            .as_ref()
            .ok_or_else(|| DaemonError::Store("applyable audit lacks target oid".into()))?
            .as_str()
            .to_string();
        let (run_id, outcome) = insert_audited_settlement_run(
            &store,
            &snapshot,
            &AuditedRunRequest {
                repository_identity: &identity,
                canonical_repo_dir: &snapshot.report.canonical_repo_dir,
                target_ref: &target_ref,
                target_oid: &target_oid,
                plan_digest: &plan_digest,
                idempotency_key: &key,
                authorization_digest: &authorization,
                request_fingerprint: &fingerprint,
            },
            &sandbox_base,
            agent_caller,
            None,
        )?;
        Ok::<_, DaemonError>(match outcome {
            InsertSettlementRunOutcome::Inserted => ApplyStart::Inserted(
                store
                    .blocking_lock_checked()?
                    .get_source_worktree_settlement_run(run_id)?
                    .ok_or_else(|| DaemonError::Store("settlement receipt disappeared".into()))?
                    .validate_wire()
                    .map_err(DaemonError::Store)?,
            ),
            InsertSettlementRunOutcome::Replay(receipt) => {
                ApplyStart::Replay(receipt.validate_wire().map_err(DaemonError::Store)?)
            }
        })
    })
    .await
    .map_err(|error| DaemonError::Process(error.to_string()))?
}

/// The detached apply's background task (#2869): audit, record, publish the
/// receipt to whoever waits, then settle the roots. A failure before the
/// intent exists is published to the waiters and kept for the next exact retry.
async fn record_and_drive_detached(
    audit: AuditContext,
    driver: SettlementDriver,
    params: ApplySourceWorktreeCohortParams,
    agent_caller: Option<Uuid>,
    key: PendingApplyKey,
    sender: tokio::sync::watch::Sender<PendingApplyOutcome>,
) {
    let start = match record_detached_run(&audit, &driver, &params, agent_caller).await {
        Ok(start) => start,
        Err(error) => {
            tracing::info!(%error, "Detached settlement apply ended before recording a run");
            let _ = sender.send(Some(Err(PendingApplyFailure::capture(&error))));
            return;
        }
    };
    let receipt = match &start {
        ApplyStart::Inserted(receipt) | ApplyStart::Replay(receipt) => receipt.clone(),
        ApplyStart::Fresh(_) => {
            let _ = sender.send(Some(Err(PendingApplyFailure::Other(
                "detached apply reached an unrecorded start".into(),
            ))));
            return;
        }
    };
    // The store replays a recorded run by key, so the registry entry is done.
    let in_flight = if matches!(start, ApplyStart::Replay(_)) && !replay_needs_driver(&receipt) {
        None
    } else {
        try_mark_settlement_run_in_flight(receipt.run_id)
    };
    remove_pending_apply(&key);
    let _ = sender.send(Some(Ok(receipt)));
    if let Some(in_flight) = in_flight {
        drive_detached(driver, start, in_flight);
    }
}

/// The owned handles one cohort audit reads, so a detached apply (#2869) can run
/// its audit in a task that outlives the RPC future.
struct AuditContext {
    store: Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    sandbox_base: PathBuf,
}

impl AuditContext {
    async fn snapshot(
        &self,
        repository_identity: &str,
        agent_caller: Option<Uuid>,
        repository_wait: Option<std::time::Duration>,
    ) -> Result<AuditSnapshot> {
        let mut inventory = self
            .load_inventory(repository_identity, agent_caller)
            .await?;
        exclude_runtime_completed_drift(&mut inventory, &*self.completed.read().await);
        let active = self.active.read().await;
        let active_ids = active.keys().copied().collect::<HashSet<_>>();
        let active_cwds = collect_runtime_path_inputs(&active);
        drop(active);
        let sandbox_base = self.sandbox_base.clone();
        let identity = repository_identity.to_string();
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let Some(wait) = repository_wait else {
                return build_audit(
                    &identity,
                    &sandbox_base,
                    inventory,
                    &active_ids,
                    &active_cwds,
                );
            };
            let canonical_repo_dir = unique_repository_dir(&identity, &inventory)?;
            let origin = PathBuf::from(&canonical_repo_dir);
            match git_worktree::with_repository_mutation_within(
                &origin,
                wait,
                git_worktree::RepositoryHolder::Operation("settlement_apply"),
                || {
                    build_audit_locked(
                        &identity,
                        &canonical_repo_dir,
                        &sandbox_base,
                        inventory,
                        &active_ids,
                        &active_cwds,
                    )
                },
            )? {
                git_worktree::RepositoryWait::Ran(snapshot) => Ok(snapshot),
                git_worktree::RepositoryWait::Busy { holder } => {
                    Err(source_settlement_busy(&store, &identity, holder, wait))
                }
            }
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))?
    }

    async fn load_inventory(
        &self,
        repository_identity: &str,
        agent_caller: Option<Uuid>,
    ) -> Result<Vec<SourceWorktreeInventoryRow>> {
        let identity = repository_identity.to_string();
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock_checked()?;
            let mut inventory = store.source_worktree_inventory(
                &identity,
                SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
            )?;
            if let Some(caller) = agent_caller {
                store.scope_agent_source_worktree_inventory(caller, &identity, &mut inventory)?;
            }
            Ok(inventory)
        })
        .await
        .map_err(|error| DaemonError::Process(error.to_string()))?
    }
}

#[cfg(test)]
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

#[cfg(test)]
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
    apply_locked_with_agent_authority(
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
        reap_orphans_after_intent,
        None,
        None,
    )
}

/// Everything an apply decides before its first root: the durable run, its
/// journal and whether the initiating authority is already gone.
struct PreparedApply {
    run_id: Uuid,
    canonical_repo_dir: String,
    target_ref: String,
    replaying: bool,
    journal: Vec<SourceWorktreeSettlementJournalItem>,
    authority_lost: bool,
}

/// Run a whole apply with the caller already holding the repository mutex.
/// Each root takes the provider-cwd exclusion here, *inside* that mutex, so this
/// shape is only for callers that cannot overlap a provider launch or archive
/// cleanup (startup recovery, tests). The production RPC apply uses
/// [`apply_with_root_scheduling`], which follows the lock order documented on
/// `acquire_settlement_cwd_exclusion`.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn apply_locked_with_agent_authority(
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
    agent_caller: Option<Uuid>,
    automatic_config: Option<&crate::config::RuntimeConfig>,
) -> Result<ApplyOutcome> {
    let prepared = prepare_apply(
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
        reap_orphans_after_intent,
        agent_caller,
        automatic_config,
    )?;
    let mut settled_ids = initial_settled_ids(&prepared.journal);
    let mut policy_stopped = prepared.authority_lost;
    for item in &prepared.journal {
        apply_journal_item(
            store,
            prepared.run_id,
            item,
            prepared.replaying,
            &mut policy_stopped,
            &mut settled_ids,
            || {
                settle_one_item_with_agent_authority(
                    store,
                    active,
                    completed,
                    prepared.run_id,
                    &prepared.target_ref,
                    Path::new(&prepared.canonical_repo_dir),
                    sandbox_base,
                    item,
                    prepared.replaying,
                    agent_caller,
                    automatic_config,
                )
            },
            || {
                let Some(exclusion) = wait_for_settlement_lock(acquire_settlement_cwd_exclusion())
                else {
                    return Ok(());
                };
                close_item_after_policy_stop_under_exclusion(
                    &exclusion,
                    store,
                    active,
                    completed,
                    prepared.run_id,
                    sandbox_base,
                    item,
                    prepared.replaying,
                )
                .map(|_| ())
            },
        )?;
    }
    finish_apply(store, prepared.run_id, settled_ids)
}

/// Production apply. Lock order (outermost first): provider-cwd exclusion ->
/// repository mutex -> (bounded) Store. The setup holds the mutex, then every
/// root takes the cwd exclusion *before* it re-enters the mutex for that one
/// root, so a provider launch or archive cleanup that holds the cwd lock and
/// waits for the mutex is admitted between roots and never starves the rest of
/// the cohort (#2081).
#[allow(clippy::too_many_arguments)]
fn apply_with_root_scheduling(
    origin: &Path,
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
    agent_caller: Option<Uuid>,
    automatic_config: Option<&crate::config::RuntimeConfig>,
    on_prepared: Option<&mut dyn FnMut(Uuid)>,
) -> Result<ApplyOutcome> {
    let prepared = git_worktree::with_repository_mutation(origin, || {
        prepare_apply(
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
            reap_orphans_after_intent,
            agent_caller,
            automatic_config,
        )
    })?;
    if let Some(on_prepared) = on_prepared {
        // The durable intent exists and its driver is now registered.
        on_prepared(prepared.run_id);
    }
    let mut settled_ids = initial_settled_ids(&prepared.journal);
    let mut policy_stopped = prepared.authority_lost;
    for item in &prepared.journal {
        apply_journal_item(
            store,
            prepared.run_id,
            item,
            prepared.replaying,
            &mut policy_stopped,
            &mut settled_ids,
            || {
                settle_root_exclusion_first(origin, store, prepared.run_id, item, |exclusion| {
                    settle_one_item_under_exclusion(
                        exclusion,
                        store,
                        active,
                        completed,
                        prepared.run_id,
                        &prepared.target_ref,
                        Path::new(&prepared.canonical_repo_dir),
                        sandbox_base,
                        item,
                        agent_caller,
                        automatic_config,
                    )
                })
            },
            || {
                settle_root_exclusion_first(origin, store, prepared.run_id, item, |exclusion| {
                    close_item_after_policy_stop_under_exclusion(
                        exclusion,
                        store,
                        active,
                        completed,
                        prepared.run_id,
                        sandbox_base,
                        item,
                        prepared.replaying,
                    )
                })
                .map(|_| ())
            },
        )?;
    }
    // Effects are complete; the database tail retries root by root too.
    let receipt = store
        .blocking_lock_checked()?
        .get_source_worktree_settlement_run(prepared.run_id)?
        .ok_or_else(|| DaemonError::Store("settlement receipt disappeared".into()))?;
    retry_settlement_database_items_with(store, &receipt, |item| {
        settle_root_exclusion_first(origin, store, receipt.run_id, item, |exclusion| {
            settle_one_item_under_exclusion(
                exclusion,
                store,
                active,
                completed,
                receipt.run_id,
                &receipt.target_ref,
                Path::new(&receipt.canonical_repo_dir),
                sandbox_base,
                item,
                None,
                None,
            )
        })
    })?;
    finish_apply(store, prepared.run_id, settled_ids)
}

fn initial_settled_ids(journal: &[SourceWorktreeSettlementJournalItem]) -> Vec<Uuid> {
    journal
        .iter()
        .filter(|item| item.phase == SourceWorktreeSettlementPhaseV1::Settled)
        .map(|item| item.session_id)
        .collect()
}

/// One iteration of the per-root loop, shared by both scheduling shapes.
#[allow(clippy::too_many_arguments)]
fn apply_journal_item(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
    replaying: bool,
    policy_stopped: &mut bool,
    settled_ids: &mut Vec<Uuid>,
    settle: impl FnOnce() -> Result<bool>,
    close_after_policy_stop: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if item.phase.is_terminal() && !retained_intent_can_be_reproved(item) {
        return Ok(());
    }
    if *policy_stopped {
        return close_after_policy_stop();
    }
    match settle() {
        Ok(true) => settled_ids.push(item.session_id),
        Ok(false) => {}
        Err(error) => {
            // Only loss of initiating policy stops the remaining plan.
            // Other failures belong to this root, never its neighbors.
            if settlement_policy_refused(&error) {
                // Authority loss or pressure resolution is a terminal stop.
                // Close this untouched root and the rest of the plan now
                // so a manual audit is never fenced until another tick.
                mark_unattempted_under_root(store, run_id, item, replaying, true)?;
                *policy_stopped = true;
                return Ok(());
            }
            tracing::warn!(
                run_id = %run_id,
                session_id = %item.session_id,
                error = %error,
                "Settlement item retained after an unexpected saga failure"
            );
            retain_failed_settlement_item(store, run_id, item.session_id, &error)?;
        }
    }
    Ok(())
}

fn finish_apply(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run_id: Uuid,
    mut settled_ids: Vec<Uuid>,
) -> Result<ApplyOutcome> {
    settled_ids.sort_unstable();
    settled_ids.dedup();
    let store = store.blocking_lock_checked()?;
    flush_settlement_surrenders(&store, run_id);
    let receipt = store
        .get_source_worktree_settlement_run(run_id)?
        .ok_or_else(|| DaemonError::Store("settlement receipt disappeared".into()))?;
    Ok(ApplyOutcome {
        receipt,
        settled_ids,
    })
}

#[allow(clippy::too_many_arguments)]
fn prepare_apply(
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
    agent_caller: Option<Uuid>,
    automatic_config: Option<&crate::config::RuntimeConfig>,
) -> Result<PreparedApply> {
    if let Some(config) = automatic_config {
        require_automatic_settlement_pressure(config, sandbox_base)?;
    }

    if let Some(caller) = agent_caller {
        store
            .blocking_lock_checked()?
            .authorize_agent_source_worktree_cohort(caller, repository_identity)?;
    }
    let (run_id, canonical_repo_dir, target_ref, replaying) = match start {
        ApplyStart::Fresh(snapshot) => {
            let active_guard = wait_for_settlement_lock(active.read()).ok_or_else(|| {
                DaemonError::InvalidParam(
                    "runtime state is busy; audit was not applied and may be retried".into(),
                )
            })?;
            let active_ids = active_guard.keys().copied().collect::<HashSet<_>>();
            let active_cwds = collect_runtime_path_inputs(&active_guard);
            drop(active_guard);
            let mut inventory = {
                let store = store.blocking_lock_checked()?;
                let mut inventory = store.source_worktree_inventory(
                    repository_identity,
                    SOURCE_WORKTREE_SETTLEMENT_MAX_ROOTS.saturating_add(1),
                )?;
                if let Some(caller) = agent_caller {
                    store.scope_agent_source_worktree_inventory(
                        caller,
                        repository_identity,
                        &mut inventory,
                    )?;
                }
                inventory
            };
            with_runtime_completed(completed, |completed| {
                exclude_runtime_completed_drift(&mut inventory, completed);
            })?;
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
            let (run_id, outcome) = insert_audited_settlement_run(
                store,
                &recomputed,
                &AuditedRunRequest {
                    repository_identity,
                    canonical_repo_dir: &canonical_repo_dir,
                    target_ref: &target_ref,
                    target_oid: &target_oid,
                    plan_digest,
                    idempotency_key,
                    authorization_digest,
                    request_fingerprint,
                },
                sandbox_base,
                agent_caller,
                automatic_config,
            )?;
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
        ApplyStart::Inserted(receipt) => {
            let receipt = receipt.validate_wire().map_err(DaemonError::Store)?;
            (
                receipt.run_id,
                receipt.canonical_repo_dir,
                receipt.target_ref,
                false,
            )
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

    if replaying {
        store
            .blocking_lock_checked()?
            .reopen_source_worktree_quarantine_proof_fences(run_id)?;
        store
            .blocking_lock_checked()?
            .reopen_source_worktree_completion_fences(run_id)?;
    }
    let journal = store
        .blocking_lock_checked()?
        .list_source_worktree_settlement_journal_items(run_id)?;
    let authority_lost = if let Some(caller) = agent_caller {
        match store
            .blocking_lock_checked()?
            .authorize_agent_source_worktree_cohort(caller, repository_identity)
        {
            Ok(()) => false,
            Err(error) if settlement_policy_refused(&error) => true,
            Err(error) => return Err(error),
        }
    } else {
        false
    };
    if reap_orphans_after_intent && !authority_lost {
        let candidate_ids = {
            let store = store.blocking_lock_checked()?;
            let mut ids = Vec::new();
            for item in journal
                .iter()
                .filter(|item| phase_can_still_apply_git_effect(&item.phase))
            {
                if let Some(caller) = agent_caller {
                    match store.authorize_agent_source_worktree_root(
                        caller,
                        repository_identity,
                        item.custody_id,
                    ) {
                        Ok(()) => {}
                        Err(error) if settlement_policy_refused(&error) => continue,
                        Err(error) => return Err(error),
                    }
                }
                ids.push(item.session_id);
            }
            ids
        };
        let reaped = super::reaper::reap_startup_settlement_orphans_checked(&candidate_ids)?;
        if reaped > 0 {
            tracing::warn!(
                reaped,
                run_id = %run_id,
                "Reaped exact-stamped provider orphan(s) after durable settlement intent"
            );
        }
    }
    Ok(PreparedApply {
        run_id,
        canonical_repo_dir,
        target_ref,
        replaying,
        journal,
        authority_lost,
    })
}

/// The request-bound fields of a freshly recorded settlement intent.
struct AuditedRunRequest<'a> {
    repository_identity: &'a str,
    canonical_repo_dir: &'a str,
    target_ref: &'a str,
    target_oid: &'a str,
    plan_digest: &'a str,
    idempotency_key: &'a str,
    authorization_digest: &'a str,
    request_fingerprint: &'a str,
}

/// Record the durable intent for an audited cohort: one run row plus one
/// journal item per eligible root. Needs only the Store, never the repository
/// mutex, so a detached apply (#2866) records it while another run still holds
/// the repository. Returns the run id this call chose with the store's outcome
/// (a concurrent insert of the same idempotency key answers `Replay`).
fn insert_audited_settlement_run(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    audit: &AuditSnapshot,
    request: &AuditedRunRequest<'_>,
    sandbox_base: &Path,
    agent_caller: Option<Uuid>,
    automatic_config: Option<&crate::config::RuntimeConfig>,
) -> Result<(Uuid, InsertSettlementRunOutcome)> {
    let run_id = Uuid::new_v4();
    let new_run = NewSettlementRun {
        run_id,
        repository_identity: request.repository_identity.to_string(),
        canonical_repo_dir: request.canonical_repo_dir.to_string(),
        target_ref: request.target_ref.to_string(),
        target_oid: request.target_oid.to_string(),
        plan_digest: request.plan_digest.to_string(),
        idempotency_key: request.idempotency_key.to_string(),
        authorization_digest: request.authorization_digest.to_string(),
        request_fingerprint: request.request_fingerprint.to_string(),
        observed_count: audit.report.counts.observed,
        retained_count: audit.report.counts.retained,
        retained: retained_roots_of(&audit.report),
        items: audit
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
                target_oid: request.target_oid.to_string(),
                evidence_digest: item.evidence_digest.clone(),
                clean_state_digest: item.clean_state_digest.clone(),
                reserved_effects: item.inventory.reserved_effects,
                active_effects: item.inventory.active_effects,
                participant_count: item.inventory.participant_count,
            })
            .collect(),
    };
    let mut guard = store.blocking_lock_checked()?;
    if let Some(caller) = agent_caller {
        for item in &new_run.items {
            guard.authorize_agent_source_worktree_root(
                caller,
                request.repository_identity,
                item.custody_id,
            )?;
        }
    }
    if let Some(config) = automatic_config {
        require_automatic_settlement_pressure(config, sandbox_base)?;
    }
    let attribution = rsi_common::cohort_settlement::SourceWorktreeSettlementAttributionV1 {
        origin: if automatic_config.is_some() {
            rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Automatic
        } else if agent_caller.is_some() {
            rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Agent
        } else {
            rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Operator
        },
        agent_session_id: agent_caller,
    };
    let outcome = guard.insert_source_worktree_settlement_run_as(&new_run, &attribution)?;
    Ok((run_id, outcome))
}

#[cfg(test)]
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
    settle_one_item_with_agent_authority(
        store,
        active,
        completed,
        run_id,
        target_ref,
        canonical_repo_dir,
        sandbox_base,
        item,
        replaying,
        None,
        None,
    )
}

/// Settle one root for a caller that already holds the repository mutex. The
/// cwd exclusion is taken here, inside that mutex, with a short bound: only
/// the database-only retry tail and tests use this shape. Production apply
/// and recovery Git effects use [`settle_root_exclusion_first`]
/// (lock order: cwd exclusion, then mutex).
#[allow(clippy::too_many_arguments)]
fn settle_one_item_with_agent_authority(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    run_id: Uuid,
    target_ref: &str,
    canonical_repo_dir: &Path,
    sandbox_base: &Path,
    item: &SourceWorktreeSettlementJournalItem,
    _replaying: bool,
    agent_caller: Option<Uuid>,
    automatic_config: Option<&crate::config::RuntimeConfig>,
) -> Result<bool> {
    let Some(cwd_exclusion) = wait_for_settlement_lock(acquire_settlement_cwd_exclusion()) else {
        mark_item_stopped_blocking(
            store,
            run_id,
            item,
            SourceWorktreeSettlementRefusalV1::RuntimeStateContended,
            "provider cwd admission remained contended before effect",
        )?;
        return Ok(false);
    };
    settle_one_item_under_exclusion(
        &cwd_exclusion,
        store,
        active,
        completed,
        run_id,
        target_ref,
        canonical_repo_dir,
        sandbox_base,
        item,
        agent_caller,
        automatic_config,
    )
}

/// Upper bound a root waits for the provider-cwd exclusion when it holds no
/// other lock. A launch holds admission through `git fetch`, worktree
/// allocation and provider spawn, so this is far above the 1 s inverse-wait
/// bound; expiry leaves a proven-untouched root retryable, never a hang.
const SETTLEMENT_CWD_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Lock order for one root: provider-cwd exclusion first, with no other lock
/// held, then the repository mutex for that root alone. Provider launch and
/// archive cleanup hold the cwd lock and then wait for the mutex, so the
/// mutex is never held across a wait for the cwd lock (#2081).
fn settle_root_exclusion_first(
    origin: &Path,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
    settle: impl FnOnce(&CwdExclusion) -> Result<bool>,
) -> Result<bool> {
    let Some(cwd_exclusion) =
        wait_for_settlement_lock_for(acquire_settlement_cwd_exclusion(), SETTLEMENT_CWD_WAIT)
    else {
        mark_item_stopped_blocking(
            store,
            run_id,
            item,
            SourceWorktreeSettlementRefusalV1::RuntimeStateContended,
            "provider cwd admission remained contended before effect",
        )?;
        return Ok(false);
    };
    // Record the run as the repository's holder (#2663): a bounded waiter names
    // this run, not whichever run is latest in the journal.
    git_worktree::with_repository_mutation_as(
        origin,
        git_worktree::RepositoryHolder::SettlementRun(run_id),
        || settle(&cwd_exclusion),
    )
}

/// Upper bound on retaking the runtime and Store fences after a long Git step
/// ran without them (#2790). Expiry leaves the item in its durable phase with
/// its marker, which the next apply or startup recovery replays.
const SETTLEMENT_FENCE_REACQUIRE: std::time::Duration = std::time::Duration::from_secs(30);

type SettlementFences<'a> = (
    tokio::sync::RwLockReadGuard<'a, HashMap<Uuid, super::types::TrackedSession>>,
    tokio::sync::RwLockReadGuard<'a, HashMap<Uuid, super::types::CompletedSession>>,
    tokio::sync::MutexGuard<'a, crate::store::Store>,
);

/// Retake the fences in the order `settle_one_item_under_exclusion` first
/// took them (active, completed, Store), each under the same wait bound.
fn reacquire_settlement_fences<'a>(
    active: &'a Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: &'a Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    store: &'a Arc<tokio::sync::Mutex<crate::store::Store>>,
) -> Result<SettlementFences<'a>> {
    let busy = || {
        tracing::warn!(
            "Settlement fences stayed contended after a long Git step; the item stays replayable"
        );
        crate::store::custody_lock_order::lock_order_busy_error()
    };
    let active =
        wait_for_settlement_lock_for(active.read(), SETTLEMENT_FENCE_REACQUIRE).ok_or_else(busy)?;
    let completed = wait_for_settlement_lock_for(completed.read(), SETTLEMENT_FENCE_REACQUIRE)
        .ok_or_else(busy)?;
    let store =
        wait_for_settlement_lock_for(store.lock(), SETTLEMENT_FENCE_REACQUIRE).ok_or_else(busy)?;
    Ok((active, completed, store))
}

type CwdExclusion = tokio::sync::RwLockWriteGuard<'static, ()>;

/// The provider-cwd exclusion one settlement root takes: the daemon-wide
/// `spawn_single_flight` barrier that provider launch and archive cleanup
/// share. A unit test takes a lock of its own instead ([`test_cwd_admission`]).
#[cfg(not(test))]
async fn acquire_settlement_cwd_exclusion() -> CwdExclusion {
    super::spawn_single_flight::acquire_settlement_cwd_exclusion().await
}

#[cfg(test)]
async fn acquire_settlement_cwd_exclusion() -> CwdExclusion {
    if REAL_CWD_BARRIER.with(std::cell::Cell::get) {
        return super::spawn_single_flight::acquire_settlement_cwd_exclusion().await;
    }
    test_cwd_admission().write().await
}

#[cfg(test)]
thread_local! {
    /// Set while this thread runs inside [`with_real_cwd_barrier`].
    static REAL_CWD_BARRIER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// #283: run `body` with this thread's settlement taking the real daemon-wide
/// barrier, the lock a provider launch shares in production. The only test
/// that does so, and the serial guard keeps a second one out: every other
/// unit test takes a lock of its own ([`test_cwd_admission`], #274), so no
/// other test's settlement waits on, or holds, the real barrier.
#[cfg(target_os = "linux")]
#[cfg(test)]
fn with_real_cwd_barrier<T>(body: impl FnOnce() -> T) -> T {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            REAL_CWD_BARRIER.with(|flag| flag.set(false));
        }
    }
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    REAL_CWD_BARRIER.with(|flag| flag.set(true));
    let _restore = Restore;
    body()
}

#[cfg(test)]
type TestCwdAdmission = &'static tokio::sync::RwLock<()>;

#[cfg(test)]
thread_local! {
    /// The admission a test scope selected on this thread ([`with_cwd_admission`]).
    static FIXTURE_CWD_ADMISSION: std::cell::Cell<Option<TestCwdAdmission>> =
        const { std::cell::Cell::new(None) };
    /// The admission this thread falls back to when neither a scope nor a Tokio
    /// runtime selects one.
    static THREAD_CWD_ADMISSION: std::cell::Cell<Option<TestCwdAdmission>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn new_test_cwd_admission() -> TestCwdAdmission {
    Box::leak(Box::new(tokio::sync::RwLock::const_new(())))
}

/// #274 (after #270): where a unit test's settlement takes its provider-cwd
/// exclusion. The daemon-wide barrier is one lock per process, so a test that
/// held it (a paused proof, a held Store, a recovery or ticker run through a
/// `SessionManager` that holds it through a whole holder proof) made every
/// other test's one-second in-mutex wait surrender, which leaves a quarantined
/// item untouched: a test's result depended on which tests shared its process.
/// So no unit test takes the daemon-wide barrier. In order:
/// 1. the lock a test scope selected on this thread ([`with_cwd_admission`],
///    [`with_fixture_cwd_admission`]), for a fixture that settles synchronously
///    or contends on purpose;
/// 2. the lock of the Tokio runtime this thread belongs to, so a manager's
///    ticker, recovery and apply (which run on that runtime's blocking threads)
///    and the test that drives it share one lock, as they share the barrier in
///    production;
/// 3. a lock of this thread's own, for a test that calls the saga directly.
///
/// A test that checks settlement against a provider launch takes the launch
/// side of the same lock (`test_launch_cwd_admission`). Production always
/// takes the one daemon-wide barrier.
#[cfg(test)]
fn test_cwd_admission() -> TestCwdAdmission {
    if let Some(scoped) = FIXTURE_CWD_ADMISSION.with(std::cell::Cell::get) {
        return scoped;
    }
    if let Some(runtime) = runtime_cwd_admission() {
        return runtime;
    }
    THREAD_CWD_ADMISSION.with(|slot| {
        slot.get().unwrap_or_else(|| {
            let own = new_test_cwd_admission();
            slot.set(Some(own));
            own
        })
    })
}

/// The lock of the Tokio runtime the caller runs on, `None` outside a runtime.
/// A runtime's blocking threads count as its own.
#[cfg(test)]
fn runtime_cwd_admission() -> Option<TestCwdAdmission> {
    static BY_RUNTIME: std::sync::LazyLock<
        std::sync::Mutex<HashMap<tokio::runtime::Id, TestCwdAdmission>>,
    > = std::sync::LazyLock::new(Default::default);
    let runtime = tokio::runtime::Handle::try_current().ok()?.id();
    let mut by_runtime = BY_RUNTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Some(
        *by_runtime
            .entry(runtime)
            .or_insert_with(new_test_cwd_admission),
    )
}

/// Keeps a scope's admission selected on this thread; the previous selection
/// returns on drop, also when the scope's body panics.
#[cfg(test)]
struct CwdAdmissionScope {
    previous: Option<TestCwdAdmission>,
}

#[cfg(test)]
impl Drop for CwdAdmissionScope {
    fn drop(&mut self) {
        FIXTURE_CWD_ADMISSION.with(|slot| slot.set(self.previous));
    }
}

/// Run `body` with `admission` as this thread's provider-cwd exclusion.
#[cfg(test)]
fn with_cwd_admission<T>(admission: TestCwdAdmission, body: impl FnOnce() -> T) -> T {
    let _scope = CwdAdmissionScope {
        previous: FIXTURE_CWD_ADMISSION.with(|slot| slot.replace(Some(admission))),
    };
    body()
}

/// #270: run `body` with a provider-cwd admission private to this scope. A
/// fixture that settles synchronously on its own thread uses this so its result
/// does not depend on the tests beside it, and a nested scope gets a fresh one.
#[cfg(test)]
fn with_fixture_cwd_admission<T>(body: impl FnOnce() -> T) -> T {
    with_cwd_admission(new_test_cwd_admission(), body)
}

/// The body of one root's saga. The caller holds the provider-cwd exclusion
/// (proved by the guard argument) and the repository mutex.
#[allow(clippy::too_many_arguments)]
fn settle_one_item_under_exclusion(
    _cwd_exclusion: &CwdExclusion,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    run_id: Uuid,
    target_ref: &str,
    canonical_repo_dir: &Path,
    sandbox_base: &Path,
    item: &SourceWorktreeSettlementJournalItem,
    agent_caller: Option<Uuid>,
    automatic_config: Option<&crate::config::RuntimeConfig>,
) -> Result<bool> {
    if retained_intent_can_be_reproved(item) {
        let Some(reopened) = reprove_retained_intent_under_exclusion(
            store,
            active,
            completed,
            run_id,
            sandbox_base,
            item,
        )?
        else {
            return Ok(false);
        };
        let result = settle_one_item_under_exclusion(
            _cwd_exclusion,
            store,
            active,
            completed,
            run_id,
            target_ref,
            canonical_repo_dir,
            sandbox_base,
            &reopened,
            agent_caller,
            automatic_config,
        );
        if result.as_ref().is_err_and(settlement_policy_refused) {
            mark_unattempted_under_root(store, run_id, &reopened, true, true)?;
        }
        return result;
    }
    let _root_guard =
        crate::store::custody_lock_order::lock_custody_root_under_repository(item.custody_id)?;
    #[cfg(test)]
    pause_settlement_proof_if_requested(item.session_id);
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
    // The Arcs keep their names below; the guards shadow `active`/`completed`.
    // The fences are released around the long Git proofs and the worktree
    // deletion (#2790) and taken again by `reacquire_settlement_fences`.
    let active_lock = active;
    let completed_lock = completed;
    let mut active = match wait_for_settlement_lock(active_lock.read()) {
        Some(active) => active,
        None => {
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
        )?;
        return Ok(false);
    }
    let mut completed = match wait_for_settlement_lock(completed_lock.read()) {
        Some(completed) => completed,
        None => {
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
            )?;
            return Ok(false);
        }
    };
    let mut store_guard = match wait_for_settlement_lock(store.lock()) {
        Some(store) => store,
        None => {
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
            )?;
            return Ok(false);
        }
    };

    if let Some(caller) = agent_caller {
        store_guard.authorize_agent_source_worktree_root(
            caller,
            &item.repository_identity,
            item.custody_id,
        )?;
    }
    if let Some(config) = automatic_config {
        require_automatic_settlement_pressure(config, sandbox_base)?;
    }

    // #247: the last fence before the first effect. A Session that became
    // paused or unfinished work after its intent was committed (an Issue
    // launch, an operator pause) keeps its untouched sandbox; only this item
    // is closed, never the rest of the plan.
    if retain_resumable_before_effect(&store_guard, run_id, item)? {
        return Ok(false);
    }

    let completion_retry =
        store_guard.source_worktree_completion_retry_exists(run_id, item.session_id)?;
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
                    let mut unregistered = worktree_paths_are_absent_and_unregistered(
                        canonical_repo_dir,
                        original_root,
                        &quarantine_root,
                    )?;
                    // An audited completion retry may remove the one stale
                    // registration the retained marker authenticates, then
                    // continue the branch-only completion below (#2132).
                    if !unregistered
                        && completion_retry
                        && durable_tuple_matches
                        && let Some(marker) = authority.as_ref()
                    {
                        match remove_exact_stale_quarantine_registration_from_marker(
                            canonical_repo_dir,
                            original_root,
                            &quarantine_root,
                            item,
                            marker,
                        ) {
                            Ok(true) => {
                                unregistered = worktree_paths_are_absent_and_unregistered(
                                    canonical_repo_dir,
                                    original_root,
                                    &quarantine_root,
                                )?;
                            }
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(session_id=%item.session_id, error=%error, "Stale quarantine registration was retained");
                            }
                        }
                    }
                    if !unregistered {
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
                        git_worktree::DirectRefObservation::Commit(ref oid)
                            if completion_retry
                                && oid == &item.source_oid
                                && durable_tuple_matches
                                && git_worktree::source_ref_has_zero_registrations_locked(
                                    canonical_repo_dir,
                                    &item.source_ref,
                                )?
                                && !git_worktree::source_ref_has_symref_dependents_locked(
                                    canonical_repo_dir,
                                    &item.source_ref,
                                )? =>
                        {
                            let target = observe_preserving_target(
                                canonical_repo_dir,
                                target_ref,
                                &item.repository_identity,
                                &item.source_oid,
                            )?;
                            // A retained canonical marker plus exact path/ref
                            // absence permits completion of the interrupted
                            // remove-then-delete steps (the branch-only shape).
                            // Custody and ancestry are freshly checked; no
                            // holder proof is bypassed for an existing tree.
                            if let Err(error) =
                                git_worktree::complete_quarantine_and_delete_source_ref_locked(
                                    canonical_repo_dir,
                                    original_root,
                                    &quarantine_root,
                                    target_ref,
                                    &target.local_target_oid,
                                    &item.source_ref,
                                    &item.source_oid,
                                )
                            {
                                if let Err(restore_error) = compensate_branch_first_source_ref(
                                    canonical_repo_dir,
                                    original_root,
                                    &quarantine_root,
                                    &item.source_ref,
                                    &item.source_oid,
                                ) {
                                    tracing::warn!(session_id=%item.session_id, error=%restore_error, "Branch-only completion compensation was refused");
                                }
                                store_guard.mark_source_worktree_settlement_recovery_required(
                                    run_id, item.session_id, current_phase,
                                    SourceWorktreeSettlementRefusalV1::RefDeleteFailed.as_str(),
                                    "branch-only completion failed its exact ref/path postconditions",
                                )?;
                                tracing::warn!(session_id=%item.session_id, error=%error, "Branch-only completion retained its recovery obligation");
                                return Ok(false);
                            }
                            store_guard.advance_source_worktree_settlement_item(
                                run_id, item.session_id, SourceWorktreeSettlementPhaseV1::IntentCommitted,
                                SourceWorktreeSettlementPhaseV1::WorktreeRemoved, None,
                                Some("canonical removal authority and fresh custody/ancestry checks completed the branch-only residue"), None,
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

            // The tree walk, holder inventory and Git proofs are the long part
            // of a root. Hold no Store, active or completed guard across them:
            // the proof reads only the filesystem and Git, and every runtime
            // and durable tuple is re-checked after the fences return (#2790).
            drop(store_guard);
            drop(completed);
            drop(active);
            #[cfg(test)]
            settlement_unlocked_step_hook("first_proof");
            let first_proof = prove_quarantine_remove_authority(
                run_id,
                target_ref,
                canonical_repo_dir,
                original_root,
                &quarantine_root,
                item,
                None,
            );
            (active, completed, store_guard) =
                reacquire_settlement_fences(&active_lock, &completed_lock, store)?;
            let (observed_authority, first_tree) = match first_proof {
                Ok(proof) => proof,
                Err(DaemonError::QuarantineProofDeferred) => {
                    // No destructive effect follows an incomplete proof. Keep the
                    // intent and any authenticated marker replayable on the next
                    // apply/startup pass; terminal fences are immutable.
                    store_guard.advance_source_worktree_settlement_item(
                        run_id,
                        item.session_id,
                        current_phase.clone(),
                        current_phase,
                        None,
                        Some(QUARANTINE_PROOF_DEFERRED_OBSERVATION),
                        None,
                    )?;
                    return Ok(false);
                }
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
                if !retained.matches_quarantine_identity(&observed_authority) {
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
            #[cfg(test)]
            SETTLEMENT_BEFORE_FINAL_PROOF_TEST_HOOK.with(|slot| {
                if let Some(hook) = slot.borrow_mut().take() {
                    hook();
                }
            });
            drop(store_guard);
            drop(completed);
            drop(active);
            #[cfg(test)]
            settlement_unlocked_step_hook("final_proof");
            let final_proof = prove_quarantine_remove_authority(
                run_id,
                target_ref,
                canonical_repo_dir,
                original_root,
                &quarantine_root,
                item,
                Some(&first_tree),
            );
            (active, completed, store_guard) =
                reacquire_settlement_fences(&active_lock, &completed_lock, store)?;
            // The fences were open during the proof: the runtime and durable
            // tuple must still match before any Git effect follows it.
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
                    "runtime or durable path authority drifted during the final quarantine proof",
                )?;
                return Ok(false);
            }
            let (final_authority, _) = match final_proof {
                Ok(proof) => proof,
                Err(DaemonError::QuarantineProofDeferred) => {
                    // No destructive effect follows an incomplete proof. Keep the
                    // intent and any authenticated marker replayable on the next
                    // apply/startup pass; terminal fences are immutable.
                    store_guard.advance_source_worktree_settlement_item(
                        run_id,
                        item.session_id,
                        current_phase.clone(),
                        current_phase,
                        None,
                        Some(QUARANTINE_PROOF_DEFERRED_OBSERVATION),
                        None,
                    )?;
                    return Ok(false);
                }
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
            if !authority
                .as_ref()
                .is_some_and(|retained| retained.matches_quarantine_identity(&final_authority))
            {
                store_guard.mark_source_worktree_settlement_recovery_required(
                    run_id,
                    item.session_id,
                    current_phase,
                    SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                    "the final quarantine identity does not match its durable marker",
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
            // Other sandboxes share these refs. The exact source/target and
            // symbolic dependents of the source are settlement postconditions;
            // unrelated writes cannot invalidate a successful removal.
            let delete = if completion_retry {
                git_worktree::complete_quarantine_and_delete_source_ref_locked(
                    canonical_repo_dir,
                    original_root,
                    &quarantine_root,
                    target_ref,
                    final_authority.removal_target_oid().as_str(),
                    &item.source_ref,
                    &item.source_oid,
                )
            } else {
                git_worktree::delete_source_ref_atomically_locked(
                    canonical_repo_dir,
                    target_ref,
                    final_authority.removal_target_oid().as_str(),
                    &item.source_ref,
                    &item.source_oid,
                )
            };
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
                ) && !git_worktree::source_ref_has_symref_dependents_locked(
                    canonical_repo_dir,
                    &item.source_ref,
                )? && matches!(
                    git_worktree::observe_direct_ref_locked(canonical_repo_dir, target_ref)?,
                    git_worktree::DirectRefObservation::Commit(ref oid)
                        if oid == final_authority.removal_target_oid().as_str()
                ))
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
                    "source deletion did not preserve the exact source, target, and symbolic-dependent postconditions",
                )?;
                return Ok(false);
            }

            #[cfg(test)]
            SETTLEMENT_BEFORE_DANGLING_REMOVE_TEST_HOOK.with(|slot| {
                if let Some(hook) = slot.borrow_mut().take() {
                    hook();
                }
            });
            // Deleting the quarantined tree is the longest Git effect and needs
            // no Store or runtime fence: the source ref is already gone and the
            // post-removal checks below and `effect_phase_drift` re-read both.
            drop(store_guard);
            drop(completed);
            drop(active);
            #[cfg(test)]
            settlement_unlocked_step_hook("remove");
            let remove = git_worktree::remove_worktree_after_source_ref_delete_non_force_locked(
                canonical_repo_dir,
                original_root,
                &quarantine_root,
                &item.source_ref,
                &item.source_oid,
            );
            (active, completed, store_guard) =
                reacquire_settlement_fences(&active_lock, &completed_lock, store)?;
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
                    && !git_worktree::source_ref_has_symref_dependents_locked(
                        canonical_repo_dir,
                        &item.source_ref,
                    )?
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
                    ))
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

/// Remove exactly the stale Git registration of an absent quarantine when the
/// retained marker authenticates its administration (#2132). The source ref
/// must still be the direct exact commit; the stale admin directory's id and
/// path must match the marker's digests. `Ok(false)` means nothing was removed
/// and the caller keeps its existing fence.
fn remove_exact_stale_quarantine_registration_from_marker(
    canonical_repo_dir: &Path,
    original_root: &Path,
    quarantine_root: &Path,
    item: &SourceWorktreeSettlementJournalItem,
    marker: &QuarantineRemoveAuthorityV1,
) -> Result<bool> {
    if !matches!(
        git_worktree::observe_direct_ref_locked(canonical_repo_dir, &item.source_ref)?,
        git_worktree::DirectRefObservation::Commit(ref oid) if oid == &item.source_oid
    ) {
        return Ok(false);
    }
    let Some(stale) = git_worktree::observe_stale_quarantine_registration_locked(
        canonical_repo_dir,
        original_root,
        quarantine_root,
        &item.source_ref,
        &item.source_oid,
    )?
    else {
        return Ok(false);
    };
    let admin_dir = stale
        .admin_directory
        .to_str()
        .ok_or_else(|| DaemonError::Store("stale Git admin path is not UTF-8".into()))?;
    if !marker.matches_admin_identity(admin_dir, &stale.admin_id) {
        return Err(DaemonError::Process(
            "stale quarantine registration does not match the marker's Git administration".into(),
        ));
    }
    git_worktree::remove_stale_quarantine_registration_locked(
        canonical_repo_dir,
        original_root,
        quarantine_root,
        &item.source_ref,
        &item.source_oid,
        &stale,
    )?;
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
    static SETTLEMENT_BEFORE_FINAL_PROOF_TEST_HOOK:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static SETTLEMENT_UNLOCKED_STEP_TEST_HOOK:
        std::cell::RefCell<Option<Box<dyn FnMut(&'static str)>>> = std::cell::RefCell::new(None);
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

/// Test seam: pause one session's settlement item while it holds the
/// repository mutex and its custody stripe (Issue #606).
#[cfg(test)]
struct SettlementProofPause {
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static SETTLEMENT_PROOF_PAUSE: std::sync::LazyLock<
    std::sync::Mutex<HashMap<Uuid, SettlementProofPause>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

#[cfg(test)]
fn pause_settlement_proof_if_requested(session_id: Uuid) {
    let pause = {
        let mut slot = SETTLEMENT_PROOF_PAUSE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.remove(&session_id)
    };
    if let Some(pause) = pause {
        let _ = pause.reached.send(());
        let _ = pause
            .release
            .recv_timeout(std::time::Duration::from_secs(60));
    }
}

/// Test seam (#2790): runs on the settlement thread while the Store, active
/// and completed fences are released around a long Git step.
#[cfg(test)]
fn settlement_unlocked_step_hook(step: &'static str) {
    SETTLEMENT_UNLOCKED_STEP_TEST_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(step);
        }
    });
}

#[cfg(test)]
fn set_unlocked_step_test_hook(hook: impl FnMut(&'static str) + 'static) {
    SETTLEMENT_UNLOCKED_STEP_TEST_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn set_before_dangling_remove_test_hook(hook: impl FnOnce() + 'static) {
    SETTLEMENT_BEFORE_DANGLING_REMOVE_TEST_HOOK.with(|slot| {
        assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
    });
}

#[cfg(test)]
fn set_before_final_proof_test_hook(hook: impl FnOnce() + 'static) {
    SETTLEMENT_BEFORE_FINAL_PROOF_TEST_HOOK.with(|slot| {
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
            // Spending another complete deadline immediately just amplifies
            // contention. The durable intent schedules the later retry.
            Err(DaemonError::QuarantineProofDeferred) => {
                return Err(DaemonError::QuarantineProofDeferred);
            }
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
    let holder = if expected_tree.is_none() {
        super::reaper::prove_preliminary_quarantine_holders(&tree)?
    } else {
        // A shared preliminary snapshot never replaces the fresh final proof
        // after the durable removal marker has been committed.
        super::reaper::prove_quarantine_has_no_untrusted_same_uid_holders(&tree)?
    };
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
        // The unchanged-ref postconditions compare the local target branch;
        // ancestry was proved against the fetched tip in `observe_preserving_target`.
        removal_target_oid: SourceWorktreeGitOidV1::parse(target.local_target_oid)
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
    let target = git_worktree::observe_settlement_target_locked(canonical_repo_dir)?;
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
    effect_started: bool,
) -> Result<()> {
    if phase == SourceWorktreeSettlementPhaseV1::IntentCommitted
        && !effect_started
        && settlement_intent_paths_untouched(run_id, item).unwrap_or(false)
    {
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
        completed_entry_matches_stored(
            completed,
            &item.original_status,
            &item.original_updated_at,
            &item.sandbox_root,
            &item.sandbox_branch,
        )
    })
}

/// The one predicate for "the runtime completed entry is the stored session":
/// the quarantine fence (`runtime_completed_matches`) and the audit
/// (`exclude_runtime_completed_drift`, #2650) both use it, so a root the audit
/// calls eligible is not one the fence refuses as `custody_drift`.
fn completed_entry_matches_stored(
    completed: &super::types::CompletedSession,
    stored_status: &str,
    stored_updated_at: &str,
    sandbox_root: &str,
    sandbox_branch: &str,
) -> bool {
    completed_entry_mismatch(
        completed,
        stored_status,
        stored_updated_at,
        sandbox_root,
        sandbox_branch,
    )
    .is_none()
}

/// How far the stored `updated_at` may lead the runtime completed entry's and
/// still be the same terminal transition (#252, hub #2731).
///
/// The finalizer (`lifecycle.rs`) stamps the runtime copy with `Utc::now()`,
/// then persists status, session kind and final metadata, and each of those
/// Store writers stamps `updated_at` with its own `Utc::now()` (the metadata
/// writer's `to_rfc3339()` is the `+00:00` spelling seen in the stored value).
/// The completed entry is never re-stamped, so the stored row is a few
/// milliseconds later than the runtime copy for every root finalized in this
/// daemon's lifetime (observed: 11.2 ms), and the exact comparison retained
/// such a legitimately terminal root as `custody_unverified` until restart.
///
/// The window is one-sided and small: a runtime entry later than the stored
/// row (a newer completion the Store has not seen) and a stored write more
/// than this after the runtime copy (an unmirrored Store change such as an
/// archive or pending-archive write) still fail closed, as do status, root
/// and branch drift, which are compared exactly.
const RUNTIME_COMPLETED_TERMINAL_WRITE_SKEW: chrono::Duration = chrono::Duration::seconds(2);

/// True when `stored` is the runtime entry's `observed` terminal write, allowing
/// only the finalizer's own Store-after-runtime write order (#252).
fn stored_is_same_terminal_write(
    stored: chrono::DateTime<chrono::Utc>,
    observed: chrono::DateTime<chrono::Utc>,
) -> bool {
    let lead = stored.signed_duration_since(observed);
    lead >= chrono::Duration::zero() && lead <= RUNTIME_COMPLETED_TERMINAL_WRITE_SKEW
}

/// Mirror a Store session-metadata write's stamp into the runtime completed
/// entry's `updated_at` (#257). Call it under the same `completed` write lock
/// that applies the written field.
///
/// Title and description refinement, and the rename, description, rating,
/// project, workflow, active-task, label, parent and lead-pin writers, stamp
/// the row's `updated_at` with their own time; the runtime copy used to keep
/// the finalizer's stamp, so a write more than
/// [`RUNTIME_COMPLETED_TERMINAL_WRITE_SKEW`] after finalization retained the
/// root as `custody_unverified` (`check=runtime_completed_updated_at`) until a
/// restart reloaded `completed`.
///
/// Never a blind copy: the entry moves to exactly the stamp the write stored,
/// and only when the row reached that stamp from the entry's own `updated_at`
/// through stamped writes alone. The Store decides that inside each write's
/// transaction (#265, hub #2818: the write's run of stamped writes), so the
/// mirrors may arrive in any order and none is held back: the newest write of
/// the run moves the entry, an older one that arrives later is a no-op. A
/// Store change the runtime copy never saw (a newer completion, an archive,
/// any other unstamped write) starts a new run and stays visible as drift
/// after any later metadata write. A failed write, a write that changed no row
/// and an unparseable prior stamp leave the entry unchanged. Returns whether
/// the entry advanced.
pub(super) fn mirror_completed_store_write(
    completed: &mut super::types::CompletedSession,
    stamp: Option<crate::store::SessionWriteStamp>,
) -> bool {
    let Some(stamp) = stamp else {
        return false;
    };
    if !completed_store_write_follows(completed, &stamp) {
        return false;
    }
    completed.session.updated_at = stamp.stamped_at;
    completed.store_writes.updated_at = Some(super::types::AppliedStoreWrite {
        write_seq: stamp.write_seq,
        run_seq: stamp.run_seq,
        stamped_at: stamp.stamped_at,
    });
    true
}

/// True when the row reached `stamp` from the entry's `updated_at` through
/// stamped Store writes only (#265). A write older, by Store order, than the
/// one the entry holds never follows it. A newer one follows when it is of the
/// same run, or replaced exactly the entry's value (the #257 chain). An entry
/// no mirror advanced holds the finalizer's (or the restored row's) stamp,
/// and the write's run must start from it: the #252 window covers the
/// finalizer's Store write landing just after its runtime stamp, and is the
/// only place it is used.
fn completed_store_write_follows(
    completed: &super::types::CompletedSession,
    stamp: &crate::store::SessionWriteStamp,
) -> bool {
    let current = completed.session.updated_at;
    let advanced = completed
        .store_writes
        .updated_at
        .filter(|applied| applied.stamped_at == current);
    if let Some(applied) = advanced {
        if stamp.write_seq <= applied.write_seq {
            return false;
        }
        if stamp.run_seq == applied.run_seq {
            return true;
        }
    }
    if stamp.previous_updated_at == Some(current) {
        return true;
    }
    advanced.is_none()
        && stamp
            .run_base
            .is_some_and(|base| stored_is_same_terminal_write(base, current))
}

/// Apply one mirrored Store write to a completed entry: the field only when
/// the write is newer, by Store commit order, than the last one that set it
/// (#263, #265: a late mirror of an older write never puts its value back),
/// then the stamp ([`mirror_completed_store_write`]).
fn mirror_into_completed(
    entry: &mut super::types::CompletedSession,
    field: super::types::CompletedStoreField,
    stamp: Option<crate::store::SessionWriteStamp>,
    apply: impl FnOnce(&mut rsi_common::types::Session),
) {
    if entry.store_writes.fields.admit(field, stamp.as_ref()) {
        apply(&mut entry.session);
    }
    mirror_completed_store_write(entry, stamp);
}

/// Apply a field a Store write persisted to the session's runtime entry,
/// wherever the session lives (#257, #265).
///
/// A completed entry takes the field and the write's stamp under one
/// `completed` write lock ([`mirror_into_completed`]); a live entry takes the
/// field, ordered the same way. Call it after the Store write returned, so
/// both lookups follow its commit. A session in neither map is between them
/// (a Continue, its rollback, the finalizer or a rotation is moving it); the
/// mover publishes it through [`publish_from_store`], which reads this write
/// from the Store, so the write is never lost.
pub(super) async fn apply_store_write(
    active: &tokio::sync::RwLock<HashMap<Uuid, super::TrackedSession>>,
    completed: &tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>,
    session_id: Uuid,
    field: super::types::CompletedStoreField,
    stamp: Option<crate::store::SessionWriteStamp>,
    apply: impl FnOnce(&mut rsi_common::types::Session),
) {
    {
        let mut completed = completed.write().await;
        if let Some(entry) = completed.get_mut(&session_id) {
            mirror_into_completed(entry, field, stamp, apply);
            return;
        }
    }
    let mut active = active.write().await;
    let Some(tracked) = active.get_mut(&session_id) else {
        return;
    };
    if tracked.store_fields.admit(field, stamp.as_ref()) {
        apply(&mut tracked.session);
    }
}

/// [`apply_store_write`] for a completed entry alone (tests drive the mirror
/// order directly through it).
#[cfg(test)]
pub(super) async fn apply_completed_store_write(
    completed: &tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>,
    session_id: Uuid,
    field: super::types::CompletedStoreField,
    stamp: Option<crate::store::SessionWriteStamp>,
    apply: impl FnOnce(&mut rsi_common::types::Session),
) {
    let mut completed = completed.write().await;
    if let Some(entry) = completed.get_mut(&session_id) {
        mirror_into_completed(entry, field, stamp, apply);
    }
}

/// One session's Store row and its committed stamped writes, read together
/// under the Store lock (#265): the row reflects every stamped write of the
/// session up to the last one recorded and none after it.
pub(super) struct StoreFieldSnapshot {
    row: rsi_common::types::Session,
    writes: Option<crate::store::SessionWrites>,
}

impl StoreFieldSnapshot {
    async fn read(
        store: &tokio::sync::Mutex<crate::store::Store>,
        session_id: Uuid,
    ) -> Result<Option<Self>> {
        let store = store.lock().await;
        Self::read_locked(&store, session_id)
    }

    /// [`Self::read`] for a caller already holding the Store lock.
    fn read_locked(store: &crate::store::Store, session_id: Uuid) -> Result<Option<Self>> {
        let Some(row) = store.get_session(session_id)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            row,
            writes: crate::store::session_writes(session_id),
        }))
    }

    /// No stamped write of the session committed since the read.
    fn is_current(&self) -> bool {
        let read = self.writes.map_or(0, |writes| writes.last.write_seq);
        crate::store::last_session_write_seq(self.row.id) == read
    }

    /// Take from the row each mirrored field a stamped Store write set after
    /// the last write of it `fields` applied: the mirror that write would have
    /// made. A field no stamped write changed keeps the entry's value, so an
    /// unstamped Store change is never adopted here (it stays visible as
    /// drift).
    fn apply_fields(
        &self,
        session: &mut rsi_common::types::Session,
        fields: &mut super::types::StoreFieldOrder,
    ) {
        let Some(writes) = self.writes else {
            return;
        };
        for field in super::types::CompletedStoreField::ALL {
            if fields.admit_seq(field, writes.field(field)) {
                super::types::copy_store_field(field, &self.row, session);
            }
        }
    }

    /// Reconcile a completed entry: the mirrored fields, and its `updated_at`
    /// when the row's last change is the last stamped write (the mirror that
    /// write would have made).
    pub(super) fn reconcile_completed(&self, entry: &mut super::types::CompletedSession) {
        self.apply_fields(&mut entry.session, &mut entry.store_writes.fields);
        let last = self
            .writes
            .map(|writes| writes.last)
            .filter(|last| last.stamped_at == self.row.updated_at);
        if last.is_some() {
            mirror_completed_store_write(entry, last);
        }
    }

    /// Reconcile a live entry's mirrored fields.
    pub(super) fn reconcile_active(&self, tracked: &mut super::TrackedSession) {
        self.apply_fields(&mut tracked.session, &mut tracked.store_fields);
    }
}

/// Optimistic reads before a publisher serializes with the Store writers.
const PUBLISH_FROM_STORE_ATTEMPTS: usize = 16;

/// Failed reads of the session row, with the Store lock held, after which a
/// serialized publisher starts logging that it is still waiting for a
/// readable Store (it keeps retrying; #2883).
const PUBLISH_FROM_STORE_READ_FAILURES_LOGGED_FROM: u32 = 16;

/// Pause between serialized attempts that found the Store busy or the row
/// changing under a second Store handle. It doubles every
/// [`PUBLISH_FROM_STORE_BACKOFF_STEP`] consecutive read failures, up to
/// [`PUBLISH_FROM_STORE_BACKOFF_DOUBLINGS`] times (64 ms).
const PUBLISH_FROM_STORE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1);
const PUBLISH_FROM_STORE_BACKOFF_STEP: u32 = 4;
const PUBLISH_FROM_STORE_BACKOFF_DOUBLINGS: u32 = 6;

/// The pause after `read_failures` consecutive failed reads of the row.
fn publish_from_store_backoff(read_failures: u32) -> std::time::Duration {
    let doublings =
        (read_failures / PUBLISH_FROM_STORE_BACKOFF_STEP).min(PUBLISH_FROM_STORE_BACKOFF_DOUBLINGS);
    PUBLISH_FROM_STORE_BACKOFF * (1 << doublings)
}

/// Test seam: awaited with the attempt number (1-based; one past
/// [`PUBLISH_FROM_STORE_ATTEMPTS`] for the serialized fallback) in the gap
/// between a publisher's Store read and its map write lock. A test commits
/// and mirrors a Store write there.
pub(super) type PublishGap<'a> = &'a (dyn Fn(usize) -> Publication<'a> + Send + Sync);

/// Publish a session moving between the runtime maps, reconciled from the
/// Store (#265, hub #2819, #2874): a Continue publishing it live or rolling
/// back, the finalizer, a rotation. A mirror of a Store write that committed
/// while the session was in neither map found no entry; this read sees that
/// write.
///
/// The row is read under the Store lock, then the session is inserted under
/// the map's write lock only if no stamped write of it committed since the
/// read (else it reads again). A write committing after that check mirrors
/// after the insert, into the published entry. Holds no lock while waiting
/// for the other. Call it holding neither the Store lock nor `map`'s.
///
/// An entry whose store-backed fields were not reconciled is never published
/// (#2883): if the row cannot be read, the serialized fallback keeps retrying
/// with a bounded backoff, holding no lock between attempts, until the Store
/// is readable. The entry is published only once reconciled; a caller that
/// gives up (drops this future) publishes nothing.
///
/// A stale snapshot is never published: after
/// [`PUBLISH_FROM_STORE_ATTEMPTS`] optimistic reads that each lost the race
/// to a writer, the publisher takes the map's write lock and then only
/// *tries* the Store lock (it never awaits one lock holding the other, so it
/// cannot deadlock a writer). Holding both, no writer can commit, so the row
/// it reads is current and its fields are reconciled before the insert.
pub(super) async fn publish_from_store<E>(
    map: &tokio::sync::RwLock<HashMap<Uuid, E>>,
    store: &tokio::sync::Mutex<crate::store::Store>,
    session_id: Uuid,
    entry: E,
    reconcile: impl Fn(&StoreFieldSnapshot, &mut E),
) {
    publish_from_store_with_gap(map, store, session_id, entry, reconcile, None, None).await;
}

/// [`publish_from_store`] with test seams. `gap` is described at
/// [`PublishGap`]; `read_failed` is awaited, holding no lock, with the count
/// of consecutive failed row reads after each failed serialized read.
pub(super) async fn publish_from_store_with_gap<E>(
    map: &tokio::sync::RwLock<HashMap<Uuid, E>>,
    store: &tokio::sync::Mutex<crate::store::Store>,
    session_id: Uuid,
    mut entry: E,
    reconcile: impl Fn(&StoreFieldSnapshot, &mut E),
    gap: Option<PublishGap<'_>>,
    read_failed: Option<PublishGap<'_>>,
) {
    for attempt in 1..=PUBLISH_FROM_STORE_ATTEMPTS {
        let snapshot = StoreFieldSnapshot::read(store, session_id).await;
        if let Some(gap) = gap {
            gap(attempt).await;
        }
        let mut published = map.write().await;
        let snapshot = match snapshot {
            Ok(snapshot) if snapshot.as_ref().is_none_or(StoreFieldSnapshot::is_current) => {
                snapshot
            }
            // A write committed since the read, or the read failed: read again.
            _ => continue,
        };
        if let Some(snapshot) = snapshot.as_ref() {
            reconcile(snapshot, &mut entry);
        }
        published.insert(session_id, entry);
        return;
    }
    if let Some(gap) = gap {
        gap(PUBLISH_FROM_STORE_ATTEMPTS + 1).await;
    }
    let mut read_failures: u32 = 0;
    loop {
        {
            let mut published = map.write().await;
            if let Ok(store) = store.try_lock() {
                match StoreFieldSnapshot::read_locked(&store, session_id) {
                    Ok(snapshot)
                        if snapshot.as_ref().is_none_or(StoreFieldSnapshot::is_current) =>
                    {
                        if let Some(snapshot) = snapshot.as_ref() {
                            reconcile(snapshot, &mut entry);
                        }
                        published.insert(session_id, entry);
                        return;
                    }
                    // A writer through another Store handle committed: read again.
                    Ok(_) => read_failures = 0,
                    Err(error) => {
                        read_failures = read_failures.saturating_add(1);
                        if read_failures >= PUBLISH_FROM_STORE_READ_FAILURES_LOGGED_FROM
                            && read_failures.is_power_of_two()
                        {
                            tracing::error!(%session_id, %error, read_failures,
                                "runtime publication cannot read the session row; \
                                 retrying until the Store is readable");
                        }
                    }
                }
            }
        }
        if let (Some(read_failed), true) = (read_failed, read_failures > 0) {
            read_failed(read_failures as usize).await;
        }
        tokio::time::sleep(publish_from_store_backoff(read_failures)).await;
    }
}

/// A publication future, boxed: its callers (Continue and its rollbacks,
/// rotation, the finalizer) are already large futures, and the entry moved
/// into it would otherwise grow each of them by a whole session.
pub(super) type Publication<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

/// [`publish_from_store`] into `completed`.
pub(super) fn publish_completed_from_store<'a>(
    completed: &'a tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>,
    store: &'a tokio::sync::Mutex<crate::store::Store>,
    session_id: Uuid,
    entry: super::types::CompletedSession,
) -> Publication<'a> {
    Box::pin(publish_from_store(
        completed,
        store,
        session_id,
        entry,
        StoreFieldSnapshot::reconcile_completed,
    ))
}

/// [`publish_from_store`] into `active`.
pub(super) fn publish_active_from_store<'a>(
    active: &'a tokio::sync::RwLock<HashMap<Uuid, super::TrackedSession>>,
    store: &'a tokio::sync::Mutex<crate::store::Store>,
    session_id: Uuid,
    tracked: super::TrackedSession,
) -> Publication<'a> {
    Box::pin(publish_from_store(
        active,
        store,
        session_id,
        tracked,
        StoreFieldSnapshot::reconcile_active,
    ))
}

/// The first field on which the runtime completed entry disagrees with the
/// stored session, as `check=runtime_completed_<field> stored=<..> observed=<..>`
/// (#2731). `None` when [`completed_entry_matches_stored`] holds.
fn completed_entry_mismatch(
    completed: &super::types::CompletedSession,
    stored_status: &str,
    stored_updated_at: &str,
    sandbox_root: &str,
    sandbox_branch: &str,
) -> Option<String> {
    let observed_status = format!("{:?}", completed.session.status);
    if observed_status != stored_status {
        return Some(format!(
            "check=runtime_completed_status stored={stored_status} observed={observed_status}"
        ));
    }
    let persisted_updated_at = chrono::DateTime::parse_from_rfc3339(stored_updated_at)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc));
    if !persisted_updated_at
        .is_some_and(|stored| stored_is_same_terminal_write(stored, completed.session.updated_at))
    {
        return Some(format!(
            "check=runtime_completed_updated_at stored={stored_updated_at} observed={}",
            completed
                .session
                .updated_at
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        ));
    }
    if completed.session.sandbox_root.as_deref() != Some(Path::new(sandbox_root)) {
        return Some(format!(
            "check=runtime_completed_sandbox_root stored={sandbox_root} observed={}",
            completed
                .session
                .sandbox_root
                .as_deref()
                .map_or("none".to_string(), |root| root.display().to_string())
        ));
    }
    if completed.session.sandbox_branch.as_deref() != Some(sandbox_branch) {
        return Some(format!(
            "check=runtime_completed_sandbox_branch stored={sandbox_branch} observed={}",
            completed
                .session
                .sandbox_branch
                .as_deref()
                .unwrap_or("none")
        ));
    }
    None
}

/// #2650: the audit reads Store rows only, but the quarantine fence also
/// requires the session's runtime completed entry to equal the stored session.
/// A row whose entry disagrees (a Store write that moved `updated_at` or the
/// status without the runtime copy, e.g. an archive in progress) would be
/// planned eligible and then refused `custody_drift` on every pass. Mark it
/// not eligible at the audit instead; the fence itself is unchanged.
fn exclude_runtime_completed_drift(
    inventory: &mut [SourceWorktreeInventoryRow],
    completed: &HashMap<Uuid, super::types::CompletedSession>,
) {
    for row in inventory {
        if row.agent_scope_disposition.is_some() {
            continue;
        }
        let (Some(session_id), Some(status), Some(updated_at)) = (
            row.session_id,
            row.status.as_deref(),
            row.session_updated_at.as_deref(),
        ) else {
            continue;
        };
        let Some(entry) = completed.get(&session_id) else {
            continue;
        };
        if let Some(detail) = completed_entry_mismatch(
            entry,
            status,
            updated_at,
            &row.sandbox_root,
            &row.sandbox_branch,
        ) {
            row.agent_scope_disposition =
                Some(rsi_common::cohort_settlement::SourceWorktreeDispositionV1::CustodyUnverified);
            row.agent_scope_detail = Some(detail);
        }
    }
}

/// Which recovery a retained run gets: the startup pass, which also resumes a
/// retained intent that can be reproved, or the ticker's deferred retry
/// (#271, #273), which resumes only unfinished items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryPass {
    Startup,
    DeferredRetry,
}

impl RecoveryPass {
    fn resumes(self, item: &SourceWorktreeSettlementJournalItem) -> bool {
        if !item.phase.is_terminal() {
            return true;
        }
        self == Self::Startup && retained_intent_can_be_reproved(item)
    }
}

fn retained_intent_can_be_reproved(item: &SourceWorktreeSettlementJournalItem) -> bool {
    item.phase == SourceWorktreeSettlementPhaseV1::RecoveryRequired
        && item.refusal_code.as_deref() == Some("recovery_proof_failed")
        && item.after_observation.as_deref()
            == Some(
                "retained intent followed an earlier terminal stop and cannot prove no prior effect",
            )
        && item.before_observation.is_none()
        && item.reserved_effects == 0
        && item.active_effects == 0
}

/// Called with provider-cwd and repository exclusion. Unlike the old quarantine
/// fence, this shape has no proof that an effect even began. Prove the retained
/// root, refs and custody unchanged before appending any superseding intent.
fn reprove_retained_intent_under_exclusion(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    run_id: Uuid,
    sandbox_base: &Path,
    item: &SourceWorktreeSettlementJournalItem,
) -> Result<Option<SourceWorktreeSettlementJournalItem>> {
    let _root_guard =
        crate::store::custody_lock_order::lock_custody_root_under_repository(item.custody_id)?;
    let Some(active) = wait_for_settlement_lock(active.read()) else {
        return Ok(None);
    };
    let Some(completed) = wait_for_settlement_lock(completed.read()) else {
        return Ok(None);
    };
    let Some(mut store) = wait_for_settlement_lock(store.lock()) else {
        return Ok(None);
    };
    let original = Path::new(&item.sandbox_root);
    let proof = (|| -> Result<bool> {
        let quarantine =
            git_worktree::derive_settlement_quarantine_path(original, run_id, item.session_id)?;
        if !root_is_exact_sandbox(original, sandbox_base, item.session_id)
            || nofollow_path_exists(&quarantine)?
            || !runtime_completed_matches(&completed, item)
            || runtime_overlaps_settlement_roots(&active, item.session_id, original, &quarantine)
            || !journal_inventory_matches(&store, run_id, item, original)?
        {
            return Ok(false);
        }
        let repository = Path::new(&item.canonical_repo_dir);
        let observed = git_worktree::observe_worktree_locked(repository, original)?;
        Ok(!observed.root_is_symlink
            && observed.root_exists
            && observed.registered
            && observed.clean
            && observed.registered_branch.as_deref() == Some(item.source_ref.as_str())
            && observed.head_ref.as_deref() == Some(item.source_ref.as_str())
            && observed.registered_head.as_deref() == Some(item.source_oid.as_str())
            && observed.head_oid.as_deref() == Some(item.source_oid.as_str())
            && observed.clean_state_digest == item.clean_state_digest
            && matches!(git_worktree::observe_direct_ref_locked(repository, &item.source_ref)?,
                git_worktree::DirectRefObservation::Commit(oid) if oid == item.source_oid)
            && git_worktree::source_ref_is_registered_only_at_locked(
                repository,
                &item.source_ref,
                original,
                &item.source_oid,
            )?
            && !git_worktree::source_ref_has_symref_dependents_locked(
                repository,
                &item.source_ref,
            )?)
    })();
    match proof {
        Ok(true) => {}
        Ok(false) => return Ok(None),
        Err(error) => {
            tracing::warn!(run_id=%run_id, session_id=%item.session_id, error=%error,
                "Retained-intent reproof failed; preserving the original recovery fence");
            return Ok(None);
        }
    }
    if !store.reopen_source_worktree_untouched_intent_fence(run_id, item.session_id)? {
        return Ok(None);
    }
    Ok(store
        .list_source_worktree_settlement_journal_items(run_id)?
        .into_iter()
        .find(|current| current.session_id == item.session_id))
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
        // A retained no-effect fence has no quarantine yet and is not an
        // effect-capable alias. The normal proof checks both path aliases
        // again after the audited transition to IntentCommitted.
        if retained_intent_can_be_reproved(item) {
            &[]
        } else {
            &aliases
        },
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

/// Retry a transient database tail once per driver pass, using the exact normal
/// replay proof. Persistent database faults stay replayable for startup, never
/// become an inspection-only RecoveryRequired item merely because SQLite failed.
fn retry_settlement_database_items(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    run: &SourceWorktreeSettlementRunV1,
    sandbox_base: &Path,
) -> Result<()> {
    retry_settlement_database_items_with(store, run, |item| {
        settle_one_item_with_agent_authority(
            store,
            active,
            completed,
            run.run_id,
            &run.target_ref,
            Path::new(&run.canonical_repo_dir),
            sandbox_base,
            item,
            true,
            None,
            None,
        )
    })
}

fn retry_settlement_database_items_with(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run: &SourceWorktreeSettlementRunV1,
    mut settle: impl FnMut(&SourceWorktreeSettlementJournalItem) -> Result<bool>,
) -> Result<()> {
    let journal = {
        let guard = store.blocking_lock_checked()?;
        let receipt = guard
            .get_source_worktree_settlement_run(run.run_id)?
            .ok_or_else(|| DaemonError::Store("settlement receipt disappeared".into()))?;
        let retry_ids = receipt
            .items
            .iter()
            .filter(|item| {
                item.phase == SourceWorktreeSettlementPhaseV1::BranchRemoved
                    && item.refusal_code
                        == Some(SourceWorktreeSettlementRefusalV1::DatabaseSettlementFailed)
            })
            .map(|item| item.session_id)
            .collect::<HashSet<_>>();
        if retry_ids.is_empty() {
            return Ok(());
        }
        guard
            .list_source_worktree_settlement_journal_items(run.run_id)?
            .into_iter()
            .filter(|item| retry_ids.contains(&item.session_id))
            .collect::<Vec<_>>()
    };
    for item in &journal {
        // Effects are already complete; recovery owes the exact database tail
        // even if the initiating manager or automatic pressure changed.
        if let Err(error) = settle(item) {
            tracing::warn!(run_id=%run.run_id, session_id=%item.session_id, error=%error,
                "Database settlement retry retained its durable replay obligation");
            retain_failed_settlement_item(store, run.run_id, item.session_id, &error)?;
        }
    }
    Ok(())
}

/// Wait on a queued lock future on the blocking settlement thread. Keeping the
/// same future preserves Tokio's FIFO admission (including queued provider-cwd
/// readers between roots). A short wall-clock bound breaks inverse lock waits;
/// this also works in synchronous recovery tests without a Tokio timer driver.
/// Run `body` under a bounded read of the runtime completed map (never held
/// across a Store lock); a busy map is a retryable refusal, as for `active`.
fn with_runtime_completed(
    completed: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    body: impl FnOnce(&HashMap<Uuid, super::types::CompletedSession>),
) -> Result<()> {
    let guard = wait_for_settlement_lock(completed.read()).ok_or_else(|| {
        DaemonError::InvalidParam(
            "runtime state is busy; audit was not applied and may be retried".into(),
        )
    })?;
    body(&*guard);
    Ok(())
}

/// The roots an audit kept (#2635), in audit order with canonical sequences:
/// every observed item that is not eligible, with its retention class and the
/// audit's diagnostic, for the run record.
fn retained_roots_of(
    report: &rsi_common::cohort_settlement::SourceWorktreeCohortAuditV1,
) -> Vec<rsi_common::cohort_settlement::SourceWorktreeRetainedRootV1> {
    report
        .items
        .iter()
        .filter(|item| !item.disposition.is_eligible())
        .enumerate()
        .map(
            |(sequence, item)| rsi_common::cohort_settlement::SourceWorktreeRetainedRootV1 {
                sequence: sequence as u32,
                session_id: item.session_id,
                custody_id: item.custody_id,
                session_status: item.status.clone(),
                sandbox_root: item.sandbox_root.clone(),
                source_ref: item.source_ref.clone(),
                disposition: item.disposition.clone(),
                diagnostic: item.diagnostic.clone(),
            },
        )
        .collect()
}

fn wait_for_settlement_lock<F: std::future::Future>(future: F) -> Option<F::Output> {
    wait_for_settlement_lock_for(future, std::time::Duration::from_secs(1))
}

fn wait_for_settlement_lock_for<F: std::future::Future>(
    future: F,
    bound: std::time::Duration,
) -> Option<F::Output> {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
    let deadline = std::time::Instant::now() + bound;
    loop {
        if let std::task::Poll::Ready(guard) = future.as_mut().poll(&mut context) {
            return Some(guard);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// The observation a holder-inventory deadline leaves on a retained item: the
/// proof was incomplete, so the item keeps its phase and any marker.
const QUARANTINE_PROOF_DEFERRED_OBSERVATION: &str = "quarantine_proof_deferred";

/// The observation prefix of an effect-started item whose attempt surrendered
/// a bounded wait (#273): `settlement_surrendered: runtime_state_contended` or
/// `settlement_surrendered: root_busy`. The item keeps its phase and marker.
const SETTLEMENT_SURRENDERED_PREFIX: &str = "settlement_surrendered: ";

/// Why an attempt stopped short on one item without deciding anything about
/// it: a cwd-admission or runtime-state wait expired, or a lock-order-bounded
/// Store or root-lock wait surrendered `root_busy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettlementSurrender {
    RuntimeStateContended,
    RootBusy,
}

impl SettlementSurrender {
    fn observation(self) -> String {
        let reason = match self {
            Self::RuntimeStateContended => {
                SourceWorktreeSettlementRefusalV1::RuntimeStateContended.as_str()
            }
            Self::RootBusy => "root_busy",
        };
        format!("{SETTLEMENT_SURRENDERED_PREFIX}{reason}")
    }
}

/// Surrenders noted where the attempt may hold a root lock or the repository
/// mutex, so it must not wait for the Store (`custody_lock_order`). Keyed by
/// run, then item; written by [`flush_settlement_surrenders`] at a point that
/// holds a Store guard. This mutex is a leaf: nothing is awaited under it.
static PENDING_SETTLEMENT_SURRENDERS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<Uuid, HashMap<Uuid, SettlementSurrender>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn note_settlement_surrender(run_id: Uuid, session_id: Uuid, reason: SettlementSurrender) {
    PENDING_SETTLEMENT_SURRENDERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(run_id)
        .or_default()
        .insert(session_id, reason);
}

/// Write every surrender noted for `run_id` under the caller's Store guard. A
/// failed write is logged and leaves the item as the surrender left it.
fn flush_settlement_surrenders(store: &crate::store::Store, run_id: Uuid) {
    let Some(pending) = PENDING_SETTLEMENT_SURRENDERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&run_id)
    else {
        return;
    };
    if let Err(error) = record_settlement_surrenders(store, run_id, |session_id| {
        pending.get(&session_id).copied()
    }) {
        tracing::warn!(run_id=%run_id, %error, "Settlement surrender reason was not recorded");
    }
}

/// Record `settlement_surrendered: <reason>` on each item `reason_for` names
/// that an attempt left effect-started with nothing retryable on record. An
/// existing deadline deferral is kept; a proven-untouched item needs no retry
/// from here.
fn record_settlement_surrenders(
    store: &crate::store::Store,
    run_id: Uuid,
    reason_for: impl Fn(Uuid) -> Option<SettlementSurrender>,
) -> Result<()> {
    for item in store.list_source_worktree_settlement_journal_items(run_id)? {
        let Some(reason) = reason_for(item.session_id) else {
            continue;
        };
        if !settlement_surrender_recordable(run_id, &item) {
            continue;
        }
        let observation = reason.observation();
        store.advance_source_worktree_settlement_item(
            run_id,
            item.session_id,
            item.phase.clone(),
            item.phase.clone(),
            None,
            Some(&observation),
            item.refusal_code.as_deref(),
        )?;
        tracing::warn!(run_id=%run_id, session_id=%item.session_id, observation,
            "Settlement attempt surrendered an effect-started item; retained for retry");
    }
    Ok(())
}

fn settlement_surrender_recordable(
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
) -> bool {
    if item.phase.is_terminal() {
        return false;
    }
    // The database tail owns its own retry and its refusal text.
    if item.phase == SourceWorktreeSettlementPhaseV1::BranchRemoved
        && item.refusal_code.as_deref()
            == Some(SourceWorktreeSettlementRefusalV1::DatabaseSettlementFailed.as_str())
    {
        return false;
    }
    if item.after_observation.as_deref() == Some(QUARANTINE_PROOF_DEFERRED_OBSERVATION) {
        return false;
    }
    !settlement_intent_paths_untouched(run_id, item).unwrap_or(false)
}

/// Whether the ticker retries `observation` on a manual run: a holder-deadline
/// deferral (#271) or a bounded-wait surrender (#273). Both clear on their own
/// and a retry takes no effect without a complete fresh proof, so either is
/// due on every pass.
fn manual_retry_observation(observation: &str) -> bool {
    observation == QUARANTINE_PROOF_DEFERRED_OBSERVATION
        || observation.starts_with(SETTLEMENT_SURRENDERED_PREFIX)
}

/// The nonterminal manual (agent or operator) runs holding an unfinished item
/// the 300 s ticker retries (see [`manual_retry_observation`]). A run with no
/// attribution predates origins and is manual. An automatic run's deferral is
/// resumed by the automatic pass, under disk pressure, as before.
fn retryable_deferred_settlement_runs(store: &crate::store::Store) -> Result<Vec<Uuid>> {
    let mut runs = Vec::new();
    let mut after_run_id = None;
    loop {
        let run_ids =
            store.list_nonterminal_source_worktree_settlement_run_ids(after_run_id, 64)?;
        let Some(&last) = run_ids.last() else {
            return Ok(runs);
        };
        after_run_id = Some(last);
        for run_id in run_ids {
            let retryable = store
                .list_source_worktree_settlement_journal_items(run_id)?
                .iter()
                .filter(|item| !item.phase.is_terminal())
                .any(|item| {
                    item.after_observation
                        .as_deref()
                        .is_some_and(manual_retry_observation)
                });
            if retryable && settlement_run_is_manual(store, run_id)? {
                runs.push(run_id);
            }
        }
    }
}

fn settlement_run_is_manual(store: &crate::store::Store, run_id: Uuid) -> Result<bool> {
    let Some(run) = store.get_source_worktree_settlement_run(run_id)? else {
        return Ok(false);
    };
    Ok(run.attribution.as_ref().map(|a| a.origin)
        != Some(rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Automatic))
}

/// Reload the journal after a failure: an effect may have advanced its phase.
/// Bounded lock surrender leaves the exact recovery obligation retryable; the
/// caller may still hold a root lock or the repository mutex, so its reason is
/// only noted here and written once the attempt holds the Store (#273).
fn retain_failed_settlement_item(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run_id: Uuid,
    session_id: Uuid,
    error: &DaemonError,
) -> Result<()> {
    if crate::store::custody_lock_order::is_lock_order_busy_error(error) {
        note_settlement_surrender(run_id, session_id, SettlementSurrender::RootBusy);
        return Ok(());
    }
    let store = store.blocking_lock_checked()?;
    retain_failed_settlement_items(&store, run_id, Some(session_id), error)
}

fn retain_failed_settlement_items(
    store: &crate::store::Store,
    run_id: Uuid,
    session_id: Option<Uuid>,
    error: &DaemonError,
) -> Result<()> {
    if crate::store::custody_lock_order::is_lock_order_busy_error(error) {
        // The caller holds the Store guard, so the surrender is written now.
        if let Err(record_error) = record_settlement_surrenders(store, run_id, |id| {
            session_id
                .is_none_or(|session_id| session_id == id)
                .then_some(SettlementSurrender::RootBusy)
        }) {
            tracing::warn!(run_id=%run_id, error=%record_error,
                "Settlement surrender reason was not recorded");
        }
        return Ok(());
    }
    let receipt = store
        .get_source_worktree_settlement_run(run_id)?
        .ok_or_else(|| DaemonError::Store("settlement receipt disappeared".into()))?;
    let journal = store.list_source_worktree_settlement_journal_items(run_id)?;
    let observation = format!("settlement attempt failed: {error}");
    for item in receipt.items {
        if !item.phase.is_terminal()
            && !(item.phase == SourceWorktreeSettlementPhaseV1::BranchRemoved
                && item.refusal_code
                    == Some(SourceWorktreeSettlementRefusalV1::DatabaseSettlementFailed))
            && session_id.is_none_or(|id| id == item.session_id)
        {
            let untouched = journal
                .iter()
                .find(|row| row.session_id == item.session_id)
                .is_some_and(|row| settlement_intent_paths_untouched(run_id, row).unwrap_or(false));
            if untouched {
                store.mark_source_worktree_settlement_unattempted(
                    run_id,
                    item.session_id,
                    &observation,
                )?;
            } else {
                store.mark_source_worktree_settlement_recovery_required(
                    run_id,
                    item.session_id,
                    item.phase,
                    SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                    &observation,
                )?;
            }
        }
    }
    Ok(())
}

fn mark_item_stopped_blocking(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
    code: SourceWorktreeSettlementRefusalV1,
    observation: &str,
) -> Result<()> {
    if code == SourceWorktreeSettlementRefusalV1::RuntimeStateContended {
        // Contention says nothing about prior effects. Keep effect-started or
        // ambiguous items pending for exact replay; release proven untouched
        // intents as a typed refusal that a fresh audit can retry. The kept
        // item's reason is written once the attempt holds the Store (#273).
        if !settlement_intent_paths_untouched(run_id, item).unwrap_or(false) {
            note_settlement_surrender(
                run_id,
                item.session_id,
                SettlementSurrender::RuntimeStateContended,
            );
            return Ok(());
        }
        return store
            .blocking_lock_checked()?
            .mark_source_worktree_settlement_refused(
                run_id,
                item.session_id,
                item.phase.clone(),
                code.as_str(),
                observation,
            );
    }
    let store = store.blocking_lock_checked()?;
    if settlement_intent_paths_untouched(run_id, item).unwrap_or(false) {
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
        .blocking_lock_checked()?
        .mark_source_worktree_settlement_recovery_required(
            run_id,
            item.session_id,
            item.phase.clone(),
            SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
            observation,
        )
}

/// The stable part of a failure (an RPC code, else the text before the first
/// colon) so a path or id inside the message does not defeat notice dedupe.
fn settlement_error_class(error: &DaemonError) -> String {
    if let DaemonError::StructuredRpc { data, .. } = error
        && let Some(code) = data["code"].as_str()
    {
        return code.chars().take(80).collect();
    }
    error
        .to_string()
        .split(':')
        .next()
        .unwrap_or_default()
        .chars()
        .take(80)
        .collect()
}

fn settlement_policy_refused(error: &DaemonError) -> bool {
    matches!(error, DaemonError::StructuredRpc { data, .. }
        if data["code"] == "storage_control_not_authorized"
            || data["code"] == "storage_control_not_in_scope"
            || data["code"] == "storage_control_automatic_settlement_not_due")
}

/// Called under the repository mutation lock. A journal phase alone cannot
/// distinguish a fresh intent from a crash after its first quarantine effect.
fn settlement_item_has_no_effect(
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
) -> Result<bool> {
    if item.phase != SourceWorktreeSettlementPhaseV1::IntentCommitted {
        return Ok(false);
    }
    #[cfg(test)]
    {
        let mut failures = SETTLEMENT_RECOVERY_PROBE_FAILURE.lock().unwrap();
        if failures.remove(&item.session_id) {
            return Err(DaemonError::Process(
                "injected Git recovery read failure".into(),
            ));
        }
    }
    let original = Path::new(&item.sandbox_root);
    let quarantine =
        git_worktree::derive_settlement_quarantine_path(original, run_id, item.session_id)?;
    Ok(nofollow_path_exists(original)?
        && !nofollow_path_exists(&quarantine)?
        && matches!(git_worktree::observe_direct_ref_locked(
            Path::new(&item.canonical_repo_dir), &item.source_ref,
        )?, git_worktree::DirectRefObservation::Commit(oid) if oid == item.source_oid)
        && git_worktree::source_ref_is_registered_only_at_locked(
            Path::new(&item.canonical_repo_dir),
            &item.source_ref,
            original,
            &item.source_oid,
        )?)
}

#[cfg(test)]
static SETTLEMENT_RECOVERY_PROBE_FAILURE: std::sync::LazyLock<std::sync::Mutex<HashSet<Uuid>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

/// Close every proven untouched retained intent even after an earlier root lost
/// initiating policy. The audit records revival before the legal Unattempted
/// transition; failed or contended reproof leaves the old fence intact.
#[allow(clippy::too_many_arguments)]
fn close_item_after_policy_stop_under_exclusion(
    _exclusion: &CwdExclusion,
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    completed: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::CompletedSession>>>,
    run_id: Uuid,
    sandbox_base: &Path,
    item: &SourceWorktreeSettlementJournalItem,
    replaying: bool,
) -> Result<bool> {
    if retained_intent_can_be_reproved(item) {
        match reprove_retained_intent_under_exclusion(
            store,
            active,
            completed,
            run_id,
            sandbox_base,
            item,
        ) {
            Ok(Some(reopened)) => {
                mark_unattempted_under_root(store, run_id, &reopened, true, true)?
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(run_id=%run_id, session_id=%item.session_id, error=%error,
                "Policy-stop reproof deferred; retaining this item and continuing the batch"),
        }
    } else {
        mark_unattempted_under_root(store, run_id, item, replaying, true)?;
    }
    Ok(false)
}

/// Closes `item` as `Unattempted` when its Session is paused or unfinished work
/// and no Git effect has started. Called under the repository mutation lock
/// with the store held; an item whose effects began is never touched here, so
/// recovery of a started removal is unchanged.
fn retain_resumable_before_effect(
    store: &crate::store::Store,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
) -> Result<bool> {
    if item.phase != SourceWorktreeSettlementPhaseV1::IntentCommitted
        || item.before_observation.is_some()
        || !matches!(
            item.original_status.as_str(),
            "Completed" | "Failed" | "Interrupted"
        )
    {
        return Ok(false);
    }
    let Some(hold) = store.automatic_reclaim_resume_hold(item.session_id)? else {
        return Ok(false);
    };
    if !settlement_item_has_no_effect(run_id, item)? {
        return Ok(false);
    }
    tracing::info!(
        run_id = %run_id,
        session_id = %item.session_id,
        hold,
        "Settlement retained a paused or unfinished Session before any effect"
    );
    store.mark_source_worktree_settlement_unattempted(
        run_id,
        item.session_id,
        "Session is paused or unfinished work awaiting resume; sandbox retained before any effect",
    )?;
    Ok(true)
}

fn mark_unattempted_under_root(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
    replaying: bool,
    policy_stopped: bool,
) -> Result<()> {
    let _root_guard =
        crate::store::custody_lock_order::lock_custody_root_under_repository(item.custody_id)?;
    let store = store.blocking_lock_checked()?;
    let no_effect = if !replaying {
        true
    } else if !policy_stopped {
        // A non-policy saga failure retains its existing recovery fence.
        false
    } else {
        match settlement_item_has_no_effect(run_id, item) {
            Ok(no_effect) => no_effect,
            Err(error) => {
                tracing::warn!(run_id=%run_id, session_id=%item.session_id, error=%error,
                    "Settlement stop probe failed; retaining the untouched root");
                settlement_intent_paths_untouched(run_id, item).unwrap_or(false)
            }
        }
    };
    match item.phase {
        SourceWorktreeSettlementPhaseV1::IntentCommitted if no_effect => {
            store.mark_source_worktree_settlement_unattempted(
                run_id,
                item.session_id,
                "no effect attempted after settlement authorization or an earlier item stopped the batch",
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

/// A failed Git read never grants removal authority. With the original root
/// intact, no quarantine and no effect-authority marker, close the untouched
/// intent. Otherwise keep an explicit recovery fence for inspection.
fn settlement_intent_paths_untouched(
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
) -> Result<bool> {
    let original = Path::new(&item.sandbox_root);
    let quarantine =
        git_worktree::derive_settlement_quarantine_path(original, run_id, item.session_id)?;
    Ok(
        item.phase == SourceWorktreeSettlementPhaseV1::IntentCommitted
            && item.before_observation.is_none()
            && nofollow_path_exists(original)?
            && !nofollow_path_exists(&quarantine)?,
    )
}

fn close_failed_settlement_probe(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    run_id: Uuid,
    item: &SourceWorktreeSettlementJournalItem,
) -> Result<()> {
    let _root_guard =
        crate::store::custody_lock_order::lock_custody_root_under_repository(item.custody_id)?;
    let store = store.blocking_lock_checked()?;
    if settlement_intent_paths_untouched(run_id, item).unwrap_or(false) {
        store.mark_source_worktree_settlement_unattempted(
            run_id,
            item.session_id,
            "Git recovery probe failed before any effect; original root retained",
        )
    } else {
        store.mark_source_worktree_settlement_recovery_required(
            run_id,
            item.session_id,
            item.phase.clone(),
            SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
            "recovery probe failed; prior effect cannot be excluded",
        )
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
    build_audit_with_target_fetch_locked(
        repository_identity,
        canonical_repo_dir,
        sandbox_base,
        inventory,
        active_ids,
        active_cwds,
        false,
    )
}

/// `fetch_target` refreshes `refs/remotes/origin/<branch>` first (bounded); a
/// failed fetch refuses the audit as `target_fetch_failed` and never falls back
/// to the local branch. Apply passes `false` so its recheck binds the same
/// fetched tip the operator's `plan_digest` named.
fn build_audit_with_target_fetch_locked(
    repository_identity: &str,
    canonical_repo_dir: &str,
    sandbox_base: &Path,
    inventory: Vec<SourceWorktreeInventoryRow>,
    active_ids: &HashSet<Uuid>,
    active_cwds: &[RuntimePathInput],
    fetch_target: bool,
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
    let fetch_failed =
        fetch_target && git_worktree::fetch_settlement_target_locked(origin).is_err();
    let target_result = if fetch_failed {
        Err(DaemonError::Process(
            "settlement target fetch failed".into(),
        ))
    } else {
        git_worktree::observe_settlement_target_locked(origin)
    };
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
        Err(_) if fetch_failed => (
            None,
            None,
            Some(
                "target_fetch_failed: fetching the origin target tip failed or timed out"
                    .to_string(),
            ),
        ),
        Err(_) => (
            None,
            None,
            Some(
                "target_unavailable: symbolic local HEAD or its fetched tip could not be resolved"
                    .to_string(),
            ),
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
    let mut target_note: Option<String> = None;
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

        // #2731: name the ref a nonancestor verdict was proved against and how
        // old it is, so a stale remote-tracking ref is told apart from work
        // that really has not landed. Computed once, only when needed.
        if disposition == SourceWorktreeDispositionV1::RetainedNonAncestor {
            if let (Some(target_ref), Some(target_oid)) =
                (target_ref.as_deref(), target_oid.as_deref())
            {
                let note = target_note.get_or_insert_with(|| {
                    describe_settlement_target(
                        &git_worktree::observe_settlement_target_freshness_locked(
                            origin, target_ref,
                        ),
                        target_oid,
                        fetch_target,
                    )
                });
                diagnostic = Some(format!("not an ancestor of {note}"));
            }
        }

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
        // Retained rows remain visible in the report, but are not authorized
        // for removal. Their runtime metadata must not stale an unchanged plan.
        // Eligibility transitions still change this exact set in either direction.
        eligible_items: &items
            .iter()
            .filter(|item| item.disposition.is_eligible())
            .collect::<Vec<_>>(),
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
        last_run_retained_roots: Vec::new(),
        refusal,
    }
    .validate_wire()
    .map_err(DaemonError::Store)?;
    Ok(AuditSnapshot { report, eligible })
}

/// Classifies one inventory row and enforces the wire invariant that an item
/// carrying a scheduled or Session path dependency is retained *as that
/// dependency*. The ordered checks in [`classify_row_by_precedence`] return early
/// (an active owner, a nonterminal status, unverified custody, ...) before they
/// reach the dependency checks, so a row with such a dependency can leave them
/// with a non-dependency retained disposition. The scan-bound overflow in the
/// store inventory is the broad case: it stamps `u64::MAX` on every root. Such a
/// row stays retained and gains its dependency reason; the displaced
/// classification is kept in the diagnostic. Eligible rows never carry a
/// dependency (the precedence checks return the dependency disposition first).
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

    let disposition = classify_row_by_precedence(
        origin,
        sandbox_base,
        canonical_base,
        row,
        timestamp_valid,
        session_id,
        active_ids,
        active_cwds,
        target_ref,
        target_oid,
        cohort_refused,
        source_ref,
        source_oid,
        clean_state_digest,
        proof,
        diagnostic,
        git_evidence,
    );
    // Preserve the precise decoder branch even when an earlier disposition
    // (active owner, foreign scope, etc.) takes precedence.
    if let Some(reason) = &row.session_path_dependency_reason {
        let ambiguity = format!("Session path dependency evidence is ambiguous [{reason}]");
        *diagnostic = Some(match diagnostic.take() {
            Some(existing) if ambiguity.len() + existing.len() + 2
                <= rsi_common::cohort_settlement::SOURCE_WORKTREE_SETTLEMENT_MAX_DIAGNOSTIC_BYTES => {
                format!("{ambiguity}; {existing}")
            }
            _ => ambiguity,
        });
    }
    let dependency = if row.scheduled_dependency_count != 0 {
        D::ScheduledDependency
    } else if row.session_path_dependency_count != 0 {
        D::SessionPathDependency
    } else {
        return disposition;
    };
    if matches!(
        disposition,
        D::ScheduledDependency | D::SessionPathDependency
    ) {
        return disposition;
    }
    // A dependency-bearing row is never eligible; the displaced reason stays
    // visible to the operator.
    let displaced = format!(
        "retained for its dependency; also {}",
        serde_json::to_string(&disposition)
            .map(|label| label.trim_matches('"').to_string())
            .unwrap_or_else(|_| "unclassified".into())
    );
    *diagnostic = Some(match diagnostic.take() {
        Some(existing) if existing.len() + displaced.len() + 2
                <= rsi_common::cohort_settlement::SOURCE_WORKTREE_SETTLEMENT_MAX_DIAGNOSTIC_BYTES => {
            format!("{existing}; {displaced}")
        }
        Some(existing) => existing,
        None => displaced,
    });
    *source_oid = None;
    *clean_state_digest = None;
    *proof = SourceWorktreeProofV1::None;
    dependency
}

/// A remote-tracking comparison ref whose tip has not changed for this long is
/// reported `stale_ref` (#2731): `rolling` moves several times an hour, so an
/// older tracking tip most likely predates landed work.
const STALE_TARGET_REF_SECS: u64 = 30 * 60;

/// `compared_ref=<ref>@<oid12> ref_updated_secs_ago=N tip_committed_secs_ago=M
/// fetched_this_pass=<bool>` plus `stale_ref` when a remote-tracking ref was not
/// refreshed by this pass and its tip is older than [`STALE_TARGET_REF_SECS`].
fn describe_settlement_target(
    freshness: &git_worktree::SettlementTargetFreshness,
    target_oid: &str,
    fetched_this_pass: bool,
) -> String {
    let age = |secs: Option<u64>| secs.map_or("unknown".to_string(), |secs| secs.to_string());
    let mut note = format!(
        "compared_ref={}@{} ref_updated_secs_ago={} tip_committed_secs_ago={} fetched_this_pass={fetched_this_pass}",
        freshness.compared_ref,
        &target_oid[..target_oid.len().min(12)],
        age(freshness.ref_updated_secs_ago),
        age(freshness.tip_committed_secs_ago),
    );
    let remote_tracking = freshness.compared_ref.starts_with("refs/remotes/");
    let old = freshness
        .ref_updated_secs_ago
        .is_some_and(|secs| secs > STALE_TARGET_REF_SECS);
    if remote_tracking && !fetched_this_pass && old {
        note.push_str(" stale_ref");
    }
    note
}

/// The first of the stored session/custody identity checks a root fails, as
/// `check=<field> stored=<..> observed=<..>` (#2731); `None` when the stored
/// session still describes this exact `Live` git worktree of `origin`.
fn session_identity_mismatch(row: &SourceWorktreeInventoryRow, origin: &Path) -> Option<String> {
    let origin = origin.to_string_lossy();
    let opt = |value: Option<&str>| value.unwrap_or("none").to_string();
    if row.repository_identity.is_empty() {
        return Some("check=repository_identity stored=empty".into());
    }
    if row.canonical_repo_dir != origin {
        return Some(format!(
            "check=canonical_repo_dir stored={} observed={origin}",
            row.canonical_repo_dir
        ));
    }
    if row.session_working_dir.as_deref() != Some(row.canonical_repo_dir.as_str()) {
        return Some(format!(
            "check=session_working_dir stored_session={} custody_repo_dir={}",
            opt(row.session_working_dir.as_deref()),
            row.canonical_repo_dir
        ));
    }
    if row.session_sandbox_kind.as_deref() != Some("GitWorktree") {
        return Some(format!(
            "check=session_sandbox_kind stored={} expected=GitWorktree",
            opt(row.session_sandbox_kind.as_deref())
        ));
    }
    if row.session_sandbox_root.as_deref() != Some(row.sandbox_root.as_str()) {
        return Some(format!(
            "check=session_sandbox_root stored_session={} custody_root={}",
            opt(row.session_sandbox_root.as_deref()),
            row.sandbox_root
        ));
    }
    if row.session_sandbox_branch.as_deref() != Some(row.sandbox_branch.as_str()) {
        return Some(format!(
            "check=session_sandbox_branch stored_session={} custody_branch={}",
            opt(row.session_sandbox_branch.as_deref()),
            row.sandbox_branch
        ));
    }
    if row.session_cleanup_state.as_deref() != Some("Live") {
        return Some(format!(
            "check=session_cleanup_state stored={} expected=Live",
            opt(row.session_cleanup_state.as_deref())
        ));
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn classify_row_by_precedence(
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

    if let Some(disposition) = &row.agent_scope_disposition {
        if let Some(detail) = &row.agent_scope_detail {
            *diagnostic = Some(detail.clone());
        }
        return disposition.clone();
    }
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
        *diagnostic = Some(format!(
            "check=updated_at_unparseable stored={}",
            row.session_updated_at.as_deref().unwrap_or("none")
        ));
        return D::CustodyUnverified;
    }
    if !matches!(
        row.status.as_deref(),
        Some("Completed" | "Failed" | "Interrupted" | "Archived" | "Deleted")
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
    // #247: a paused or unfinished worker is not finished work, whatever its
    // clean tree says; its Session may still be continued from this root.
    if let Some(hold) = row.resume_hold {
        *diagnostic = Some(format!(
            "Session is paused or unfinished work awaiting resume [{hold}]; sandbox retained"
        ));
        return D::NonterminalStatus;
    }
    if row.owner_session_id != Some(session_id) || row.session_id != Some(session_id) {
        let show = |id: Option<Uuid>| id.map_or("none".to_string(), |id| id.to_string());
        *diagnostic = Some(format!(
            "check=owner_session stored_owner={} stored_session_link={} observed={session_id}",
            show(row.owner_session_id),
            show(row.session_id)
        ));
        return D::CustodyUnverified;
    }
    if row.validation_state != "verified" {
        *diagnostic = Some(format!(
            "check=validation_state stored={} expected=verified generation={} validated_generation={}",
            row.validation_state, row.generation, row.validated_generation
        ));
        return D::CustodyUnverified;
    }
    if row.generation == 0 || row.validated_generation != row.generation {
        *diagnostic = Some(format!(
            "check=validated_generation stored_generation={} validated_generation={}",
            row.generation, row.validated_generation
        ));
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
            *diagnostic =
                Some("scheduled dependency evidence is ambiguous; candidate retained".into());
        }
        return D::ScheduledDependency;
    }
    if row.session_path_dependency_count != 0 {
        if row.session_path_dependency_count == u64::MAX {
            *diagnostic =
                Some("Session path dependency evidence is ambiguous; candidate retained".into());
        }
        return D::SessionPathDependency;
    }
    if let Some(detail) = session_identity_mismatch(row, origin) {
        *diagnostic = Some(detail);
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
    // Unrelated refs are compared immediately around the removal effect. Their
    // movement during normal worker activity does not invalidate this plan.
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

/// The settlement target tip a fence input records: the last fetched
/// `refs/remotes/origin/<branch>` tip (or the local branch for a repository
/// without an `origin`), exactly what the audit would compare ancestry against
/// after its own fetch. Reads the local ref only.
///
/// # Errors
/// The cohort's repository path is ambiguous, or the target cannot be resolved.
fn observed_settlement_target_tip(
    repository_identity: &str,
    inventory: &[SourceWorktreeInventoryRow],
) -> Result<String> {
    let canonical_repo_dir = unique_repository_dir(repository_identity, inventory)?;
    let origin = PathBuf::from(canonical_repo_dir);
    git_worktree::with_repository_mutation(&origin, || {
        git_worktree::observe_settlement_target_locked(&origin)
    })
    .map(|target| target.target_oid)
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
        eligible_items: &[],
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
        last_run_retained_roots: receipt.retained_roots.clone(),
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
        eligible_items: &[],
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
            last_run_retained_roots: Vec::new(),
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

    /// Runtime for fixtures that build a live `SessionManager` and then drive
    /// settlement effects. Paused current-thread time keeps the manager's
    /// background workflow watcher (a periodic Store reader) from ticking
    /// while settlement's `spawn_blocking` work waits on the Store, so a
    /// happy-path count never races a reader (#1972). Production Store
    /// contention handling is untouched; its own test holds the Store.
    fn paused_settlement_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("paused settlement runtime")
    }

    /// Let the watcher spawned by `SessionManager::new` finish its immediate
    /// first tick before any settlement effect begins.
    async fn drain_background_startup_tick() {
        tokio::task::yield_now().await;
    }

    /// What a provider launch holds in production: the shared side of the lock
    /// settlement takes exclusively. A test that checks settlement against a
    /// launch waits on this one, so neither depends on another test's lock
    /// (see [`test_cwd_admission`]).
    async fn test_launch_cwd_admission() -> tokio::sync::RwLockReadGuard<'static, ()> {
        test_cwd_admission().read().await
    }

    fn create_private_sandbox_base(path: &Path) {
        std::fs::create_dir_all(path).expect("sandbox base directory");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base permissions");
    }

    /// Re-executed by `startup_owned_sleep` on macOS as the stamped child.
    #[cfg(target_os = "macos")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn startup_owned_sleep_child_helper() {
        if std::env::var_os("RSI_TEST_STARTUP_OWNED_SLEEP").is_none() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(300));
    }

    fn startup_owned_sleep(session_id: Uuid) -> crate::session::reaper::StartupReaperChild {
        // macOS withholds a platform binary's environment (`/bin/sleep`), so
        // the stamped child there is this ad-hoc-signed test binary, parked.
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut command = Command::new(std::env::current_exe().expect("test binary"));
            command
                .args([
                    "--exact",
                    "session::cohort_settlement::tests::startup_owned_sleep_child_helper",
                    "--test-threads=1",
                ])
                .env("RSI_TEST_STARTUP_OWNED_SLEEP", "1")
                .stdout(std::process::Stdio::null());
            command
        };
        #[cfg(not(target_os = "macos"))]
        let mut command = {
            let mut command = Command::new("sleep");
            // Fixed-point aggregate tests can outlive the former 30-second child;
            // StartupReaperChild still guarantees kill-and-wait cleanup on every path.
            command.arg("300");
            command
        };
        command
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
        #[cfg(target_os = "linux")]
        _fixture_holder_registration: crate::session::reaper::FixtureHolderTestProcGuard,
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
            let directory = crate::test_support::disk_backed_tempdir(label);
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
            let mut session = rsid_store::test_support::make_test_session();
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
            #[cfg(target_os = "linux")]
            let fixture_holder_registration =
                crate::session::reaper::register_fixture_holder_test_proc(
                    directory.path(),
                    &quarantine_holder_proc,
                );
            Self {
                _directory: directory,
                quarantine_holder_proc,
                #[cfg(target_os = "linux")]
                _fixture_holder_registration: fixture_holder_registration,
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
            let mut session = rsid_store::test_support::make_test_session();
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

        /// The stored dependency-health relevance of one Session (0 proven
        /// irrelevant, 1 relevant, 2 invalid), as the V175 classifier writes it.
        fn dependency_health(&self, session_id: Uuid) -> i64 {
            self.store
                .conn
                .query_row(
                    "SELECT relevance FROM source_worktree_session_dependency_health
                      WHERE session_id=?1",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .expect("read Session dependency health")
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
                retained: Vec::new(),
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

        /// A second Live custody-backed root of the same repository, integrated
        /// like the first, so one settlement run carries two items.
        fn add_second_root(&mut self, label: &str) {
            self.add_second_root_with_custody(label, Uuid::new_v4());
        }

        /// Like `add_second_root`, but the second custody id is drawn so its
        /// custody-root lock stripe differs from the primary root's. A test that
        /// holds one root's stripe then contends only that root (Issue #606).
        fn add_second_root_in_other_stripe(&mut self, label: &str) {
            let primary_shard =
                crate::store::sandbox_custody::custody_root_lock_shard(self.custody_id);
            let custody_id = std::iter::repeat_with(Uuid::new_v4)
                .find(|id| {
                    crate::store::sandbox_custody::custody_root_lock_shard(*id) != primary_shard
                })
                .expect("a custody id in a different lock stripe");
            self.add_second_root_with_custody(label, custody_id);
        }

        fn add_second_root_with_custody(&mut self, label: &str, custody_id: Uuid) {
            let session_id = Uuid::new_v4();
            let branch = format!("rsi/{label}/{session_id}");
            let allocation = git_worktree::allocate(
                &self.sandbox_base,
                session_id,
                &self.repository,
                &self.source_oid,
                Some(&branch),
            )
            .expect("allocate second candidate worktree");
            let mut session = rsid_store::test_support::make_test_session();
            session.id = session_id;
            session.status = SessionStatus::Completed;
            session.working_dir = self.target.canonical_repo_dir.clone();
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(allocation.root.clone());
            session.sandbox_branch = Some(branch.clone());
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            self.store
                .insert_session_with_custody(
                    &session,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id,
                        canonical_repo_dir: self
                            .target
                            .canonical_repo_dir
                            .to_string_lossy()
                            .into_owned(),
                        sandbox_root: allocation.root.to_string_lossy().into_owned(),
                        sandbox_branch: branch,
                        repository_identity: self.target.repository_identity.clone(),
                        source_commit: self.source_oid.clone(),
                        cause: CustodyCause::FreshLaunch,
                    }),
                )
                .expect("seed second custody-backed terminal Session");
        }

        /// Commit one run whose items are every eligible root in the audit.
        fn commit_intent_for_all_eligible(&mut self, key: &str) -> NewSettlementRun {
            self.commit_intent_for_all_eligible_as(
                key,
                &rsi_common::cohort_settlement::SourceWorktreeSettlementAttributionV1 {
                    origin:
                        rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Operator,
                    agent_session_id: None,
                },
            )
        }

        fn commit_intent_for_all_eligible_as(
            &mut self,
            key: &str,
            attribution: &rsi_common::cohort_settlement::SourceWorktreeSettlementAttributionV1,
        ) -> NewSettlementRun {
            self.commit_intent_for_all_eligible_with_params(key, attribution)
                .0
        }

        /// Commit one run over every eligible root, with the request
        /// fingerprint the real apply path derives, and return the parameters
        /// an RPC replay of that request would carry.
        fn commit_intent_for_all_eligible_with_params(
            &mut self,
            key: &str,
            attribution: &rsi_common::cohort_settlement::SourceWorktreeSettlementAttributionV1,
        ) -> (NewSettlementRun, ApplySourceWorktreeCohortParams) {
            let audit = self.audit();
            let mut run = self.run_from_audit(&audit, key);
            let params = ApplySourceWorktreeCohortParams {
                repository_identity: self.target.repository_identity.clone(),
                plan_digest: audit.report.plan_digest.clone(),
                authorization: audit
                    .report
                    .authorization_phrase
                    .clone()
                    .expect("fixture authorization phrase"),
                idempotency_key: key.into(),
            };
            let authorization_digest = hash_bytes(params.authorization.as_bytes());
            run.request_fingerprint = hash_canonical(&CanonicalApplyFingerprint {
                schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                repository_identity: &params.repository_identity,
                plan_digest: params.plan_digest.as_str(),
                authorization_digest: authorization_digest.as_str(),
                idempotency_key: &params.idempotency_key,
            })
            .expect("fixture request fingerprint")
            .as_str()
            .to_string();
            run.items = audit
                .eligible
                .iter()
                .map(|eligible| NewSettlementItem {
                    session_id: eligible.inventory.session_id.expect("eligible session"),
                    custody_id: eligible.inventory.custody_id,
                    original_status: eligible.inventory.status.clone().expect("status"),
                    original_updated_at: eligible
                        .inventory
                        .session_updated_at
                        .clone()
                        .expect("updated_at"),
                    custody_generation: eligible.inventory.generation,
                    canonical_repo_dir: eligible.inventory.canonical_repo_dir.clone(),
                    sandbox_root: eligible.inventory.sandbox_root.clone(),
                    sandbox_branch: eligible.inventory.sandbox_branch.clone(),
                    repository_identity: eligible.inventory.repository_identity.clone(),
                    source_ref: eligible.source_ref.clone(),
                    source_oid: eligible.source_oid.clone(),
                    target_oid: run.items[0].target_oid.clone(),
                    evidence_digest: eligible.evidence_digest.clone(),
                    clean_state_digest: eligible.clean_state_digest.clone(),
                    reserved_effects: eligible.inventory.reserved_effects,
                    active_effects: eligible.inventory.active_effects,
                    participant_count: eligible.inventory.participant_count,
                })
                .collect();
            assert!(run.items.len() >= 2, "fixture carries multiple items");
            assert!(matches!(
                self.store
                    .insert_source_worktree_settlement_run_as(&run, attribution)
                    .expect("commit multi-item settlement intent"),
                InsertSettlementRunOutcome::Inserted
            ));
            (run, params)
        }

        /// Two runs over disjoint roots of one repository, inserted in order:
        /// the older run over the primary root, then the newer run over the
        /// second root, so the newer one is the latest receipt (#2663). Both
        /// are built from one audit.
        fn commit_two_disjoint_runs(&mut self) -> (NewSettlementRun, NewSettlementRun) {
            self.add_second_root("disjoint-2663");
            let audit = self.audit();
            assert_eq!(audit.eligible.len(), 2, "two eligible disjoint roots");
            let mut older = self.run_from_audit(&audit, "fixture:disjoint-older-2663");
            let mut newer = self.run_from_audit(&audit, "fixture:disjoint-newer-2663");
            let target_oid = older.items[0].target_oid.clone();
            let item_for = |eligible: &EligibleSettlementItem| NewSettlementItem {
                session_id: eligible.inventory.session_id.expect("eligible session"),
                custody_id: eligible.inventory.custody_id,
                original_status: eligible.inventory.status.clone().expect("status"),
                original_updated_at: eligible
                    .inventory
                    .session_updated_at
                    .clone()
                    .expect("updated_at"),
                custody_generation: eligible.inventory.generation,
                canonical_repo_dir: eligible.inventory.canonical_repo_dir.clone(),
                sandbox_root: eligible.inventory.sandbox_root.clone(),
                sandbox_branch: eligible.inventory.sandbox_branch.clone(),
                repository_identity: eligible.inventory.repository_identity.clone(),
                source_ref: eligible.source_ref.clone(),
                source_oid: eligible.source_oid.clone(),
                target_oid: target_oid.clone(),
                evidence_digest: eligible.evidence_digest.clone(),
                clean_state_digest: eligible.clean_state_digest.clone(),
                reserved_effects: eligible.inventory.reserved_effects,
                active_effects: eligible.inventory.active_effects,
                participant_count: eligible.inventory.participant_count,
            };
            let (primary, other): (Vec<_>, Vec<_>) = audit
                .eligible
                .iter()
                .partition(|eligible| eligible.inventory.custody_id == self.custody_id);
            assert_eq!((primary.len(), other.len()), (1, 1), "two disjoint roots");
            older.items = vec![item_for(primary[0])];
            newer.items = vec![item_for(other[0])];
            for run in [&mut older, &mut newer] {
                // One root each: the bounded counts describe this run's item.
                run.observed_count = 1;
                run.retained_count = 0;
            }
            for run in [&older, &newer] {
                assert!(matches!(
                    self.store
                        .insert_source_worktree_settlement_run(run)
                        .expect("commit disjoint settlement intent"),
                    InsertSettlementRunOutcome::Inserted
                ));
            }
            (older, newer)
        }

        /// A manager that owns this fixture's Store.
        fn into_manager(&mut self) -> SessionManager {
            SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                std::mem::replace(
                    &mut self.store,
                    crate::store::Store::open_in_memory().expect("replacement fixture Store"),
                ),
                false,
                self._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
                self.sandbox_base.clone(),
            )
            .expect("construct manager")
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
            self.replay_run_with(run_id, false)
        }

        /// #273: replay while this thread holds the replay's private cwd
        /// admission, so each root's bounded in-mutex wait expires as it does
        /// behind a long provider launch.
        #[cfg(target_os = "linux")]
        fn replay_run_with_cwd_admission_held(
            &mut self,
            run_id: Uuid,
        ) -> (ApplyOutcome, Arc<tokio::sync::Mutex<crate::store::Store>>) {
            self.replay_run_with(run_id, true)
        }

        /// #270: the replay settles under a provider-cwd admission of its own,
        /// so a co-tenant test holding the daemon-wide barrier cannot make it
        /// surrender.
        fn replay_run_with(
            &mut self,
            run_id: Uuid,
            hold_cwd_admission: bool,
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
            let outcome = with_fixture_cwd_admission(|| {
                let _held = hold_cwd_admission.then(|| {
                    wait_for_settlement_lock(acquire_settlement_cwd_exclusion())
                        .expect("the replay's private cwd admission is free")
                });
                self.with_quarantine_holder_test_proc(|| {
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
            })
            .expect("replay fixture settlement");
            (outcome, store)
        }

        fn commit_intent_for_all_eligible_rpc(
            &mut self,
            key: &str,
        ) -> (NewSettlementRun, ApplySourceWorktreeCohortParams) {
            self.commit_intent_for_all_eligible_with_params(
                key,
                &rsi_common::cohort_settlement::SourceWorktreeSettlementAttributionV1 {
                    origin:
                        rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Operator,
                    agent_session_id: None,
                },
            )
        }

        /// Move one named root of a multi-item run into its quarantine.
        fn move_item_to_quarantine(&self, run: &NewSettlementRun, session_id: Uuid) -> PathBuf {
            let item = run
                .items
                .iter()
                .find(|item| item.session_id == session_id)
                .expect("run item for the session");
            git_worktree::with_repository_mutation(&self.repository, || {
                let proof = git_worktree::prepare_settlement_quarantine_path(
                    Path::new(&item.sandbox_root),
                    run.run_id,
                    session_id,
                )?;
                git_worktree::move_worktree_to_quarantine_non_force_locked(
                    &self.repository,
                    &proof,
                    &item.source_ref,
                    &item.source_oid,
                )?;
                Ok(proof.quarantine_root)
            })
            .expect("move named fixture root to quarantine")
        }

        /// A fully terminal run: one root settled, one refused, and the
        /// fixture's primary root fenced by an old first-proof fence with its
        /// quarantine still on disk. Only a reopening pass can finish it.
        fn commit_mixed_terminal_run_with_first_proof_fence(
            &mut self,
            key: &str,
        ) -> MixedTerminalRun {
            // The key is an idempotency key; root labels become branch names.
            let label = key.replace(':', "-");
            self.add_second_root(&format!("{label}-settled"));
            self.add_second_root(&format!("{label}-refused"));
            let (run, params) = self.commit_intent_for_all_eligible_rpc(key);
            let journal = self
                .store
                .list_source_worktree_settlement_journal_items(run.run_id)
                .expect("mixed run journal");
            let others = journal
                .iter()
                .filter(|item| item.session_id != self.session_id)
                .cloned()
                .collect::<Vec<_>>();
            let (settled, refused) = (others[0].clone(), others[1].clone());

            let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
                &mut self.store,
                crate::store::Store::open_in_memory().expect("replacement fixture Store"),
            )));
            let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            self.with_quarantine_holder_test_proc(|| {
                git_worktree::with_repository_mutation(&self.repository, || {
                    settle_one_item_locked(
                        &store,
                        &active,
                        &completed,
                        run.run_id,
                        &run.target_ref,
                        &self.repository,
                        &self.sandbox_base,
                        &settled,
                        false,
                    )
                })
            })
            .expect("settle the first mixed-run root");
            self.store = Arc::try_unwrap(store)
                .ok()
                .expect("sole fixture Store owner")
                .into_inner();
            self.store
                .mark_source_worktree_settlement_refused(
                    run.run_id,
                    refused.session_id,
                    SourceWorktreeSettlementPhaseV1::IntentCommitted,
                    SourceWorktreeSettlementRefusalV1::RuntimeStateContended.as_str(),
                    "active runtime map was contended before effect",
                )
                .expect("refuse the second mixed-run root");
            let quarantine = self.move_item_to_quarantine(&run, self.session_id);
            self.store
                .mark_source_worktree_settlement_recovery_required(
                    run.run_id,
                    self.session_id,
                    SourceWorktreeSettlementPhaseV1::IntentCommitted,
                    "recovery_proof_failed",
                    "quarantined worktree failed its removal-authority proof",
                )
                .expect("fence the primary root with an old first-proof fence");
            let fenced = self
                .store
                .get_source_worktree_settlement_run(run.run_id)
                .expect("read the fenced mixed run")
                .expect("fenced mixed run");
            assert!(fenced.items.iter().all(|item| item.phase.is_terminal()));
            assert_eq!(fenced.counts.settled, 1, "{fenced:?}");
            assert_eq!(fenced.counts.refused, 1);
            assert_eq!(fenced.counts.recovery_required, 1);
            assert!(fenced.finished_at.is_some());
            assert_eq!(
                effect_capable_replay_session_ids(&fenced),
                vec![self.session_id],
                "only the first-proof fence is revivable"
            );
            MixedTerminalRun {
                run,
                params,
                quarantine,
                fenced,
            }
        }
    }

    struct MixedTerminalRun {
        run: NewSettlementRun,
        params: ApplySourceWorktreeCohortParams,
        quarantine: PathBuf,
        fenced: SourceWorktreeSettlementRunV1,
    }

    fn retained_intent_receipt_item(
        before_observation: Option<&str>,
    ) -> rsi_common::cohort_settlement::SourceWorktreeSettlementItemV1 {
        rsi_common::cohort_settlement::SourceWorktreeSettlementItemV1 {
            sequence: 0,
            session_id: Uuid::new_v4(),
            custody_id: Uuid::new_v4(),
            source_ref: "refs/heads/rsi/retained-intent".into(),
            expected_source_oid: SourceWorktreeGitOidV1::parse("1".repeat(40))
                .expect("fixture oid"),
            phase: SourceWorktreeSettlementPhaseV1::RecoveryRequired,
            refusal_code: Some(SourceWorktreeSettlementRefusalV1::RecoveryProofFailed),
            before_observation: before_observation.map(str::to_owned),
            after_observation: Some(
                "retained intent followed an earlier terminal stop and cannot prove no prior effect"
                    .into(),
            ),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn retained_intent_fence_is_revivable_only_when_untouched() {
        let untouched = retained_intent_receipt_item(None);
        assert!(item_has_revivable_fence(&untouched));

        // A quarantine move wrote `before_observation`, so an effect started:
        // the store will not reopen it and replay must return the stored receipt.
        let effect_started = retained_intent_receipt_item(Some("quarantine authority marker"));
        assert!(!item_has_revivable_fence(&effect_started));

        // The other revivable shapes are unchanged.
        let mut first_proof = retained_intent_receipt_item(Some("quarantine authority marker"));
        first_proof.after_observation =
            Some("quarantined worktree failed its removal-authority proof".into());
        assert!(item_has_revivable_fence(&first_proof));
        let mut ref_delete = retained_intent_receipt_item(Some("quarantine authority marker"));
        ref_delete.refusal_code = Some(SourceWorktreeSettlementRefusalV1::RefDeleteFailed);
        ref_delete.after_observation = None;
        assert!(item_has_revivable_fence(&ref_delete));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    /// #2622: an audit that cannot take the repository inside its wait answers
    /// a typed `source_settlement_busy` naming the unfinished run, writes
    /// nothing and returns no plan; the same audit succeeds once the holder
    /// releases.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn audit_behind_a_held_repository_answers_busy_naming_the_holder_run() {
        let mut fixture = IntegratedFixture::new("audit-busy-2622");
        let run = fixture.commit_intent("fixture:audit-busy-2622");
        let repository = fixture.repository.clone();
        let repository_identity = fixture.target.repository_identity.clone();
        let sandbox_base = fixture.sandbox_base.clone();
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            std::mem::replace(
                &mut fixture.store,
                crate::store::Store::open_in_memory().expect("replacement fixture Store"),
            ),
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
            sandbox_base,
        )
        .expect("construct manager");

        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder_run_id = run.run_id;
        let holder = std::thread::spawn(move || {
            git_worktree::with_repository_mutation_as(
                &repository,
                git_worktree::RepositoryHolder::SettlementRun(holder_run_id),
                || {
                    held_tx.send(()).unwrap();
                    // Holds the repository longer than the audit's wait.
                    release_rx.recv().unwrap();
                    Ok(())
                },
            )
        });
        held_rx.recv().unwrap();

        let started = std::time::Instant::now();
        let error = manager
            .audit_source_worktree_cohort_as(
                repository_identity.clone(),
                None,
                Some(std::time::Duration::from_millis(150)),
            )
            .await
            .expect_err("a held repository must not be audited");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let DaemonError::StructuredRpc { data, .. } = error else {
            panic!("expected a typed busy refusal, got {error:?}");
        };
        assert_eq!(data["code"], "source_settlement_busy");
        assert_eq!(data["disposition"], "busy");
        assert_eq!(data["holder_run_id"], run.run_id.to_string());
        assert_eq!(data["holder_known"], true);
        assert_eq!(data["writes"], 0);
        assert!(data["retry_after_secs"].as_u64().unwrap() > 0);
        assert!(data.get("plan_digest").is_none());
        assert!(data.get("authorization_phrase").is_none());
        assert!(
            AGENT_AUDIT_REPOSITORY_WAIT < std::time::Duration::from_secs(30),
            "the agent audit must answer inside the 30 s RPC deadline"
        );

        release_tx.send(()).unwrap();
        holder.join().unwrap().unwrap();
        let audit = manager
            .audit_source_worktree_cohort_as(
                repository_identity,
                None,
                Some(std::time::Duration::from_secs(5)),
            )
            .await
            .expect("the audit answers once the holder released");
        assert_eq!(audit.run_id, Some(run.run_id));
    }

    /// Hold the repository as `holder` on a thread, run the agent audit with a
    /// short wait and return the typed busy answer's data.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    async fn busy_data_while_held_by(
        manager: &SessionManager,
        repository: PathBuf,
        repository_identity: String,
        holder: git_worktree::RepositoryHolder,
    ) -> serde_json::Value {
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            git_worktree::with_repository_mutation_as(&repository, holder, || {
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        held_rx.recv().unwrap();
        let error = manager
            .audit_source_worktree_cohort_as(
                repository_identity,
                None,
                Some(std::time::Duration::from_millis(150)),
            )
            .await
            .expect_err("a held repository must not be audited");
        release_tx.send(()).unwrap();
        thread.join().unwrap().unwrap();
        let DaemonError::StructuredRpc { data, .. } = error else {
            panic!("expected a typed busy refusal, got {error:?}");
        };
        assert_eq!(data["code"], "source_settlement_busy");
        assert_eq!(data["writes"], 0);
        data
    }

    /// #2663: the busy answer names the run that holds the repository, not the
    /// latest receipt. The older run (recovery or replay) holds the repository
    /// while a newer run over a disjoint root is the latest unfinished receipt.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn busy_names_the_older_holder_run_while_a_newer_run_is_latest() {
        let mut fixture = IntegratedFixture::new("audit-busy-older-2663");
        let (older, newer) = fixture.commit_two_disjoint_runs();
        let repository_identity = fixture.target.repository_identity.clone();
        let latest = fixture
            .store
            .latest_source_worktree_settlement_run(&repository_identity)
            .unwrap()
            .expect("latest receipt");
        assert_eq!(latest.run_id, newer.run_id, "the newer run is the latest");
        assert!(latest.finished_at.is_none());
        let older_created_at = fixture
            .store
            .get_source_worktree_settlement_run(older.run_id)
            .unwrap()
            .expect("older run")
            .created_at;
        let repository = fixture.repository.clone();
        let manager = fixture.into_manager();

        let data = busy_data_while_held_by(
            &manager,
            repository,
            repository_identity,
            git_worktree::RepositoryHolder::SettlementRun(older.run_id),
        )
        .await;
        assert_eq!(data["holder_run_id"], older.run_id.to_string());
        assert_ne!(data["holder_run_id"], newer.run_id.to_string());
        assert_eq!(data["holder_operation"], "settlement_run");
        assert_eq!(data["holder_known"], true);
        assert_eq!(data["holder_started_at"], older_created_at);
    }

    /// #2663: a finished latest receipt neither hides the run that really holds
    /// the repository nor is named as the holder.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn busy_names_the_holder_run_when_the_latest_receipt_is_finished() {
        let mut fixture = IntegratedFixture::new("audit-busy-finished-2663");
        let (older, newer) = fixture.commit_two_disjoint_runs();
        let newer_item = fixture
            .store
            .list_source_worktree_settlement_journal_items(newer.run_id)
            .unwrap()
            .into_iter()
            .next()
            .expect("newer run item");
        fixture
            .store
            .mark_source_worktree_settlement_refused(
                newer.run_id,
                newer_item.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementRefusalV1::RuntimeStateContended.as_str(),
                "finish the newer run without an effect",
            )
            .expect("finish the newer run");
        let repository_identity = fixture.target.repository_identity.clone();
        let latest = fixture
            .store
            .latest_source_worktree_settlement_run(&repository_identity)
            .unwrap()
            .expect("latest receipt");
        assert_eq!(latest.run_id, newer.run_id);
        assert!(
            latest.finished_at.is_some(),
            "the latest receipt is finished"
        );
        let repository = fixture.repository.clone();
        let manager = fixture.into_manager();

        let data = busy_data_while_held_by(
            &manager,
            repository,
            repository_identity,
            git_worktree::RepositoryHolder::SettlementRun(older.run_id),
        )
        .await;
        assert_eq!(data["holder_run_id"], older.run_id.to_string());
        assert_eq!(data["holder_known"], true);
    }

    /// #2663: a Git operation that is not a settlement saga holds the
    /// repository while an unfinished (inactive) run is the latest receipt: the
    /// answer names the operation and no run.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn busy_does_not_attribute_a_non_settlement_holder_to_the_latest_run() {
        let mut fixture = IntegratedFixture::new("audit-busy-operation-2663");
        let run = fixture.commit_intent("fixture:audit-busy-operation-2663");
        let repository_identity = fixture.target.repository_identity.clone();
        let latest = fixture
            .store
            .latest_source_worktree_settlement_run(&repository_identity)
            .unwrap()
            .expect("latest receipt");
        assert_eq!(latest.run_id, run.run_id);
        assert!(latest.finished_at.is_none(), "the latest run is unfinished");
        let repository = fixture.repository.clone();
        let manager = fixture.into_manager();

        for holder in [
            git_worktree::RepositoryHolder::Operation("sandbox_purge"),
            git_worktree::RepositoryHolder::Operation("git_operation"),
        ] {
            let label = match &holder {
                git_worktree::RepositoryHolder::Operation(label) => *label,
                git_worktree::RepositoryHolder::SettlementRun(_) => unreachable!(),
            };
            let data = busy_data_while_held_by(
                &manager,
                repository.clone(),
                repository_identity.clone(),
                holder,
            )
            .await;
            assert!(data["holder_run_id"].is_null(), "{data}");
            assert_eq!(data["holder_operation"], label);
            assert_eq!(data["holder_known"], true);
            assert!(data["holder_started_at"].is_null());
        }
    }

    /// #2662: the busy answer must not wait on the Store without a bound. The
    /// audit passes its initial Store read, the repository stays held past the
    /// wait, and the Store is held by another task while the busy answer is
    /// built: the audit still answers typed busy, naming the holder the guard
    /// recorded but without the run's Store detail, with no plan and no writes,
    /// well inside the RPC deadline.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn busy_answer_is_bounded_when_the_store_is_contended() {
        let mut fixture = IntegratedFixture::new("audit-busy-store-2662");
        let run = fixture.commit_intent("fixture:audit-busy-store-2662");
        let repository = fixture.repository.clone();
        let repository_identity = fixture.target.repository_identity.clone();
        let sandbox_base = fixture.sandbox_base.clone();
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            std::mem::replace(
                &mut fixture.store,
                crate::store::Store::open_in_memory().expect("replacement fixture Store"),
            ),
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
            sandbox_base,
        )
        .expect("construct manager");

        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder_run_id = run.run_id;
        let holder = std::thread::spawn(move || {
            git_worktree::with_repository_mutation_as(
                &repository,
                git_worktree::RepositoryHolder::SettlementRun(holder_run_id),
                || {
                    held_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                },
            )
        });
        held_rx.recv().unwrap();

        // Contend the Store after the audit's initial read (which runs at once)
        // and keep it contended until the audit has answered.
        let store = Arc::clone(&manager.store);
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel::<()>();
        let (store_release_tx, store_release_rx) = tokio::sync::oneshot::channel::<()>();
        let store_holder = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let guard = store.lock().await;
            locked_tx.send(()).unwrap();
            let _ = store_release_rx.await;
            drop(guard);
        });

        let started = std::time::Instant::now();
        let error = manager
            .audit_source_worktree_cohort_as(
                repository_identity,
                None,
                Some(std::time::Duration::from_millis(1200)),
            )
            .await
            .expect_err("a held repository must not be audited");
        let elapsed = started.elapsed();
        assert!(
            locked_rx.await.is_ok(),
            "the Store was contended during the busy answer"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "busy answer took {elapsed:?} behind a contended Store"
        );
        let DaemonError::StructuredRpc { data, .. } = error else {
            panic!("expected a typed busy refusal, got {error:?}");
        };
        assert_eq!(data["code"], "source_settlement_busy");
        assert_eq!(data["disposition"], "busy");
        // The guard names the holder without the Store; only the run's detail
        // (origin, start, counts) is dropped when the Store stays contended.
        assert_eq!(data["holder_known"], true);
        assert_eq!(data["holder_run_id"], run.run_id.to_string());
        assert!(data["holder_origin"].is_null());
        assert!(data["holder_started_at"].is_null());
        assert_eq!(data["writes"], 0);
        assert!(data.get("plan_digest").is_none());
        assert!(data.get("authorization_phrase").is_none());

        store_release_tx.send(()).unwrap();
        store_holder.await.unwrap();
        release_tx.send(()).unwrap();
        holder.join().unwrap().unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn scheduled_dependency_query_finds_matching_tail_after_16384_unrelated_jobs() {
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
            for index in 1..=16_385_u128 {
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn scheduled_dependency_query_ignores_more_than_16384_unrelated_jobs() {
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
            for index in 1..=16_385 {
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
        assert!(audit.report.applyable);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        );
        assert_eq!(audit.report.items[0].scheduled_dependency_count, 0);
        let run = fixture.commit_intent("history-independent-settlement");
        let (outcome, store) = fixture.replay_run(run.run_id);
        assert_eq!(outcome.receipt.counts.settled, 1, "{:#?}", outcome.receipt);
        assert_eq!(
            store
                .blocking_lock()
                .get_session(fixture.session_id)
                .expect("read settled owner")
                .expect("settled owner remains in history")
                .sandbox_cleanup_state,
            Some(SandboxCleanupState::Purged)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
        let mut alias = rsid_store::test_support::make_test_session();
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
            "a near-miss inert projection tuple must fail the dependency proof closed"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
        let mut alias = rsid_store::test_support::make_test_session();
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn terminal_cleanup_failed_and_quarantined_projections_are_inert_dependencies() {
        use rsi_common::types::SandboxCustodyErrorCodeV1;

        let mut fixture = IntegratedFixture::new("terminal-inert-projections");
        let inside = fixture.allocation.root.join("historical-provider-cwd");
        std::fs::create_dir_all(&inside).expect("create historical provider cwd");

        let mut rootless_failed = rsid_store::test_support::make_test_session();
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

        let mut rootless_purged = rsid_store::test_support::make_test_session();
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
            "malformed recognized state must fail the dependency proof closed"
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
            "malformed quarantined state must fail the dependency proof closed"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn purged_missing_root_history_is_inert_but_custody_contradictions_retain() {
        let mut fixture = IntegratedFixture::new("purged-root-missing-history");
        let historical = fixture.insert_synthetic_custody(
            "purged-history",
            "repo:purged-history".into(),
            fixture.allocation.root.to_string_lossy().into_owned(),
        );
        let custody: String = fixture
            .store
            .conn
            .query_row(
                "SELECT sandbox_custody_id FROM sessions WHERE id=?1",
                [historical.to_string()],
                |row| row.get(0),
            )
            .expect("historical custody");
        fixture
            .store
            .tombstone_custody_root(
                Uuid::parse_str(&custody).expect("custody UUID"),
                1,
                CustodyCause::StartupReconciliation,
            )
            .expect("purge historical root");
        // Real hub shape: startup publishes verified historical_purged while
        // preserving the aggregate's old root_missing filesystem observation.
        fixture.store.conn.execute(
            "UPDATE sandbox_custody_roots SET validation_state='invalid',validation_error_code='root_missing' WHERE custody_id=?1",
            [&custody],
        ).expect("retain missing-root observation");
        fixture
            .store
            .publish_startup_terminal_root(Uuid::parse_str(&custody).expect("custody UUID"))
            .expect("publish terminal projection");
        let audit = fixture.audit();
        assert!(audit.report.applyable);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        );
        assert_eq!(audit.report.items[0].session_path_dependency_count, 0);
        // Publisher-to-consumer parity (#2050): the settlement decoder treats
        // this publisher shape as inert, so the health index must too.
        assert_eq!(
            fixture.dependency_health(historical),
            0,
            "a purged root retaining root_missing is settled history"
        );

        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET status='Running' WHERE id=?1",
                [historical.to_string()],
            )
            .expect("activate historical path claimant");
        let active = fixture.audit();
        assert!(!active.report.applyable);
        assert_eq!(
            active.report.items[0].disposition,
            SourceWorktreeDispositionV1::SessionPathDependency
        );
        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id=?1",
                [historical.to_string()],
            )
            .expect("restore terminal history");

        for mutation in [
            "UPDATE sandbox_custody_roots SET validation_error_code='ownership_conflict' WHERE custody_id=?1",
            "UPDATE session_execution_projections SET custody_generation=custody_generation+1 WHERE custody_id=?1",
        ] {
            fixture
                .store
                .conn
                .execute(mutation, [&custody])
                .expect("contradict terminal custody");
            let audit = fixture.audit();
            assert!(!audit.report.applyable);
            assert_eq!(
                audit.report.items[0].session_path_dependency_count,
                u32::MAX
            );
            assert!(
                audit.report.items[0]
                    .diagnostic
                    .as_deref()
                    .unwrap()
                    .contains("historical_purged_custody_mismatch")
            );
            assert_eq!(
                fixture.dependency_health(historical),
                2,
                "a custody contradiction stays unhealthy for the health index"
            );
            fixture.store.conn.execute(
                "UPDATE sandbox_custody_roots SET validation_error_code='root_missing',validated_generation=generation,owner_session_id=NULL WHERE custody_id=?1",
                [&custody],
            ).expect("restore missing-root history");
            fixture
                .store
                .publish_startup_terminal_root(Uuid::parse_str(&custody).expect("custody UUID"))
                .expect("restore authenticated projection generation");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn path_cleared_quarantine_retains_its_root_without_poisoning_the_cohort() {
        use rsi_common::types::SandboxCustodyErrorCodeV1;
        let mut fixture = IntegratedFixture::new("path-cleared-quarantine");
        for overlaps in [false, true] {
            let original_base = fixture.sandbox_base.clone();
            if overlaps {
                fixture.sandbox_base = fixture.allocation.root.clone();
            }
            let historical = fixture.insert_synthetic_custody(
                "quarantined-history",
                "repo:quarantined-history".into(),
                fixture.repository.to_string_lossy().into_owned(),
            );
            fixture.sandbox_base = original_base;
            let custody: String = fixture
                .store
                .conn
                .query_row(
                    "SELECT sandbox_custody_id FROM sessions WHERE id=?1",
                    [historical.to_string()],
                    |row| row.get(0),
                )
                .expect("historical custody");
            let custody_id = Uuid::parse_str(&custody).expect("custody UUID");
            fixture
                .store
                .record_custody_cleanup_failure(
                    custody_id,
                    1,
                    SandboxCustodyErrorCodeV1::CleanupFailed,
                )
                .expect("fail cleanup");
            fixture
                .store
                .quarantine_startup_terminal_root(
                    custody_id,
                    SandboxCustodyErrorCodeV1::OwnershipConflict,
                )
                .expect("quarantine history");
            fixture
                .store
                .conn
                .execute(
                    "UPDATE sessions SET sandbox_root=NULL,sandbox_branch=NULL WHERE id=?1",
                    [historical.to_string()],
                )
                .expect("retain path-cleared historical Session");
            fixture
                .store
                .publish_startup_terminal_root(custody_id)
                .expect("publish quarantine");
            assert_eq!(
                fixture.dependency_health(historical),
                1,
                "an ownerless quarantine with cleared paths protects its root without being invalid"
            );
            if !overlaps {
                let audit = fixture.audit();
                assert!(audit.report.applyable);
                assert_eq!(
                    audit.report.items[0].disposition,
                    SourceWorktreeDispositionV1::EligibleIntegratedAncestor
                );
                continue;
            }
            // The retained root path remains evidence even though Session paths
            // are NULL. A root within the candidate must still block its removal.
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
                    .unwrap()
                    .contains("quarantined_session_paths_cleared")
            );

            fixture.store.conn.execute(
            "UPDATE session_execution_projections SET custody_generation=custody_generation+1 WHERE session_id=?1",
            [historical.to_string()],
        ).expect("contradict quarantine generation");
            let audit = fixture.audit();
            assert!(!audit.report.applyable);
            assert!(
                audit.report.items[0]
                    .diagnostic
                    .as_deref()
                    .unwrap()
                    .contains("quarantined_custody_mismatch")
            );
            assert_eq!(fixture.dependency_health(historical), 2);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn durable_session_query_finds_matching_tail_after_16384_unrelated_rows() {
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
            for index in 1..=16_385_u128 {
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn durable_session_query_ignores_more_than_16384_unrelated_rows() {
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
            for index in 1..=16_385 {
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
        assert!(audit.report.applyable);
        assert_eq!(
            audit.report.items[0].disposition,
            SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        );
        assert_eq!(audit.report.items[0].session_path_dependency_count, 0);
        let run = fixture.commit_intent("history-independent-settlement");
        let (outcome, store) = fixture.replay_run(run.run_id);
        assert_eq!(outcome.receipt.counts.settled, 1, "{:#?}", outcome.receipt);
        assert_eq!(
            store
                .blocking_lock()
                .get_session(fixture.session_id)
                .expect("read settled owner")
                .expect("settled owner remains in history")
                .sandbox_cleanup_state,
            Some(SandboxCleanupState::Purged)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
            let mut alias = rsid_store::test_support::make_test_session();
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
                .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON;")
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
                .execute_batch("PRAGMA ignore_check_constraints=OFF; PRAGMA foreign_keys=ON;")
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
        let mut exact = rsid_store::test_support::make_test_session();
        exact.id = exact_id;
        exact.status = SessionStatus::Starting;
        exact.working_dir = fixture.allocation.root.clone();
        fixture
            .store
            .insert_session(&exact)
            .expect("insert exact startup alias");

        let sandbox_id = Uuid::new_v4();
        let mut sandbox_alias = rsid_store::test_support::make_test_session();
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
        let mut projection_alias = rsid_store::test_support::make_test_session();
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
        let mut unrelated = rsid_store::test_support::make_test_session();
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
        assert_eq!(resume.report.plan_digest, watch.report.plan_digest);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
        assert_eq!(missing.report.plan_digest, symbolic.report.plan_digest);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    /// #2003: rows that return early (active owner, nonterminal status,
    /// unverified custody, ...) used to keep that disposition while carrying a
    /// dependency count, so the audit's own wire validation aborted the whole
    /// cohort. They are now retained as their dependency; ambiguous evidence
    /// (`u64::MAX` on every root) has the same shape.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn dependency_bearing_rows_with_early_dispositions_retain_as_dependency() {
        use SourceWorktreeDispositionV1 as D;
        let fixture = IntegratedFixture::new("dependency-early-disposition");
        for (status, owner_active, owner_valid, scheduled, session_paths, expected) in [
            ("Running", false, true, 0, 1, D::SessionPathDependency),
            ("Completed", true, true, 0, 1, D::SessionPathDependency),
            ("Completed", false, false, 0, 1, D::SessionPathDependency),
            ("Running", false, true, 1, 0, D::ScheduledDependency),
            ("Completed", true, true, 1, 1, D::ScheduledDependency),
            (
                "Completed",
                false,
                true,
                0,
                u64::MAX,
                D::SessionPathDependency,
            ),
        ] {
            let mut inventory = fixture.inventory();
            inventory[0].status = Some(status.to_string());
            if !owner_valid {
                inventory[0].owner_session_id = None;
            }
            inventory[0].scheduled_dependency_count = scheduled;
            inventory[0].session_path_dependency_count = session_paths;
            let digest = format!("sha256:{}", "1".repeat(64));
            if scheduled != 0 {
                inventory[0].scheduled_dependency_digest = digest.clone();
            }
            if session_paths != 0 {
                inventory[0].session_path_dependency_digest = digest;
            }
            let active_ids = owner_active
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
            .expect("a dependency-bearing early row must not abort the audit");
            assert_eq!(audit.report.items[0].disposition, expected);
            assert!(!audit.report.applyable);
            assert!(audit.eligible.is_empty());
            let expected_text = if session_paths == u64::MAX {
                "evidence is ambiguous"
            } else {
                "retained for its dependency"
            };
            assert!(
                audit.report.items[0]
                    .diagnostic
                    .as_deref()
                    .is_some_and(|text| text.contains(expected_text)),
                "{:?}",
                audit.report.items[0].diagnostic
            );
            audit
                .report
                .validate_wire()
                .expect("dependency retention is wire-valid");
            assert!(fixture.allocation.root.exists());
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn audit_then_apply_binds_only_material_settlement_changes() {
        for change in [
            "none",
            "retained_activity",
            "caches",
            "dirty",
            "target",
            "source",
            "dependency",
            "new_eligible",
        ] {
            let mut fixture = IntegratedFixture::new("audit-apply");
            fixture.add_second_root("retained-worker");
            let retained = fixture
                .inventory()
                .into_iter()
                .find(|row| row.session_id != Some(fixture.session_id))
                .unwrap();
            let retained_id = retained.session_id.unwrap();
            fixture
                .store
                .conn
                .execute(
                    "UPDATE sessions SET status='Running' WHERE id=?1",
                    [retained_id.to_string()],
                )
                .unwrap();
            let (caller, project) = grant_storage_manager(&fixture);
            fixture
                .store
                .conn
                .execute(
                    "UPDATE sessions SET project_id=?1 WHERE id=?2",
                    rusqlite::params![project.to_string(), retained_id.to_string()],
                )
                .unwrap();
            let excludes = fixture.sandbox_base.join("settlement-excludes");
            std::fs::write(&excludes, "__pycache__/\n.pytest_cache/\n*.pyc\n").unwrap();
            git(
                &fixture.repository,
                &["config", "core.excludesFile", excludes.to_str().unwrap()],
            );
            if change == "caches" {
                for name in [
                    "scripts/__pycache__/module.cpython-314.pyc",
                    ".pytest_cache/v/cache/nodeids",
                    "loose.pyc",
                ] {
                    let path = fixture.allocation.root.join(name);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, "regenerable cache").unwrap();
                }
            }
            #[cfg(target_os = "linux")]
            let _proc = crate::session::reaper::scoped_archive_cleanup_test_proc(
                fixture.session_id,
                &fixture.sandbox_base,
            );
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            let _context = executor.enter();
            let manager = Arc::new(
                SessionManager::new(
                    Arc::new(crate::bus::EventBus::new(16)),
                    std::mem::replace(
                        &mut fixture.store,
                        crate::store::Store::open_in_memory().unwrap(),
                    ),
                    false,
                    fixture._directory.path().join("daemon.sock"),
                    None,
                    Vec::new(),
                    crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                    fixture.sandbox_base.clone(),
                )
                .unwrap(),
            );
            manager
                .topology_agent_self
                .set(Arc::downgrade(&manager))
                .unwrap();
            executor.block_on(async {
                tokio::task::yield_now().await;
                let control = manager.agent_control();
                let identity = fixture.target.repository_identity.clone();
                let audit = control.agent_audit_source_worktree_cohort(caller,
                    rsi_common::cohort_settlement::AuditSourceWorktreeCohortParams {
                        repository_identity: identity.clone(),
                    }).await.unwrap();
                assert_eq!(audit.counts.eligible, 1, "{change}: {audit:#?}");
                match change {
                    "retained_activity" => {
                        manager.store.lock().await.conn.execute(
                            "UPDATE sessions SET updated_at='2030-01-01T00:00:00.000000000Z' WHERE id=?1",
                            [retained_id.to_string()]).unwrap();
                        // A live worker's unrelated ref moves without changing the plan.
                        git(&fixture.repository, &["update-ref", &format!("refs/heads/{}", retained.sandbox_branch), &fixture.target.target_oid]);
                    }
                    "dirty" => { std::fs::write(fixture.allocation.root.join("tracked"), "unique work").unwrap(); }
                    "target" => {
                        std::fs::write(fixture.repository.join("later"), "later").unwrap();
                        git(&fixture.repository, &["add", "later"]);
                        git(&fixture.repository, &["commit", "-qm", "advance target"]);
                    }
                    "source" => { git(&fixture.repository, &["update-ref", &format!("refs/heads/{}", fixture.branch), &fixture.target.target_oid]); }
                    "dependency" => {
                        let mut consumer = rsid_store::test_support::make_test_session();
                        consumer.status = SessionStatus::Running;
                        consumer.working_dir = fixture.allocation.root.clone();
                        manager.store.lock().await.insert_session(&consumer).unwrap();
                    }
                    "new_eligible" => {
                        manager.store.lock().await.conn.execute("UPDATE sessions SET status='Completed' WHERE id=?1",
                            [retained_id.to_string()]).unwrap();
                    }
                    _ => {}
                }
                let result = agent_apply_to_completion(&manager, caller, ApplySourceWorktreeCohortParams {
                    repository_identity: identity,
                    plan_digest: audit.plan_digest,
                    authorization: audit.authorization_phrase.unwrap(),
                    idempotency_key: format!("fixture:audit-apply-{change}"),
                }).await;
                if matches!(change, "none" | "retained_activity" | "caches") {
                    let receipt = result.unwrap_or_else(|error| panic!("{change}: {error}"));
                    assert_eq!(receipt.counts.settled, 1, "{change}: {receipt:#?}");
                    assert_eq!(receipt.counts.retained, 1);
                    assert!(!fixture.allocation.root.exists());
                    let session = manager.store.lock().await.get_session(fixture.session_id).unwrap().unwrap();
                    assert_eq!(session.sandbox_cleanup_state, Some(SandboxCleanupState::Purged));
                } else {
                    result.expect_err("material drift must refuse before intent");
                    let count: i64 = manager.store.lock().await.conn.query_row(
                        "SELECT count(*) FROM source_worktree_settlement_runs", [], |row| row.get(0)).unwrap();
                    assert_eq!(count, 0, "{change}");
                    assert!(fixture.allocation.root.exists());
                }
                assert!(Path::new(&retained.sandbox_root).exists());
            });
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
        assert_eq!(first.report.plan_digest, second.report.plan_digest);

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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

        git_worktree::with_repository_mutation(&repository, || {
            retry_settlement_database_items(
                &store,
                &active,
                &completed,
                &first.receipt,
                &sandbox_base,
            )
        })
        .expect("driver retries the transient database tail before returning");
        let retried = store
            .blocking_lock()
            .get_source_worktree_settlement_run(first.receipt.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(retried.counts.settled, 1);
        assert!(retried.finished_at.is_some());

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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
        git(&repository, &["push", "-q", "origin", "main"]);
        let target_oid_before = git(&repository, &["rev-parse", "refs/heads/main"]);
        let unrelated_before = git(&repository, &["rev-parse", "refs/heads/keep/unrelated"]);
        let target = git_worktree::with_repository_mutation(&repository, || {
            git_worktree::observe_repository_target_locked(&repository)
        })
        .expect("observe fixture repository identity");

        let mut store = crate::store::Store::open_in_memory().expect("settlement Store");
        let custody_id = Uuid::new_v4();
        let mut session = rsid_store::test_support::make_test_session();
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
        // Settlement binds the fetched origin tip, not the local branch (#2040).
        git(
            &repository,
            &["update-ref", "refs/remotes/origin/main", &source_oid],
        );
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
            &["update-ref", "refs/remotes/origin/main", &target_oid_before],
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

    /// An agent apply answers once the run is recorded (#2789); wait here for
    /// the detached driver to finish and return the run's final receipt.
    async fn agent_apply_to_completion(
        manager: &Arc<SessionManager>,
        caller: Uuid,
        params: ApplySourceWorktreeCohortParams,
    ) -> Result<SourceWorktreeSettlementRunV1> {
        let started = manager
            .agent_control()
            .agent_apply_source_worktree_cohort(caller, params)
            .await?;
        await_run_driver_finished(started.run_id).await;
        manager
            .get_source_worktree_settlement_run(started.run_id)
            .await?
            .ok_or_else(|| DaemonError::Store("settlement receipt disappeared".into()))
    }

    async fn await_run_driver_finished(run_id: Uuid) {
        tokio::time::timeout(std::time::Duration::from_secs(120), async {
            while settlement_run_in_flight(run_id) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the detached settlement driver finishes");
    }

    fn grant_storage_manager(fixture: &IntegratedFixture) -> (Uuid, Uuid) {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        use rsi_common::harness_manager_v2::{
            ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerOperatingModeV2,
            ManagerPolicyV2,
        };
        let project = Uuid::new_v4();
        let now = chrono::Utc::now();
        fixture
            .store
            .insert_project(&rsi_common::types::Project {
                id: project,
                name: project.to_string(),
                path: Some(fixture.repository.clone()),
                description: None,
                color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET project_id=?1 WHERE id=?2",
                rusqlite::params![project.to_string(), fixture.session_id.to_string()],
            )
            .unwrap();
        let mut manager = rsid_store::test_support::make_test_session();
        manager.project_id = Some(project);
        manager.working_dir = fixture.repository.clone();
        let caller = manager.id;
        fixture.store.insert_session(&manager).unwrap();
        fixture
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: project,
                session_id: caller,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            })
            .unwrap();
        let config = fixture.store.get_harness_manager(project).unwrap().unwrap();
        fixture
            .store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: config.row_version,
                expected_policy_version: 0,
                idempotency_key: "fixture:storage-policy".into(),
                policy: ManagerPolicyV2 {
                    mode: ManagerOperatingModeV2::Execute,
                    capabilities: vec![ManagerCapabilityV2::StorageControl],
                    ..Default::default()
                },
            })
            .unwrap();
        (caller, project)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn automatic_settlement_pressure_scales_to_laptop_and_respects_configured_lower_floor() {
        let gib = 1024 * 1024 * 1024;
        assert!(!automatic_settlement_due(true, 100, 48 * gib, 192 * gib));
        assert!(automatic_settlement_due(true, 100, 48 * gib - 1, 192 * gib));
        assert!(!automatic_settlement_due(true, 20, 20 * gib, 192 * gib));
        assert!(automatic_settlement_due(true, 20, 20 * gib - 1, 192 * gib));
        assert!(!automatic_settlement_due(true, 100, 100 * gib, 775 * gib));
        assert!(automatic_settlement_due(
            true,
            100,
            100 * gib - 1,
            775 * gib
        ));
        assert!(!automatic_settlement_due(false, 100, 0, 192 * gib));
        // Unknown size keeps the configured floor, matching launch admission.
        assert!(automatic_settlement_due(true, 100, 99 * gib, 0));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(start_paused = true)]
    async fn automatic_settlement_uses_shared_proof_and_records_visible_attributed_runs() {
        use rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1;
        use std::sync::atomic::Ordering;
        for kind in ["merged", "dirty", "divergent"] {
            let fixture = IntegratedFixture::new(&format!("automatic-{kind}"));
            if kind == "dirty" {
                std::fs::write(fixture.allocation.root.join("dirty"), "retain me").unwrap();
            } else if kind == "divergent" {
                std::fs::write(fixture.allocation.root.join("tracked"), "unmerged change").unwrap();
                git(&fixture.allocation.root, &["add", "tracked"]);
                git(&fixture.allocation.root, &["commit", "-qm", "unmerged"]);
            }
            let root = fixture.allocation.root.clone();
            let identity = fixture.target.repository_identity.clone();
            let base = fixture.sandbox_base.clone();
            let source_ref = format!("refs/heads/{}", fixture.branch);
            let source_before = git(&fixture.repository, &["rev-parse", &source_ref]);
            let runtime =
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
            let manager = SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                fixture.store,
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                Arc::clone(&runtime),
                base.clone(),
            )
            .unwrap();
            drain_background_startup_tick().await;
            // The default 100 GB setting is capped at 48 GB on a 192 GB
            // laptop. Exactly at that floor is sufficient; disabled also
            // retains work below it. Smaller configured floors still apply.
            let gib = 1024 * 1024 * 1024;
            settlement_disk_samples()
                .lock()
                .unwrap()
                .insert(base.clone(), (48 * gib, 192 * gib));
            manager.run_source_worktree_auto_settlement().await.unwrap();
            assert!(root.is_dir());
            runtime
                .source_worktree_auto_settlement_enabled
                .store(false, Ordering::Relaxed);
            settlement_disk_samples()
                .lock()
                .unwrap()
                .insert(base.clone(), (10 * gib, 192 * gib));
            manager.run_source_worktree_auto_settlement().await.unwrap();
            assert!(
                manager
                    .list_source_worktree_settlement_runs(20)
                    .await
                    .unwrap()
                    .is_empty()
            );
            runtime
                .source_worktree_auto_settlement_enabled
                .store(true, Ordering::Relaxed);
            runtime
                .disk_pressure_min_free_gb
                .store(5, Ordering::Relaxed);
            manager.run_source_worktree_auto_settlement().await.unwrap();
            assert!(root.is_dir());
            runtime
                .disk_pressure_min_free_gb
                .store(20, Ordering::Relaxed);
            manager.run_source_worktree_auto_settlement().await.unwrap();
            let runs = manager
                .list_source_worktree_settlement_runs(20)
                .await
                .unwrap();
            if kind == "merged" {
                assert_eq!(runs.len(), 1);
                assert_eq!(runs[0].repository_identity, identity);
                assert_eq!(runs[0].counts.settled, 1, "{:#?}", runs[0]);
                assert_eq!(
                    runs[0].attribution.as_ref().unwrap().origin,
                    SourceWorktreeSettlementOriginV1::Automatic
                );
                let receipt = manager
                    .get_source_worktree_settlement_run(runs[0].run_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(receipt.counts.settled, 1, "{receipt:#?}");
                assert_eq!(receipt.attribution, runs[0].attribution);
                // Subsequent low-disk passes never duplicate an already-settled run.
                manager.run_source_worktree_auto_settlement().await.unwrap();
                assert_eq!(
                    manager
                        .list_source_worktree_settlement_runs(20)
                        .await
                        .unwrap()
                        .len(),
                    1
                );
            } else {
                assert!(root.is_dir());
                assert_eq!(
                    git(&fixture.repository, &["rev-parse", &source_ref]),
                    source_before
                );
                assert!(runs.is_empty());
            }
            settlement_disk_samples().lock().unwrap().remove(&base);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(start_paused = true)]
    async fn automatic_tick_resumes_only_its_own_retained_intent_with_the_original_receipt() {
        use rsi_common::cohort_settlement::{
            SourceWorktreeSettlementAttributionV1, SourceWorktreeSettlementOriginV1,
        };
        use std::sync::atomic::Ordering;
        for origin in [
            SourceWorktreeSettlementOriginV1::Automatic,
            SourceWorktreeSettlementOriginV1::Operator,
            SourceWorktreeSettlementOriginV1::Agent,
        ] {
            let mut fixture = IntegratedFixture::new(&format!("automatic-replay-{origin:?}"));
            let audit = fixture.audit();
            let mut run = fixture.run_from_audit(&audit, "fixture:automatic-retained");
            // This test enters the real request replay path. Most fixture
            // tests enter the locked saga directly and use a synthetic hash.
            run.request_fingerprint = hash_canonical(&CanonicalApplyFingerprint {
                schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                repository_identity: &run.repository_identity,
                plan_digest: &run.plan_digest,
                authorization_digest: &run.authorization_digest,
                idempotency_key: &run.idempotency_key,
            })
            .unwrap()
            .as_str()
            .to_string();
            let attribution = SourceWorktreeSettlementAttributionV1 {
                origin,
                agent_session_id: (origin == SourceWorktreeSettlementOriginV1::Agent)
                    .then_some(fixture.session_id),
            };
            fixture
                .store
                .insert_source_worktree_settlement_run_as(&run, &attribution)
                .unwrap();
            let root = fixture.allocation.root.clone();
            let base = fixture.sandbox_base.clone();
            let runtime =
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
            let manager = SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                fixture.store,
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                Arc::clone(&runtime),
                base.clone(),
            )
            .unwrap();
            drain_background_startup_tick().await;
            settlement_disk_samples()
                .lock()
                .unwrap()
                .insert(base.clone(), (0, 192 * 1024 * 1024 * 1024));
            runtime
                .source_worktree_auto_settlement_enabled
                .store(false, Ordering::Relaxed);
            manager.run_source_worktree_auto_settlement().await.unwrap();
            assert!(root.is_dir());
            runtime
                .source_worktree_auto_settlement_enabled
                .store(true, Ordering::Relaxed);
            settlement_disk_samples().lock().unwrap().insert(
                base.clone(),
                (48 * 1024 * 1024 * 1024, 192 * 1024 * 1024 * 1024),
            );
            manager.run_source_worktree_auto_settlement().await.unwrap();
            assert!(root.is_dir());
            settlement_disk_samples()
                .lock()
                .unwrap()
                .insert(base.clone(), (0, 192 * 1024 * 1024 * 1024));
            manager.run_source_worktree_auto_settlement().await.unwrap();
            let receipt = manager
                .get_source_worktree_settlement_run(run.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(receipt.run_id, run.run_id);
            assert_eq!(receipt.idempotency_key, run.idempotency_key);
            assert_eq!(receipt.attribution, Some(attribution));
            assert_eq!(
                manager
                    .list_source_worktree_settlement_runs(20)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            if origin == SourceWorktreeSettlementOriginV1::Automatic {
                assert_eq!(receipt.counts.settled, 1, "{receipt:#?}");
            } else {
                assert!(root.is_dir());
                assert_eq!(
                    receipt.items[0].phase,
                    SourceWorktreeSettlementPhaseV1::IntentCommitted
                );
            }
            settlement_disk_samples().lock().unwrap().remove(&base);
        }
    }

    /// Bodies of the settlement notice Issues in the store, oldest first.
    fn settlement_notice_bodies(store: &crate::store::Store) -> Vec<String> {
        let mut statement = store
            .conn
            .prepare("SELECT body FROM issues WHERE body LIKE '%(auto-filed by the daemon, #2452)%' ORDER BY created_at")
            .expect("prepare notice query");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query notices")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect notices")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(start_paused = true)]
    async fn automatic_terminal_fence_and_failed_audit_notify_the_owner_once_2452() {
        use rsi_common::cohort_settlement::{
            SourceWorktreeSettlementAttributionV1, SourceWorktreeSettlementOriginV1,
        };
        for origin in [
            SourceWorktreeSettlementOriginV1::Automatic,
            SourceWorktreeSettlementOriginV1::Operator,
        ] {
            let mut fixture = IntegratedFixture::new(&format!("notice-{origin:?}"));
            let audit = fixture.audit();
            let mut run = fixture.run_from_audit(&audit, "fixture:notice-fence");
            run.request_fingerprint = hash_canonical(&CanonicalApplyFingerprint {
                schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
                policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
                repository_identity: &run.repository_identity,
                plan_digest: &run.plan_digest,
                authorization_digest: &run.authorization_digest,
                idempotency_key: &run.idempotency_key,
            })
            .unwrap()
            .as_str()
            .to_string();
            fixture
                .store
                .insert_source_worktree_settlement_run_as(
                    &run,
                    &SourceWorktreeSettlementAttributionV1 {
                        origin,
                        agent_session_id: None,
                    },
                )
                .unwrap();
            // The terminal recovery fence the automatic pass must not leave silent.
            fixture
                .store
                .mark_source_worktree_settlement_recovery_required(
                    run.run_id,
                    fixture.session_id,
                    SourceWorktreeSettlementPhaseV1::IntentCommitted,
                    SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                    "fixture: quarantine residue could not be proven",
                )
                .unwrap();
            let session_id = fixture.session_id;
            let _repository_dir = fixture.repository.clone();
            let identity = fixture.target.repository_identity.clone();
            let base = fixture.sandbox_base.clone();
            let runtime =
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
            let manager = SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                fixture.store,
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                Arc::clone(&runtime),
                base.clone(),
            )
            .unwrap();
            drain_background_startup_tick().await;
            settlement_disk_samples()
                .lock()
                .unwrap()
                .insert(base.clone(), (0, 192 * 1024 * 1024 * 1024));

            // A fresh pass and a retry (the "reopen") see the same fence. One
            // sighting is run history only (#2630); the fence that survives the
            // next pass has an owner.
            manager.run_source_worktree_auto_settlement().await.unwrap();
            let first_pass = settlement_notice_bodies(&*manager.store.lock().await);
            assert!(first_pass.is_empty(), "{first_pass:#?}");
            manager.run_source_worktree_auto_settlement().await.unwrap();
            let bodies = settlement_notice_bodies(&*manager.store.lock().await);
            if origin == SourceWorktreeSettlementOriginV1::Operator {
                // A manual run returned its receipt to a waiting caller, so its
                // fence files no run notice; the automatic apply it blocks is
                // reported as that failure, once.
                assert_eq!(bodies.len(), 1, "{bodies:#?}");
                assert!(bodies[0].contains("- stage: `apply`"), "{bodies:#?}");
                assert!(!bodies[0].contains(&run.run_id.to_string()), "{bodies:#?}");
                let receipt = manager
                    .get_source_worktree_settlement_run(run.run_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(receipt.counts.recovery_required, 1, "{receipt:#?}");
                settlement_disk_samples().lock().unwrap().remove(&base);
                continue;
            }
            // The apply failures the fence causes are covered by its notice.
            assert_eq!(bodies.len(), 1, "{bodies:#?}");
            for evidence in [
                run.run_id.to_string(),
                identity.clone(),
                session_id.to_string(),
                "recovery_proof_failed".to_string(),
                "fixture: quarantine residue could not be proven".to_string(),
            ] {
                assert!(bodies[0].contains(&evidence), "{evidence} in {}", bodies[0]);
            }

            settlement_disk_samples().lock().unwrap().remove(&base);
        }
    }

    /// #2465: an automatic apply that fails after it persisted a recovery fence
    /// files the fence's run notice, not a second failure Issue, so the failed
    /// apply, the next ticks and a retry leave exactly one owner Issue.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(start_paused = true)]
    async fn failed_apply_that_persisted_a_fence_files_one_owner_issue_2465() {
        use rsi_common::cohort_settlement::{
            SourceWorktreeSettlementAttributionV1, SourceWorktreeSettlementOriginV1,
        };
        let mut fixture = IntegratedFixture::new("notice-apply-fence");
        let audit = fixture.audit();
        let mut run = fixture.run_from_audit(&audit, "fixture:notice-apply-fence");
        run.request_fingerprint = hash_canonical(&CanonicalApplyFingerprint {
            schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
            policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
            repository_identity: &run.repository_identity,
            plan_digest: &run.plan_digest,
            authorization_digest: &run.authorization_digest,
            idempotency_key: &run.idempotency_key,
        })
        .unwrap()
        .as_str()
        .to_string();
        fixture
            .store
            .insert_source_worktree_settlement_run_as(
                &run,
                &SourceWorktreeSettlementAttributionV1 {
                    origin: SourceWorktreeSettlementOriginV1::Automatic,
                    agent_session_id: None,
                },
            )
            .unwrap();
        // The apply persisted a recovery fence, then its own error escaped.
        fixture
            .store
            .mark_source_worktree_settlement_recovery_required(
                run.run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementRefusalV1::RecoveryProofFailed.as_str(),
                "fixture: quarantine residue could not be proven",
            )
            .unwrap();
        let base = fixture.sandbox_base.clone();
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            fixture.store,
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            Arc::clone(&runtime),
            base.clone(),
        )
        .unwrap();
        drain_background_startup_tick().await;
        settlement_disk_samples()
            .lock()
            .unwrap()
            .insert(base.clone(), (0, 192 * 1024 * 1024 * 1024));

        let cohorts = manager.list_source_worktree_cohorts().await.unwrap();
        assert_eq!(cohorts.len(), 1, "{cohorts:#?}");
        let failure = DaemonError::Store("fixture: apply failed after the fence".into());
        // Tick 1's failed apply, then two more ticks and a retry of the failure.
        manager
            .notify_automatic_settlement_failed_attempt(&cohorts[0], "apply", &failure)
            .await;
        manager.run_source_worktree_auto_settlement().await.unwrap();
        manager.run_source_worktree_auto_settlement().await.unwrap();
        manager
            .notify_automatic_settlement_failed_attempt(&cohorts[0], "apply", &failure)
            .await;
        let bodies = settlement_notice_bodies(&*manager.store.lock().await);
        assert_eq!(bodies.len(), 1, "{bodies:#?}");
        assert!(bodies[0].contains(&run.run_id.to_string()), "{bodies:#?}");
        assert!(bodies[0].contains("recovery_proof_failed"), "{bodies:#?}");
        settlement_disk_samples().lock().unwrap().remove(&base);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(start_paused = true)]
    async fn failed_automatic_audit_notifies_the_owner_once_2452() {
        let fixture = IntegratedFixture::new("notice-audit");
        let identity = fixture.target.repository_identity.clone();
        let base = fixture.sandbox_base.clone();
        // The repository vanishes under the cohort, so the audit cannot run.
        std::fs::remove_dir_all(&fixture.repository).expect("remove repository");
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            fixture.store,
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            Arc::clone(&runtime),
            base.clone(),
        )
        .unwrap();
        drain_background_startup_tick().await;
        settlement_disk_samples()
            .lock()
            .unwrap()
            .insert(base.clone(), (0, 192 * 1024 * 1024 * 1024));
        // One failed pass is run history only (#2630); the retry (the
        // "reopen") fails the same way, so the failure has an owner, once.
        manager.run_source_worktree_auto_settlement().await.unwrap();
        let first_pass = settlement_notice_bodies(&*manager.store.lock().await);
        assert!(first_pass.is_empty(), "{first_pass:#?}");
        manager.run_source_worktree_auto_settlement().await.unwrap();
        manager.run_source_worktree_auto_settlement().await.unwrap();
        let bodies = settlement_notice_bodies(&*manager.store.lock().await);
        assert_eq!(bodies.len(), 1, "{bodies:#?}");
        for evidence in ["- stage: `audit`", identity.as_str(), "- error class: `"] {
            assert!(bodies[0].contains(evidence), "{evidence} in {}", bodies[0]);
        }
        settlement_disk_samples().lock().unwrap().remove(&base);
    }

    /// Hub #3031: a pass over a cohort whose last audit found nothing to apply
    /// does not audit it again while nothing about it changed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(start_paused = true)]
    async fn unchanged_retained_cohort_is_not_audited_again_inside_the_backoff_3031() {
        let fixture = IntegratedFixture::new("audit-fence");
        // A dirty root: the audit succeeds and plans nothing.
        std::fs::write(fixture.allocation.root.join("dirty"), "retain me").unwrap();
        let root = fixture.allocation.root.clone();
        let session_id = fixture.session_id;
        let base = fixture.sandbox_base.clone();
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            fixture.store,
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            Arc::clone(&runtime),
            base.clone(),
        )
        .unwrap();
        drain_background_startup_tick().await;
        let gib = 1024 * 1024 * 1024;
        let pressured = (0, 192 * gib);
        settlement_disk_samples()
            .lock()
            .unwrap()
            .insert(base.clone(), pressured);
        let pass = || async {
            let mut tally = AutoSettlementTally::default();
            manager
                .run_source_worktree_auto_settlement_tallied(&mut tally)
                .await
                .unwrap();
            (tally.audited, tally.audit_fenced, tally.applied)
        };

        // The first pass audits; the unchanged cohort is then fenced, on and on.
        assert_eq!(pass().await, (1, 0, 0));
        assert_eq!(pass().await, (0, 1, 0));
        assert_eq!(pass().await, (0, 1, 0));
        assert!(root.is_dir());

        // A changed row (the owner Session was touched) is audited at once and
        // the new inputs are fenced in turn.
        {
            let store = manager.store.lock().await;
            store
                .conn
                .execute(
                    "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                        session_id.to_string()
                    ],
                )
                .unwrap();
        }
        assert_eq!(pass().await, (1, 0, 0));
        assert_eq!(pass().await, (0, 1, 0));

        // The backoff cap forces an audit of an unchanged cohort.
        manager.with_settlement_audit_fence(|fence| {
            *fence = super::super::settlement_audit_fence::AuditFence::with_windows(
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
            );
        });
        assert_eq!(pass().await, (1, 0, 0));
        assert_eq!(pass().await, (1, 0, 0));
        manager.with_settlement_audit_fence(|fence| {
            *fence = super::super::settlement_audit_fence::AuditFence::default();
        });
        assert_eq!(pass().await, (1, 0, 0));
        assert_eq!(pass().await, (0, 1, 0));

        // Relief clears the fence: the next pressure episode audits at once.
        settlement_disk_samples()
            .lock()
            .unwrap()
            .insert(base.clone(), (192 * gib, 192 * gib));
        assert_eq!(pass().await, (0, 0, 0));
        assert_eq!(manager.with_settlement_audit_fence(|fence| fence.len()), 0);
        settlement_disk_samples()
            .lock()
            .unwrap()
            .insert(base.clone(), pressured);
        assert_eq!(pass().await, (1, 0, 0));
        assert!(root.is_dir());
        settlement_disk_samples().lock().unwrap().remove(&base);
    }

    /// Hub #3056: the observed settlement target tip is a fence input. A fenced
    /// cohort is audited on the next tick once only the tip moved, and a tip
    /// that cannot be read audits every pass (fail open).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(start_paused = true)]
    async fn a_moved_target_tip_re_audits_a_fenced_cohort_at_once_3056() {
        let fixture = IntegratedFixture::new("audit-fence-tip");
        // A dirty root: the audit succeeds and plans nothing.
        std::fs::write(fixture.allocation.root.join("dirty"), "retain me").unwrap();
        let repository = fixture.repository.clone();
        let target_ref = fixture.target.target_ref.clone();
        let base = fixture.sandbox_base.clone();
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            fixture.store,
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            Arc::clone(&runtime),
            base.clone(),
        )
        .unwrap();
        drain_background_startup_tick().await;
        settlement_disk_samples()
            .lock()
            .unwrap()
            .insert(base.clone(), (0, 192 * 1024 * 1024 * 1024));
        let pass = || async {
            let mut tally = AutoSettlementTally::default();
            manager
                .run_source_worktree_auto_settlement_tallied(&mut tally)
                .await
                .unwrap();
            (tally.audited, tally.audit_fenced, tally.applied)
        };

        // Audited once, then fenced while the tip is unchanged.
        assert_eq!(pass().await, (1, 0, 0));
        assert_eq!(pass().await, (0, 1, 0));
        assert_eq!(pass().await, (0, 1, 0));

        // Only the target tip moves: the next tick audits, then fences again.
        let tip = git(&repository, &["rev-parse", &target_ref]);
        let moved = git(
            &repository,
            &[
                "commit-tree",
                &format!("{tip}^{{tree}}"),
                "-p",
                &tip,
                "-m",
                "advance",
            ],
        );
        assert_ne!(moved, tip);
        git(&repository, &["update-ref", &target_ref, &moved]);
        assert_eq!(pass().await, (1, 0, 0));
        assert_eq!(pass().await, (0, 1, 0));

        // A tip that cannot be read (detached target) is never fenced.
        std::fs::write(repository.join(".git").join("HEAD"), format!("{moved}\n")).unwrap();
        assert_eq!(pass().await, (1, 0, 0));
        assert_eq!(pass().await, (1, 0, 0));
        assert_eq!(manager.with_settlement_audit_fence(|fence| fence.len()), 0);
        settlement_disk_samples().lock().unwrap().remove(&base);
    }

    /// Hub #3031 and #2630: a failed audit is never fenced, so its persistence
    /// streak counts every pass exactly as before and still files one Issue.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(start_paused = true)]
    async fn a_failed_audit_is_never_fenced_and_keeps_its_streak_3031() {
        let fixture = IntegratedFixture::new("audit-fence-failed");
        let base = fixture.sandbox_base.clone();
        std::fs::remove_dir_all(&fixture.repository).expect("remove repository");
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            fixture.store,
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            Arc::clone(&runtime),
            base.clone(),
        )
        .unwrap();
        drain_background_startup_tick().await;
        settlement_disk_samples()
            .lock()
            .unwrap()
            .insert(base.clone(), (0, 192 * 1024 * 1024 * 1024));
        for pass in 0..3 {
            let mut tally = AutoSettlementTally::default();
            manager
                .run_source_worktree_auto_settlement_tallied(&mut tally)
                .await
                .unwrap();
            assert_eq!(
                (tally.audited, tally.audit_fenced, tally.audit_retained),
                (1, 0, 1),
                "pass {pass}"
            );
            let bodies = settlement_notice_bodies(&*manager.store.lock().await);
            // One failed pass is history only; the streak files once, and only once.
            assert_eq!(
                bodies.len(),
                usize::from(pass > 0),
                "pass {pass}: {bodies:#?}"
            );
        }
        assert_eq!(manager.with_settlement_audit_fence(|fence| fence.len()), 0);
        settlement_disk_samples().lock().unwrap().remove(&base);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn automatic_pressure_stop_immediately_closes_remaining_fresh_and_replayed_items() {
        use rsi_common::cohort_settlement::{
            SourceWorktreeSettlementAttributionV1, SourceWorktreeSettlementOriginV1,
        };
        use std::sync::atomic::Ordering;
        for replaying in [false, true] {
            for disable in [false, true] {
                let mut fixture = IntegratedFixture::new("automatic-mid-run-stop");
                for index in 1..6 {
                    fixture.add_second_root(&format!("mid-run-{index}"));
                }
                let audit = fixture.audit();
                let plan_digest = audit.report.plan_digest.as_str().to_string();
                let run_template = fixture.run_from_audit(&audit, "automatic:mid-run");
                let start = if replaying {
                    let run = fixture.commit_intent_for_all_eligible_as(
                        "automatic:mid-run",
                        &SourceWorktreeSettlementAttributionV1 {
                            origin: SourceWorktreeSettlementOriginV1::Automatic,
                            agent_session_id: None,
                        },
                    );
                    ApplyStart::Replay(
                        fixture
                            .store
                            .get_source_worktree_settlement_run(run.run_id)
                            .unwrap()
                            .unwrap(),
                    )
                } else {
                    ApplyStart::Fresh(audit)
                };
                let runtime =
                    crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
                let base = fixture.sandbox_base.clone();
                settlement_disk_samples()
                    .lock()
                    .unwrap()
                    .insert(base.clone(), (0, 192 * 1024 * 1024 * 1024));
                let callback_base = base.clone();
                let callback_runtime = Arc::clone(&runtime);
                set_after_dangling_remove_test_hook(move || {
                    if disable {
                        callback_runtime
                            .source_worktree_auto_settlement_enabled
                            .store(false, Ordering::Relaxed);
                    } else {
                        settlement_disk_samples()
                            .lock()
                            .unwrap()
                            .insert(callback_base, (u64::MAX, 192 * 1024 * 1024 * 1024));
                    }
                });
                let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
                    &mut fixture.store,
                    crate::store::Store::open_in_memory().unwrap(),
                )));
                let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
                let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
                let outcome = fixture
                    .with_quarantine_holder_test_proc(|| {
                        git_worktree::with_repository_mutation(&fixture.repository, || {
                            apply_locked_with_agent_authority(
                                start,
                                &run_template.repository_identity,
                                &run_template.idempotency_key,
                                &plan_digest,
                                &run_template.authorization_digest,
                                &run_template.request_fingerprint,
                                &base,
                                &store,
                                &active,
                                &completed,
                                false,
                                None,
                                Some(&runtime),
                            )
                        })
                    })
                    .unwrap();
                assert_eq!(outcome.receipt.counts.settled, 1);
                assert_eq!(outcome.receipt.counts.unattempted, 5);
                assert!(
                    outcome
                        .receipt
                        .items
                        .iter()
                        .all(|item| item.phase.is_terminal())
                );
                for item in &outcome.receipt.items {
                    if item.phase == SourceWorktreeSettlementPhaseV1::Unattempted {
                        let root = base.join(item.session_id.to_string());
                        assert!(&root.is_dir());
                        assert_eq!(
                            git(&fixture.repository, &["rev-parse", &item.source_ref]),
                            item.expected_source_oid.as_str()
                        );
                        assert!(
                            !store
                                .blocking_lock()
                                .path_is_inside_settlement_root(&root)
                                .unwrap()
                        );
                    }
                }
                settlement_disk_samples().lock().unwrap().remove(&base);
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn startup_recovery_retains_untouched_roots_after_disable_pressure_clear_or_agent_revocation() {
        use rsi_common::cohort_settlement::{
            SourceWorktreeSettlementAttributionV1, SourceWorktreeSettlementOriginV1,
        };
        use std::sync::atomic::Ordering;
        for reason in ["disabled", "pressure-cleared", "agent-revoked"] {
            let mut fixture = IntegratedFixture::new(&format!("startup-policy-{reason}"));
            let runtime =
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
            let attribution;
            let run;
            if reason == "agent-revoked" {
                let (caller, project) = grant_storage_manager(&fixture);
                attribution = SourceWorktreeSettlementAttributionV1 {
                    origin: SourceWorktreeSettlementOriginV1::Agent,
                    agent_session_id: Some(caller),
                };
                run = fixture.run_from_audit(&fixture.audit(), "startup:revoked-agent");
                fixture
                    .store
                    .insert_source_worktree_settlement_run_as(&run, &attribution)
                    .unwrap();
                let mut grant = fixture
                    .store
                    .get_harness_manager_policy(project)
                    .unwrap()
                    .unwrap();
                grant.policy.capabilities.clear();
                fixture
                    .store
                    .configure_harness_manager_policy(
                        &rsi_common::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2 {
                            project_id: project,
                            expected_scope_version: grant.scope_version,
                            expected_policy_version: grant.row_version,
                            policy: grant.policy,
                            idempotency_key: "startup:revoke-storage".into(),
                        },
                    )
                    .unwrap();
            } else {
                for index in 1..6 {
                    fixture.add_second_root(&format!("startup-policy-{index}"));
                }
                attribution = SourceWorktreeSettlementAttributionV1 {
                    origin: SourceWorktreeSettlementOriginV1::Automatic,
                    agent_session_id: None,
                };
                run = fixture
                    .commit_intent_for_all_eligible_as("startup:automatic-six", &attribution);
                // Reproduce the retained on-disk state from before this fix:
                // one of six roots settled, five intents still untouched.
                let first = fixture
                    .store
                    .list_source_worktree_settlement_journal_items(run.run_id)
                    .unwrap()
                    .remove(0);
                let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
                    &mut fixture.store,
                    crate::store::Store::open_in_memory().unwrap(),
                )));
                let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
                let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
                fixture.with_quarantine_holder_test_proc(|| {
                    git_worktree::with_repository_mutation(&fixture.repository, || {
                        settle_one_item_locked(
                            &store,
                            &active,
                            &completed,
                            run.run_id,
                            &run.target_ref,
                            &fixture.repository,
                            &fixture.sandbox_base,
                            &first,
                            false,
                        )
                    })
                    .unwrap()
                });
                fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
                assert_eq!(
                    fixture
                        .store
                        .get_source_worktree_settlement_run(run.run_id)
                        .unwrap()
                        .unwrap()
                        .counts
                        .settled,
                    1
                );
                if reason == "disabled" {
                    runtime
                        .source_worktree_auto_settlement_enabled
                        .store(false, Ordering::Relaxed);
                }
                settlement_disk_samples().lock().unwrap().insert(
                    fixture.sandbox_base.clone(),
                    (
                        if reason == "pressure-cleared" {
                            u64::MAX
                        } else {
                            0
                        },
                        192 * 1024 * 1024 * 1024,
                    ),
                );
            }
            let base = fixture.sandbox_base.clone();
            // Keep periodic workflows quiescent while testing recovery policy.
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            let manager = executor
                .block_on(async {
                    SessionManager::new(
                        Arc::new(crate::bus::EventBus::new(16)),
                        std::mem::replace(
                            &mut fixture.store,
                            crate::store::Store::open_in_memory().unwrap(),
                        ),
                        false,
                        fixture._directory.path().join("daemon.sock"),
                        None,
                        Vec::new(),
                        runtime,
                        base.clone(),
                    )
                })
                .unwrap();
            executor
                .block_on(async {
                    // Drain the watcher's immediate first tick before recovery.
                    tokio::task::yield_now().await;
                    manager.recover_source_worktree_settlements().await
                })
                .unwrap_or_else(|error| panic!("startup recovery after {reason}: {error}"));
            let receipt = executor
                .block_on(manager.get_source_worktree_settlement_run(run.run_id))
                .unwrap()
                .unwrap();
            assert_eq!(receipt.attribution, Some(attribution));
            assert_eq!(
                receipt.counts.settled,
                if reason == "agent-revoked" { 0 } else { 1 }
            );
            assert_eq!(
                receipt.counts.unattempted,
                if reason == "agent-revoked" { 1 } else { 5 }
            );
            for item in &receipt.items {
                if item.phase == SourceWorktreeSettlementPhaseV1::Unattempted {
                    let root = base.join(item.session_id.to_string());
                    assert!(&root.is_dir(), "{reason}");
                    assert_eq!(
                        git(&fixture.repository, &["rev-parse", &item.source_ref]),
                        item.expected_source_oid.as_str()
                    );
                    assert!(
                        !executor
                            .block_on(async {
                                manager
                                    .store
                                    .lock()
                                    .await
                                    .path_is_inside_settlement_root(&root)
                            })
                            .unwrap(),
                        "terminal unattempted roots release the journal fence"
                    );
                }
            }
            executor
                .block_on(manager.recover_source_worktree_settlements())
                .unwrap();
            assert_eq!(
                executor
                    .block_on(manager.get_source_worktree_settlement_run(run.run_id))
                    .unwrap()
                    .unwrap(),
                receipt,
                "repeated startup retains the exact outcome"
            );
            settlement_disk_samples().lock().unwrap().remove(&base);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn startup_recovery_finishes_effect_started_automatic_and_revoked_agent_items() {
        use rsi_common::cohort_settlement::{
            SourceWorktreeSettlementAttributionV1, SourceWorktreeSettlementOriginV1,
        };
        use std::sync::atomic::Ordering;
        for origin in [
            SourceWorktreeSettlementOriginV1::Automatic,
            SourceWorktreeSettlementOriginV1::Agent,
        ] {
            let mut fixture = IntegratedFixture::new("startup-effect-started");
            let grant = (origin == SourceWorktreeSettlementOriginV1::Agent)
                .then(|| grant_storage_manager(&fixture));
            let attribution = SourceWorktreeSettlementAttributionV1 {
                origin,
                agent_session_id: grant.map(|(caller, _)| caller),
            };
            let run = fixture.run_from_audit(&fixture.audit(), "startup:effect-started");
            fixture
                .store
                .insert_source_worktree_settlement_run_as(&run, &attribution)
                .unwrap();
            let quarantine = fixture.move_to_quarantine(&run);
            // Crash after the first move, before the process-proof marker.
            // Recovery reproves the real process inventory rather than
            // comparing it with a fixture marker from the synthetic inventory.
            assert_eq!(
                fixture.journal_item(run.run_id).phase,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "the first filesystem effect precedes a journal phase advance"
            );
            if let Some((_, project)) = grant {
                let mut grant = fixture
                    .store
                    .get_harness_manager_policy(project)
                    .unwrap()
                    .unwrap();
                grant.policy.capabilities.clear();
                fixture
                    .store
                    .configure_harness_manager_policy(
                        &rsi_common::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2 {
                            project_id: project,
                            expected_scope_version: grant.scope_version,
                            expected_policy_version: grant.row_version,
                            policy: grant.policy,
                            idempotency_key: "startup:revoke-effect-started".into(),
                        },
                    )
                    .unwrap();
            }
            let runtime =
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
            runtime
                .source_worktree_auto_settlement_enabled
                .store(false, Ordering::Relaxed);
            let executor = paused_settlement_runtime();
            let manager = executor
                .block_on(async {
                    SessionManager::new(
                        Arc::new(crate::bus::EventBus::new(16)),
                        std::mem::replace(
                            &mut fixture.store,
                            crate::store::Store::open_in_memory().unwrap(),
                        ),
                        false,
                        fixture._directory.path().join("daemon.sock"),
                        None,
                        Vec::new(),
                        runtime,
                        fixture.sandbox_base.clone(),
                    )
                })
                .unwrap();
            executor
                .block_on(async {
                    drain_background_startup_tick().await;
                    manager.recover_source_worktree_settlements().await
                })
                .unwrap();
            let receipt = executor
                .block_on(manager.get_source_worktree_settlement_run(run.run_id))
                .unwrap()
                .unwrap();
            assert_eq!(receipt.attribution, Some(attribution));
            assert_eq!(receipt.counts.settled, 1, "{receipt:#?}");
            assert_eq!(receipt.counts.unattempted, 0);
            assert!(!quarantine.exists());
            let retained = executor
                .block_on(async { manager.store.lock().await.get_session(fixture.session_id) })
                .unwrap()
                .unwrap();
            assert_eq!(
                retained.sandbox_cleanup_state,
                Some(SandboxCleanupState::Purged)
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn automatic_policy_revocation_retains_committed_intent_and_prevents_fresh_intent() {
        use std::sync::atomic::Ordering;
        let mut fixture = IntegratedFixture::new("automatic-revocation");
        let audit = fixture.audit();
        let run = fixture.commit_intent("automatic:revocation");
        let item = fixture
            .store
            .list_source_worktree_settlement_journal_items(run.run_id)
            .unwrap()
            .remove(0);
        let receipt = fixture
            .store
            .get_source_worktree_settlement_run(run.run_id)
            .unwrap()
            .unwrap();
        let store = Arc::new(tokio::sync::Mutex::new(fixture.store));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        runtime
            .source_worktree_auto_settlement_enabled
            .store(false, Ordering::Relaxed);
        let error = git_worktree::with_repository_mutation(&fixture.repository, || {
            settle_one_item_with_agent_authority(
                &store,
                &active,
                &completed,
                run.run_id,
                &run.target_ref,
                &fixture.repository,
                &fixture.sandbox_base,
                &item,
                true,
                None,
                Some(&runtime),
            )
        })
        .unwrap_err();
        assert!(
            matches!(error, DaemonError::StructuredRpc { data, .. } if data["code"] == "storage_control_automatic_settlement_not_due")
        );
        assert!(fixture.allocation.root.is_dir());
        assert_eq!(
            store
                .blocking_lock()
                .get_source_worktree_settlement_run(run.run_id)
                .unwrap()
                .unwrap(),
            receipt
        );
        let error = git_worktree::with_repository_mutation(&fixture.repository, || {
            apply_locked_with_agent_authority(
                ApplyStart::Fresh(audit),
                &run.repository_identity,
                "automatic:new-disabled",
                &run.plan_digest,
                &run.authorization_digest,
                &run.request_fingerprint,
                &fixture.sandbox_base,
                &store,
                &active,
                &completed,
                false,
                None,
                Some(&runtime),
            )
        })
        .err()
        .unwrap();
        assert!(
            matches!(error, DaemonError::StructuredRpc { data, .. } if data["code"] == "storage_control_automatic_settlement_not_due")
        );
        assert_eq!(
            store
                .blocking_lock()
                .list_source_worktree_settlement_runs(20)
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn agent_list_audit_apply_scopes_mixed_roots_and_preserves_deleted_live_consumers() {
        for live_consumer in [false, true] {
            let mut fixture = IntegratedFixture::new("agent-mixed-roots");
            for index in 0..4 {
                fixture.add_second_root(&format!("mixed-{index}"));
            }
            let (caller, own) = grant_storage_manager(&fixture);
            let others = fixture
                .inventory()
                .into_iter()
                .filter_map(|r| r.session_id)
                .filter(|id| *id != fixture.session_id)
                .collect::<Vec<_>>();
            let foreign_live = others[0];
            let foreign_deleted = others[1];
            let unknown = others[2];
            let deleted = others[3];
            fixture
                .store
                .conn
                .execute(
                    "UPDATE sessions SET project_id=NULL WHERE id=?1",
                    [unknown.to_string()],
                )
                .unwrap();
            let foreign = Uuid::new_v4();
            let now = chrono::Utc::now();
            fixture
                .store
                .insert_project(&rsi_common::types::Project {
                    id: foreign,
                    name: foreign.to_string(),
                    path: None,
                    description: None,
                    color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
            fixture
                .store
                .conn
                .execute(
                    "UPDATE sessions SET project_id=?1,status='Running' WHERE id=?2",
                    rusqlite::params![foreign.to_string(), foreign_live.to_string()],
                )
                .unwrap();
            fixture
                .store
                .conn
                .execute(
                    "UPDATE sessions SET project_id=?1,status='Deleted' WHERE id=?2",
                    rusqlite::params![foreign.to_string(), foreign_deleted.to_string()],
                )
                .unwrap();
            fixture
                .store
                .conn
                .execute(
                    "UPDATE sessions SET project_id=?1,status='Deleted' WHERE id=?2",
                    rusqlite::params![own.to_string(), deleted.to_string()],
                )
                .unwrap();
            let deleted_root = fixture.sandbox_base.join(deleted.to_string());
            if live_consumer {
                let mut consumer = rsid_store::test_support::make_test_session();
                consumer.project_id = Some(own);
                consumer.status = SessionStatus::Running;
                consumer.working_dir = deleted_root.clone();
                fixture.store.insert_session(&consumer).unwrap();
            }
            let operator_digest = fixture.audit().report.plan_digest;
            let identity = fixture.target.repository_identity.clone();
            #[cfg(target_os = "linux")]
            let _proc = crate::session::reaper::scoped_archive_cleanup_test_proc(
                fixture.session_id,
                &fixture.sandbox_base,
            );
            // Keep the workflow watcher's periodic Store reads out of the
            // effect-time lock proof. Paused time does not advance while
            // settlement's spawn_blocking work is running.
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            let _runtime_context = executor.enter();
            let manager = Arc::new(
                SessionManager::new(
                    Arc::new(crate::bus::EventBus::new(16)),
                    std::mem::replace(
                        &mut fixture.store,
                        crate::store::Store::open_in_memory().unwrap(),
                    ),
                    false,
                    fixture._directory.path().join("daemon.sock"),
                    None,
                    Vec::new(),
                    crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                    fixture.sandbox_base.clone(),
                )
                .unwrap(),
            );
            manager
                .topology_agent_self
                .set(Arc::downgrade(&manager))
                .unwrap();
            executor.block_on(async {
                // Finish the watcher's immediate first tick before auditing.
                tokio::task::yield_now().await;
                let control = manager.agent_control();
                let listed = control
                    .agent_list_source_worktree_cohorts(caller)
                    .await
                    .unwrap();
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0].repository_identity, identity);
                let audit = control
                    .agent_audit_source_worktree_cohort(
                        caller,
                        rsi_common::cohort_settlement::AuditSourceWorktreeCohortParams {
                            repository_identity: identity.clone(),
                        },
                    )
                    .await
                    .unwrap();
                let disposition = |id| {
                    audit
                        .items
                        .iter()
                        .find(|i| i.session_id == id)
                        .unwrap()
                        .disposition
                        .clone()
                };
                assert_eq!(
                    disposition(foreign_live),
                    SourceWorktreeDispositionV1::RetainedForeignOwner
                );
                assert_eq!(
                    disposition(foreign_deleted),
                    SourceWorktreeDispositionV1::RetainedForeignOwner
                );
                assert_eq!(
                    disposition(unknown),
                    SourceWorktreeDispositionV1::RetainedUnknownOwner
                );
                assert_eq!(
                    disposition(deleted),
                    if live_consumer {
                        SourceWorktreeDispositionV1::SessionPathDependency
                    } else {
                        SourceWorktreeDispositionV1::EligibleIntegratedAncestor
                    }
                );
                assert_ne!(
                    audit.plan_digest, operator_digest,
                    "agent exclusion is bound into the plan"
                );
                let summary = control
                    .agent_audit_source_worktree_cohort_view(
                        caller,
                        rsi_common::cohort_settlement::AgentAuditSourceWorktreeCohortParams {
                            repository_identity: identity.clone(),
                            fields: Some(rsi_common::cohort_settlement::AgentSourceWorktreeAuditFieldsV1::Summary),
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(summary["view"], "summary");
                assert_eq!(summary["counts"]["observed"], 5);
                assert_eq!(summary["items"].as_array().unwrap().len(), audit.counts.eligible as usize);
                assert_eq!(summary["plan_digest"], serde_json::json!(audit.plan_digest));
                let params = ApplySourceWorktreeCohortParams {
                    repository_identity: identity.clone(),
                    plan_digest: serde_json::from_value(summary["plan_digest"].clone()).unwrap(),
                    authorization: summary["authorization_phrase"].as_str().unwrap().to_owned(),
                    idempotency_key: "agent:mixed".into(),
                };
                let receipt = agent_apply_to_completion(&manager, caller, params.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    receipt.counts.settled,
                    if live_consumer { 1 } else { 2 },
                    "mixed-root settlement receipt (live_consumer={live_consumer}): {receipt:#?}"
                );
                assert_eq!(receipt.counts.retained, if live_consumer { 4 } else { 3 });
                assert_eq!(
                    control
                        .agent_apply_source_worktree_cohort(caller, params)
                        .await
                        .unwrap(),
                    receipt
                );
                let store = manager.store.lock().await;
                let retained_foreign = store.get_session(foreign_deleted).unwrap().unwrap();
                assert_eq!(retained_foreign.status, SessionStatus::Deleted);
                assert_eq!(
                    retained_foreign.sandbox_cleanup_state,
                    Some(SandboxCleanupState::Live)
                );
                assert_eq!(
                    retained_foreign.sandbox_root,
                    Some(fixture.sandbox_base.join(foreign_deleted.to_string()))
                );
                let session = store.get_session(deleted).unwrap().unwrap();
                assert_eq!(session.status, SessionStatus::Deleted);
                assert_eq!(
                    session.sandbox_cleanup_state,
                    Some(if live_consumer {
                        SandboxCleanupState::Live
                    } else {
                        SandboxCleanupState::Purged
                    })
                );
                if live_consumer {
                    assert_eq!(session.sandbox_root, Some(deleted_root.clone()));
                } else {
                    assert_eq!(session.sandbox_root, None);
                    assert_eq!(session.sandbox_branch, None);
                }
            });
            // Operator/automatic inventory carries no agent exclusions and
            // still admits the foreign Deleted orphan after the same proofs.
            let operator_audit = executor
                .block_on(manager.audit_source_worktree_cohort(identity.clone()))
                .unwrap();
            assert_eq!(
                operator_audit
                    .items
                    .iter()
                    .find(|i| i.session_id == foreign_deleted)
                    .unwrap()
                    .disposition,
                SourceWorktreeDispositionV1::EligibleIntegratedAncestor
            );
            assert!(
                fixture
                    .sandbox_base
                    .join(foreign_deleted.to_string())
                    .is_dir()
            );
            assert!(fixture.sandbox_base.join(foreign_live.to_string()).is_dir());
            assert!(fixture.sandbox_base.join(unknown.to_string()).is_dir());
            assert_eq!(deleted_root.is_dir(), live_consumer);
        }
    }

    /// #2789: an agent apply over a whole cohort answers as soon as the run is
    /// durably recorded, while the detached driver keeps settling. A retry in
    /// that window returns the same running run and starts no second driver,
    /// and the finished receipt is what a later retry reads.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_apply_returns_the_running_run_and_a_retry_starts_no_second_driver() {
        let mut fixture = IntegratedFixture::new("agent-detached-apply");
        for index in 1..3 {
            fixture.add_second_root(&format!("detached-apply-{index}"));
        }
        let (caller, project) = grant_storage_manager(&fixture);
        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET project_id=?1 WHERE sandbox_custody_id IS NOT NULL",
                [project.to_string()],
            )
            .unwrap();
        #[cfg(target_os = "linux")]
        let _proc = crate::session::reaper::scoped_archive_cleanup_test_proc(
            fixture.session_id,
            &fixture.sandbox_base,
        );
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                std::mem::replace(
                    &mut fixture.store,
                    crate::store::Store::open_in_memory().unwrap(),
                ),
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                fixture.sandbox_base.clone(),
            )
            .unwrap(),
        );
        manager
            .topology_agent_self
            .set(Arc::downgrade(&manager))
            .unwrap();
        tokio::task::yield_now().await;
        let control = manager.agent_control();
        let identity = fixture.target.repository_identity.clone();
        let audit = control
            .agent_audit_source_worktree_cohort(
                caller,
                rsi_common::cohort_settlement::AuditSourceWorktreeCohortParams {
                    repository_identity: identity.clone(),
                },
            )
            .await
            .unwrap();
        assert_eq!(audit.counts.eligible, 3, "{audit:#?}");
        // Hold every root at its proof: the driver cannot finish until released.
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let mut releases = Vec::new();
        for item in audit.items.iter().filter(|item| {
            item.disposition == SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        }) {
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            SETTLEMENT_PROOF_PAUSE.lock().unwrap().insert(
                item.session_id,
                SettlementProofPause {
                    reached: reached_tx.clone(),
                    release: release_rx,
                },
            );
            releases.push(release_tx);
        }
        let params = ApplySourceWorktreeCohortParams {
            repository_identity: identity,
            plan_digest: audit.plan_digest,
            authorization: audit.authorization_phrase.unwrap(),
            idempotency_key: "agent:detached".into(),
        };
        let started = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            control.agent_apply_source_worktree_cohort(caller, params.clone()),
        )
        .await
        .expect("the apply answers while the run is still settling")
        .unwrap();
        assert!(
            started.finished_at.is_none(),
            "answered before the run finished: {started:#?}"
        );
        assert_eq!(started.counts.eligible, 3);
        assert_eq!(started.counts.settled, 0);
        assert!(settlement_run_in_flight(started.run_id));
        tokio::task::spawn_blocking(move || {
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("the driver reached its first root")
        })
        .await
        .unwrap();

        let again = control
            .agent_apply_source_worktree_cohort(caller, params.clone())
            .await
            .unwrap();
        assert_eq!(again.run_id, started.run_id);
        assert!(again.finished_at.is_none(), "{again:#?}");

        for release in &releases {
            let _ = release.send(());
        }
        await_run_driver_finished(started.run_id).await;
        let finished = manager
            .get_source_worktree_settlement_run(started.run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(finished.finished_at.is_some(), "{finished:#?}");
        assert_eq!(finished.counts.settled, 3, "{finished:#?}");
        assert_eq!(
            control
                .agent_apply_source_worktree_cohort(caller, params)
                .await
                .unwrap(),
            finished,
            "a retry after the run finished returns the finished receipt"
        );
    }

    /// #2866: the detached apply returns once the audit that binds the plan
    /// digest has recorded the run, however long a root's spawn guard stays
    /// held. The run settles after the guard is released, a replay by
    /// idempotency key returns the same run id throughout, and a stale digest
    /// or a cohort with nothing left to apply is a typed refusal.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_apply_returns_while_a_root_spawn_guard_is_held_and_settles_after_release() {
        let mut fixture = IntegratedFixture::new("agent-detached-guard");
        for index in 1..3 {
            fixture.add_second_root(&format!("detached-guard-{index}"));
        }
        let (caller, project) = grant_storage_manager(&fixture);
        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET project_id=?1 WHERE sandbox_custody_id IS NOT NULL",
                [project.to_string()],
            )
            .unwrap();
        #[cfg(target_os = "linux")]
        let _proc = crate::session::reaper::scoped_archive_cleanup_test_proc(
            fixture.session_id,
            &fixture.sandbox_base,
        );
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                std::mem::replace(
                    &mut fixture.store,
                    crate::store::Store::open_in_memory().unwrap(),
                ),
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                fixture.sandbox_base.clone(),
            )
            .unwrap(),
        );
        manager
            .topology_agent_self
            .set(Arc::downgrade(&manager))
            .unwrap();
        tokio::task::yield_now().await;
        let control = manager.agent_control();
        let identity = fixture.target.repository_identity.clone();
        let audit = control
            .agent_audit_source_worktree_cohort(
                caller,
                rsi_common::cohort_settlement::AuditSourceWorktreeCohortParams {
                    repository_identity: identity.clone(),
                },
            )
            .await
            .unwrap();
        assert_eq!(audit.counts.eligible, 3, "{audit:#?}");
        let held_session = audit
            .items
            .iter()
            .find(|item| {
                item.disposition == SourceWorktreeDispositionV1::EligibleIntegratedAncestor
            })
            .expect("an eligible root")
            .session_id;
        let held_guard = super::super::spawn_single_flight::acquire_spawn_guard(held_session).await;
        let params = ApplySourceWorktreeCohortParams {
            repository_identity: identity.clone(),
            plan_digest: audit.plan_digest.clone(),
            authorization: audit.authorization_phrase.clone().unwrap(),
            idempotency_key: "agent:detached-guard".into(),
        };

        let started = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            control.agent_apply_source_worktree_cohort(caller, params.clone()),
        )
        .await
        .expect("the apply answers while a root's spawn guard is held")
        .unwrap();
        assert!(started.finished_at.is_none(), "{started:#?}");
        assert_eq!(started.counts.eligible, 3, "{started:#?}");
        assert_eq!(started.counts.settled, 0, "{started:#?}");
        assert!(settlement_run_in_flight(started.run_id));

        // The run cannot start its first effect while the guard is held.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let pending = manager
            .get_source_worktree_settlement_run(started.run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(pending.finished_at.is_none(), "{pending:#?}");
        assert_eq!(pending.counts.settled, 0, "{pending:#?}");
        let replay = control
            .agent_apply_source_worktree_cohort(caller, params.clone())
            .await
            .unwrap();
        assert_eq!(replay.run_id, started.run_id, "{replay:#?}");

        let mut stale = params.clone();
        stale.plan_digest = Sha256Digest::parse(format!("sha256:{}", "0".repeat(64))).unwrap();
        stale.authorization = format!("APPLY {identity} {}", stale.plan_digest.as_str());
        stale.idempotency_key = "agent:detached-stale".into();
        let refusal = control
            .agent_apply_source_worktree_cohort(caller, stale)
            .await
            .expect_err("a stale digest is refused, not recorded");
        assert!(
            format!("{refusal:?}").contains("source-worktree audit digest is stale"),
            "typed stale-digest refusal: {refusal:?}"
        );

        drop(held_guard);
        await_run_driver_finished(started.run_id).await;
        let finished = manager
            .get_source_worktree_settlement_run(started.run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(finished.finished_at.is_some(), "{finished:#?}");
        assert_eq!(finished.counts.settled, 3, "{finished:#?}");
        assert_eq!(
            control
                .agent_apply_source_worktree_cohort(caller, params.clone())
                .await
                .unwrap()
                .run_id,
            started.run_id,
            "a replay after the run settled returns the same run"
        );

        let mut nothing_left = params;
        nothing_left.idempotency_key = "agent:detached-empty".into();
        control
            .agent_apply_source_worktree_cohort(caller, nothing_left)
            .await
            .expect_err("a settled cohort has nothing to apply");
    }

    /// #2869: a detached apply whose whole-cohort audit outlasts the caller's
    /// budget (the repository is held by another operation) answers the typed
    /// `source_settlement_apply_pending` within the budget and writes nothing,
    /// while the audit task keeps going. An exact retry by idempotency key gets
    /// the stale-digest refusal that audit ended with, or, for a matching
    /// digest, the one run the same task recorded and settles.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_apply_answers_pending_while_the_audit_outlasts_the_budget() {
        let mut fixture = IntegratedFixture::new("agent-detached-slow-audit");
        for index in 1..3 {
            fixture.add_second_root(&format!("detached-slow-audit-{index}"));
        }
        let (caller, project) = grant_storage_manager(&fixture);
        fixture
            .store
            .conn
            .execute(
                "UPDATE sessions SET project_id=?1 WHERE sandbox_custody_id IS NOT NULL",
                [project.to_string()],
            )
            .unwrap();
        #[cfg(target_os = "linux")]
        let _proc = crate::session::reaper::scoped_archive_cleanup_test_proc(
            fixture.session_id,
            &fixture.sandbox_base,
        );
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                std::mem::replace(
                    &mut fixture.store,
                    crate::store::Store::open_in_memory().unwrap(),
                ),
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                fixture.sandbox_base.clone(),
            )
            .unwrap(),
        );
        manager
            .topology_agent_self
            .set(Arc::downgrade(&manager))
            .unwrap();
        tokio::task::yield_now().await;
        let control = manager.agent_control();
        let identity = fixture.target.repository_identity.clone();
        let audit = control
            .agent_audit_source_worktree_cohort(
                caller,
                rsi_common::cohort_settlement::AuditSourceWorktreeCohortParams {
                    repository_identity: identity.clone(),
                },
            )
            .await
            .unwrap();
        assert_eq!(audit.counts.eligible, 3, "{audit:#?}");

        // Another operation holds the repository, so the apply's audit cannot
        // finish: the same shape as a slow audit, without a test hook.
        let hold_repository = || {
            let repository = fixture.repository.clone();
            let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let holder = std::thread::spawn(move || {
                git_worktree::with_repository_mutation(&repository, || {
                    held_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(std::time::Duration::from_secs(60));
                    Ok(())
                })
            });
            held_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the repository is held");
            (release_tx, holder)
        };
        let budget_ceiling = AGENT_APPLY_AUDIT_BUDGET + std::time::Duration::from_secs(3);
        let pending_code = |error: &DaemonError| match error {
            DaemonError::StructuredRpc { data, .. } => data["code"].as_str().map(str::to_string),
            _ => None,
        };

        // A stale digest ends the audit task before any effect; the retry
        // receives that refusal, not a run.
        let mut stale = ApplySourceWorktreeCohortParams {
            repository_identity: identity.clone(),
            plan_digest: Sha256Digest::parse(format!("sha256:{}", "0".repeat(64))).unwrap(),
            authorization: String::new(),
            idempotency_key: "agent:detached-slow-stale".into(),
        };
        stale.authorization = format!("APPLY {identity} {}", stale.plan_digest.as_str());
        let (release, holder) = hold_repository();
        let started = std::time::Instant::now();
        let pending = tokio::time::timeout(
            budget_ceiling,
            control.agent_apply_source_worktree_cohort(caller, stale.clone()),
        )
        .await
        .expect("the apply answers within the budget while the audit is blocked")
        .expect_err("no run exists while the audit has not finished");
        assert!(
            started.elapsed() >= AGENT_APPLY_AUDIT_BUDGET,
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            pending_code(&pending).as_deref(),
            Some("source_settlement_apply_pending"),
            "{pending:?}"
        );
        release.send(()).unwrap();
        holder.join().unwrap().unwrap();
        let refusal = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            control.agent_apply_source_worktree_cohort(caller, stale.clone()),
        )
        .await
        .expect("the retry joins the audit task")
        .expect_err("the stale digest is refused, not recorded");
        assert!(
            format!("{refusal:?}").contains("source-worktree audit digest is stale"),
            "typed stale-plan outcome: {refusal:?}"
        );

        // A matching digest: the same task records the run once the audit ends.
        let params = ApplySourceWorktreeCohortParams {
            repository_identity: identity.clone(),
            plan_digest: audit.plan_digest.clone(),
            authorization: audit.authorization_phrase.clone().unwrap(),
            idempotency_key: "agent:detached-slow-audit".into(),
        };
        let (release, holder) = hold_repository();
        let pending = tokio::time::timeout(
            budget_ceiling,
            control.agent_apply_source_worktree_cohort(caller, params.clone()),
        )
        .await
        .expect("the apply answers within the budget while the audit is blocked")
        .expect_err("no run exists while the audit has not finished");
        assert_eq!(
            pending_code(&pending).as_deref(),
            Some("source_settlement_apply_pending"),
            "{pending:?}"
        );
        let mut different = params.clone();
        different.plan_digest = Sha256Digest::parse(format!("sha256:{}", "1".repeat(64))).unwrap();
        different.authorization = format!("APPLY {identity} {}", different.plan_digest.as_str());
        let mismatch = control
            .agent_apply_source_worktree_cohort(caller, different)
            .await
            .expect_err("the pending key belongs to a different request");
        assert!(
            format!("{mismatch:?}").contains("idempotency key already belongs"),
            "{mismatch:?}"
        );
        release.send(()).unwrap();
        holder.join().unwrap().unwrap();
        let started = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                match control
                    .agent_apply_source_worktree_cohort(caller, params.clone())
                    .await
                {
                    Ok(run) => break run,
                    Err(error) => assert_eq!(
                        pending_code(&error).as_deref(),
                        Some("source_settlement_apply_pending"),
                        "{error:?}"
                    ),
                }
            }
        })
        .await
        .expect("the audit task records the run");
        assert_eq!(started.counts.eligible, 3, "{started:#?}");
        await_run_driver_finished(started.run_id).await;
        let finished = manager
            .get_source_worktree_settlement_run(started.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(finished.counts.settled, 3, "{finished:#?}");
        assert_eq!(
            control
                .agent_apply_source_worktree_cohort(caller, params)
                .await
                .unwrap()
                .run_id,
            started.run_id,
            "a replay by idempotency key returns the same run"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn agent_authority_loss_mid_run_closes_remaining_fresh_and_replayed_intents() {
        for replaying in [false, true] {
            let mut fixture = IntegratedFixture::new("agent-mid-run-revocation");
            for index in 1..4 {
                fixture.add_second_root(&format!("revocation-{index}"));
            }
            let (caller, project) = grant_storage_manager(&fixture);
            fixture
                .store
                .conn
                .execute(
                    "UPDATE sessions SET project_id=?1 WHERE sandbox_custody_id IS NOT NULL",
                    [project.to_string()],
                )
                .unwrap();
            let audit = fixture.audit();
            let digest = audit.report.plan_digest.as_str().to_string();
            let template = fixture.run_from_audit(&audit, "agent:mid-run");
            let start = if replaying {
                let run = fixture.commit_intent_for_all_eligible_as(
                    "agent:mid-run",
                    &rsi_common::cohort_settlement::SourceWorktreeSettlementAttributionV1 {
                        origin:
                            rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Agent,
                        agent_session_id: Some(caller),
                    },
                );
                ApplyStart::Replay(
                    fixture
                        .store
                        .get_source_worktree_settlement_run(run.run_id)
                        .unwrap()
                        .unwrap(),
                )
            } else {
                ApplyStart::Fresh(audit)
            };
            // Revoke after the first successful item, before the next effect-time check.
            fixture.store.conn.execute_batch("CREATE TEMP TRIGGER revoke_storage_after_settled
                AFTER UPDATE OF phase ON source_worktree_settlement_items WHEN NEW.phase='settled'
                BEGIN UPDATE harness_manager_v2_policies SET policy_json=json_set(policy_json,'$.paused',json('true')); END;").unwrap();
            let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
                &mut fixture.store,
                crate::store::Store::open_in_memory().unwrap(),
            )));
            let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let outcome = fixture
                .with_quarantine_holder_test_proc(|| {
                    git_worktree::with_repository_mutation(&fixture.repository, || {
                        apply_locked_with_agent_authority(
                            start,
                            &template.repository_identity,
                            &template.idempotency_key,
                            &digest,
                            &template.authorization_digest,
                            &template.request_fingerprint,
                            &fixture.sandbox_base,
                            &store,
                            &active,
                            &completed,
                            false,
                            Some(caller),
                            None,
                        )
                    })
                })
                .unwrap();
            assert_eq!(outcome.receipt.counts.settled, 1);
            assert_eq!(outcome.receipt.counts.unattempted, 3);
            assert!(outcome.receipt.items.iter().all(|i| i.phase.is_terminal()));
            for item in outcome
                .receipt
                .items
                .iter()
                .filter(|i| i.phase == SourceWorktreeSettlementPhaseV1::Unattempted)
            {
                let root = fixture.sandbox_base.join(item.session_id.to_string());
                assert!(root.is_dir());
                assert_eq!(
                    git(&fixture.repository, &["rev-parse", &item.source_ref]),
                    item.expected_source_oid.as_str()
                );
                assert!(
                    !store
                        .blocking_lock()
                        .path_is_inside_settlement_root(&root)
                        .unwrap()
                );
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn startup_git_read_failure_closes_one_untouched_item_and_continues_without_degradation() {
        let mut fixture = IntegratedFixture::new("startup-git-read-error");
        fixture.add_second_root("startup-good-item");
        let run = fixture.commit_intent_for_all_eligible_as(
            "startup:git-read-error",
            &rsi_common::cohort_settlement::SourceWorktreeSettlementAttributionV1 {
                origin: rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Automatic,
                agent_session_id: None,
            },
        );
        SETTLEMENT_RECOVERY_PROBE_FAILURE
            .lock()
            .unwrap()
            .insert(run.items[0].session_id);
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        runtime
            .source_worktree_auto_settlement_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let executor = paused_settlement_runtime();
        let _runtime_context = executor.enter();
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            std::mem::replace(
                &mut fixture.store,
                crate::store::Store::open_in_memory().unwrap(),
            ),
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            runtime,
            fixture.sandbox_base.clone(),
        )
        .unwrap();
        executor
            .block_on(async {
                drain_background_startup_tick().await;
                manager.recover_source_worktree_settlements().await
            })
            .unwrap();
        let receipt = executor
            .block_on(manager.get_source_worktree_settlement_run(run.run_id))
            .unwrap()
            .unwrap();
        assert_eq!(receipt.counts.unattempted, 2);
        assert_eq!(receipt.counts.recovery_required, 0);
        for item in &receipt.items {
            let root = fixture.sandbox_base.join(item.session_id.to_string());
            assert!(root.is_dir());
            assert_eq!(
                git(&fixture.repository, &["rev-parse", &item.source_ref]),
                item.expected_source_oid.as_str()
            );
            assert!(
                !executor
                    .block_on(async {
                        manager
                            .store
                            .lock()
                            .await
                            .path_is_inside_settlement_root(&root)
                    })
                    .unwrap()
            );
        }
        executor
            .block_on(manager.recover_source_worktree_settlements())
            .unwrap();
        assert_eq!(
            executor
                .block_on(manager.get_source_worktree_settlement_run(run.run_id))
                .unwrap()
                .unwrap(),
            receipt
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn agent_apply_reuses_ancestor_proof_and_receipt_on_replay() {
        let mut fixture = IntegratedFixture::new("agent-apply-replay");
        let (caller, _) = grant_storage_manager(&fixture);
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "agent-apply-replay");
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().unwrap(),
        )));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let execute = |start| {
            fixture.with_quarantine_holder_test_proc(|| {
                git_worktree::with_repository_mutation(&fixture.repository, || {
                    apply_locked_with_agent_authority(
                        start,
                        &run.repository_identity,
                        &run.idempotency_key,
                        &run.plan_digest,
                        &run.authorization_digest,
                        &run.request_fingerprint,
                        &fixture.sandbox_base,
                        &store,
                        &active,
                        &completed,
                        false,
                        Some(caller),
                        None,
                    )
                })
            })
        };
        let receipt = execute(ApplyStart::Fresh(audit)).unwrap().receipt;
        assert_eq!(
            receipt.attribution.as_ref().unwrap().origin,
            rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Agent
        );
        assert_eq!(
            receipt.attribution.as_ref().unwrap().agent_session_id,
            Some(caller)
        );
        assert_eq!(receipt.counts.settled, 1);
        assert_eq!(
            receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled
        );
        assert!(!fixture.allocation.root.exists());
        assert_eq!(
            execute(ApplyStart::Replay(receipt.clone()))
                .unwrap()
                .receipt,
            receipt
        );
        store
            .blocking_lock()
            .authorize_agent_source_worktree_cohort(caller, &run.repository_identity)
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn run_record_keeps_retained_roots_with_their_class_after_a_mixed_apply() {
        // #2635: one pass that settles a root and retains a dirty root and a
        // non-ancestor root records the retained ones on the run itself.
        let mut fixture = IntegratedFixture::new("retained-snapshot");
        fixture.add_second_root("retained-dirty");
        fixture.add_second_root("retained-divergent");
        let roots: Vec<(Uuid, String)> = fixture
            .inventory()
            .into_iter()
            .filter(|row| row.session_id != Some(fixture.session_id))
            .map(|row| (row.session_id.unwrap(), row.sandbox_root))
            .collect();
        assert_eq!(roots.len(), 2);
        let (dirty_id, dirty_root) = roots[0].clone();
        let (divergent_id, divergent_root) = roots[1].clone();
        std::fs::write(std::path::Path::new(&dirty_root).join("tracked"), "dirty\n").unwrap();
        std::fs::write(
            std::path::Path::new(&divergent_root).join("tracked"),
            "divergent\n",
        )
        .unwrap();
        git(std::path::Path::new(&divergent_root), &["add", "tracked"]);
        git(
            std::path::Path::new(&divergent_root),
            &["commit", "-qm", "unintegrated source"],
        );
        let audit = fixture.audit();
        assert_eq!(audit.report.counts.eligible, 1);
        assert_eq!(audit.report.counts.retained, 2);
        let run = fixture.run_from_audit(&audit, "retained-snapshot");
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().unwrap(),
        )));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let receipt = fixture
            .with_quarantine_holder_test_proc(|| {
                git_worktree::with_repository_mutation(&fixture.repository, || {
                    apply_locked_with_agent_authority(
                        ApplyStart::Fresh(audit),
                        &run.repository_identity,
                        &run.idempotency_key,
                        &run.plan_digest,
                        &run.authorization_digest,
                        &run.request_fingerprint,
                        &fixture.sandbox_base,
                        &store,
                        &active,
                        &completed,
                        false,
                        None,
                        None,
                    )
                })
            })
            .unwrap()
            .receipt;
        assert_eq!(receipt.counts.settled, 1);
        assert_eq!(receipt.counts.retained, 2);
        assert_eq!(receipt.retained_roots.len(), 2);
        let by_session = |id: Uuid| {
            receipt
                .retained_roots
                .iter()
                .find(|root| root.session_id == id)
                .expect("retained root is on the run record")
        };
        assert_eq!(
            by_session(dirty_id).disposition,
            SourceWorktreeDispositionV1::DirtyWorktree
        );
        assert_eq!(
            by_session(divergent_id).disposition,
            SourceWorktreeDispositionV1::RetainedNonAncestor
        );
        assert_eq!(by_session(dirty_id).sandbox_root, dirty_root);
        assert!(
            receipt
                .retained_roots
                .iter()
                .all(|root| root.session_id != fixture.session_id)
        );
        // A later read of the stored run returns the same snapshot.
        let reread = store
            .blocking_lock()
            .get_source_worktree_settlement_run(receipt.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(reread.retained_roots, receipt.retained_roots);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn agent_audit_shows_the_runs_retained_snapshot_after_the_roots_changed() {
        // #2690: a later agent audit reclassifies the current roots; the run's
        // own retention snapshot stays beside them, scoped to the caller.
        let mut fixture = IntegratedFixture::new("agent-retained-snapshot");
        fixture.add_second_root("snapshot-dirty");
        fixture.add_second_root("snapshot-divergent");
        let (caller, own) = grant_storage_manager(&fixture);
        let roots: Vec<(Uuid, String)> = fixture
            .inventory()
            .into_iter()
            .filter(|row| row.session_id != Some(fixture.session_id))
            .map(|row| (row.session_id.unwrap(), row.sandbox_root))
            .collect();
        assert_eq!(roots.len(), 2);
        let (dirty_id, dirty_root) = roots[0].clone();
        let (divergent_id, divergent_root) = roots[1].clone();
        for id in [dirty_id, divergent_id] {
            fixture
                .store
                .conn
                .execute(
                    "UPDATE sessions SET project_id=?1 WHERE id=?2",
                    rusqlite::params![own.to_string(), id.to_string()],
                )
                .unwrap();
        }
        std::fs::write(std::path::Path::new(&dirty_root).join("tracked"), "dirty\n").unwrap();
        std::fs::write(
            std::path::Path::new(&divergent_root).join("tracked"),
            "divergent\n",
        )
        .unwrap();
        git(std::path::Path::new(&divergent_root), &["add", "tracked"]);
        git(
            std::path::Path::new(&divergent_root),
            &["commit", "-qm", "unintegrated source"],
        );
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "agent-retained-snapshot");
        let identity = fixture.target.repository_identity.clone();
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().unwrap(),
        )));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let receipt = fixture
            .with_quarantine_holder_test_proc(|| {
                git_worktree::with_repository_mutation(&fixture.repository, || {
                    apply_locked_with_agent_authority(
                        ApplyStart::Fresh(audit),
                        &run.repository_identity,
                        &run.idempotency_key,
                        &run.plan_digest,
                        &run.authorization_digest,
                        &run.request_fingerprint,
                        &fixture.sandbox_base,
                        &store,
                        &active,
                        &completed,
                        false,
                        None,
                        None,
                    )
                })
            })
            .unwrap()
            .receipt;
        assert_eq!(receipt.retained_roots.len(), 2);

        // The dirty root is cleaned after the run, so a fresh audit no longer
        // retains it as dirty; the run still recorded it as DirtyWorktree.
        git(
            std::path::Path::new(&dirty_root),
            &["checkout", "--", "tracked"],
        );
        fixture.store = Arc::try_unwrap(store)
            .ok()
            .expect("the apply released the Store")
            .into_inner();
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let _runtime_context = executor.enter();
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                std::mem::replace(
                    &mut fixture.store,
                    crate::store::Store::open_in_memory().unwrap(),
                ),
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                fixture.sandbox_base.clone(),
            )
            .unwrap(),
        );
        manager
            .topology_agent_self
            .set(Arc::downgrade(&manager))
            .unwrap();
        executor.block_on(async {
            tokio::task::yield_now().await;
            let control = manager.agent_control();
            let params = || rsi_common::cohort_settlement::AuditSourceWorktreeCohortParams {
                repository_identity: identity.clone(),
            };
            let audit = control
                .agent_audit_source_worktree_cohort(caller, params())
                .await
                .unwrap();
            assert_eq!(audit.run_id, Some(receipt.run_id));
            // The fresh classification moved on...
            let fresh = |id: Uuid| {
                audit
                    .items
                    .iter()
                    .find(|item| item.session_id == id)
                    .expect("fresh audit item")
                    .disposition
                    .clone()
            };
            assert_ne!(fresh(dirty_id), SourceWorktreeDispositionV1::DirtyWorktree);
            // ...while the snapshot keeps what the run retained and why.
            assert_eq!(audit.last_run_retained_roots.len(), 2);
            let kept = |id: Uuid| {
                audit
                    .last_run_retained_roots
                    .iter()
                    .find(|root| root.session_id == id)
                    .expect("snapshot root")
                    .disposition
                    .clone()
            };
            assert_eq!(kept(dirty_id), SourceWorktreeDispositionV1::DirtyWorktree);
            assert_eq!(
                kept(divergent_id),
                SourceWorktreeDispositionV1::RetainedNonAncestor
            );
            assert_eq!(
                audit.last_run_retained_roots, receipt.retained_roots,
                "the agent view is the run's own snapshot"
            );

            // A root the caller no longer covers drops out of the agent view.
            manager
                .store
                .lock()
                .await
                .conn
                .execute(
                    "UPDATE sessions SET project_id=NULL WHERE id=?1",
                    [divergent_id.to_string()],
                )
                .unwrap();
            let scoped = control
                .agent_audit_source_worktree_cohort(caller, params())
                .await
                .unwrap();
            assert_eq!(scoped.last_run_retained_roots.len(), 1);
            assert_eq!(scoped.last_run_retained_roots[0].session_id, dirty_id);
            assert_eq!(
                scoped.last_run_retained_roots[0].disposition,
                SourceWorktreeDispositionV1::DirtyWorktree
            );
            // The operator audit is unscoped.
            let operator = manager
                .audit_source_worktree_cohort(identity.clone())
                .await
                .unwrap();
            assert_eq!(operator.last_run_retained_roots, receipt.retained_roots);
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn agent_apply_retains_dirty_and_nonancestor_work_with_typed_reasons() {
        for divergent in [false, true] {
            let mut fixture = IntegratedFixture::new("agent-retains-unintegrated");
            let (caller, _) = grant_storage_manager(&fixture);
            std::fs::write(
                fixture.allocation.root.join("tracked"),
                "unintegrated work\n",
            )
            .unwrap();
            if divergent {
                git(&fixture.allocation.root, &["add", "tracked"]);
                git(
                    &fixture.allocation.root,
                    &["commit", "-qm", "unintegrated source"],
                );
            }
            let audit = fixture.audit();
            assert_eq!(
                audit.report.items[0].disposition,
                if divergent {
                    SourceWorktreeDispositionV1::RetainedNonAncestor
                } else {
                    SourceWorktreeDispositionV1::DirtyWorktree
                }
            );
            assert!(!audit.report.applyable);
            let digest = audit.report.plan_digest.as_str().to_string();
            let identity = fixture.target.repository_identity.clone();
            let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
                &mut fixture.store,
                crate::store::Store::open_in_memory().unwrap(),
            )));
            let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let error = git_worktree::with_repository_mutation(&fixture.repository, || {
                apply_locked_with_agent_authority(
                    ApplyStart::Fresh(audit),
                    &identity,
                    "agent:retains",
                    &digest,
                    &digest,
                    &digest,
                    &fixture.sandbox_base,
                    &store,
                    &active,
                    &completed,
                    false,
                    Some(caller),
                    None,
                )
            })
            .err()
            .expect("unintegrated work retained");
            assert!(
                matches!(normalize_agent_settlement_error(error), DaemonError::StructuredRpc { data, .. }
                if data["code"] == "storage_control_proof_refused")
            );
            assert!(fixture.allocation.root.exists());
            assert_eq!(
                std::fs::read_to_string(fixture.allocation.root.join("tracked")).unwrap(),
                "unintegrated work\n"
            );
            let count: i64 = store
                .blocking_lock()
                .conn
                .query_row(
                    "SELECT count(*) FROM source_worktree_settlement_runs",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 0);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn revoked_agent_cannot_commit_intent_or_remove_from_a_retained_intent() {
        let mut fixture = IntegratedFixture::new("agent-revoked-proof");
        let (caller, project) = grant_storage_manager(&fixture);
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "agent-revoked-proof");
        // Retained intent is for testing the effect-time check, which must
        // retain the root even if a caller was allowed when it was journaled.
        fixture
            .store
            .insert_source_worktree_settlement_run(&run)
            .unwrap();
        let item = fixture
            .store
            .list_source_worktree_settlement_journal_items(run.run_id)
            .unwrap()
            .pop()
            .unwrap();
        let mut grant = fixture
            .store
            .get_harness_manager_policy(project)
            .unwrap()
            .unwrap();
        grant.policy.paused = true;
        fixture
            .store
            .configure_harness_manager_policy(
                &rsi_common::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: project,
                    expected_scope_version: grant.scope_version,
                    expected_policy_version: grant.row_version,
                    policy: grant.policy,
                    idempotency_key: "fixture:storage-pause".into(),
                },
            )
            .unwrap();
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().unwrap(),
        )));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let error = git_worktree::with_repository_mutation(&fixture.repository, || {
            settle_one_item_with_agent_authority(
                &store,
                &active,
                &completed,
                run.run_id,
                &run.target_ref,
                &fixture.repository,
                &fixture.sandbox_base,
                &item,
                true,
                Some(caller),
                None,
            )
        })
        .unwrap_err();
        assert!(
            matches!(error, DaemonError::StructuredRpc { data, .. } if data["code"] == "storage_control_not_authorized")
        );
        let fresh_key = "fixture:revoked-fresh";
        let error = git_worktree::with_repository_mutation(&fixture.repository, || {
            apply_locked_with_agent_authority(
                ApplyStart::Fresh(audit),
                &run.repository_identity,
                fresh_key,
                &run.plan_digest,
                &run.authorization_digest,
                &run.request_fingerprint,
                &fixture.sandbox_base,
                &store,
                &active,
                &completed,
                false,
                Some(caller),
                None,
            )
        })
        .err()
        .expect("revoked fresh intent refused");
        assert!(
            matches!(error, DaemonError::StructuredRpc { data, .. } if data["code"] == "storage_control_not_authorized")
        );
        assert!(fixture.allocation.root.exists());
        assert_eq!(
            git(&fixture.repository, &["rev-parse", &fixture.branch]),
            fixture.source_oid
        );
        let guard = store.blocking_lock();
        assert_eq!(
            guard
                .get_source_worktree_settlement_run(run.run_id)
                .unwrap()
                .unwrap()
                .items[0]
                .phase,
            SourceWorktreeSettlementPhaseV1::IntentCommitted
        );
        let count: i64 = guard
            .conn
            .query_row(
                "SELECT count(*) FROM source_worktree_settlement_runs",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
        let mut session = rsid_store::test_support::make_test_session();
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
        // #2731: the verdict names the ref it compared against and its age.
        let detail = audit.report.items[0]
            .diagnostic
            .as_deref()
            .expect("nonancestor names its comparison ref");
        assert!(
            detail.starts_with("not an ancestor of compared_ref=refs/heads/")
                && detail.contains("@")
                && detail.contains(" ref_updated_secs_ago=")
                && detail.contains(" tip_committed_secs_ago=")
                && detail.contains(" fetched_this_pass=false"),
            "{detail}"
        );
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_reaps_scheduled_alias_then_refuses_untouched_root_before_git_effects() {
        let mut fixture = IntegratedFixture::new("startup-scheduled-alias");
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "startup-scheduled-alias");
        let run_id = run.run_id;
        fixture
            .store
            .insert_source_worktree_settlement_run(&run)
            .expect("insert alias-protected settlement intent");
        let alias_id = Uuid::new_v4();
        let mut scheduled_alias = rsid_store::test_support::make_test_session();
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
        manager
            .recover_startup_source_worktree_settlements()
            .await
            .expect("post-readiness recovery reaps alias before Git");
        orphan.wait_signalled("reaped scheduled alias");
        let receipt = manager
            .get_source_worktree_settlement_run(run_id)
            .await
            .expect("read alias-protected receipt")
            .expect("alias-protected receipt retained");
        assert_eq!(
            receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Refused
        );
        assert!(root.exists());
        assert!(
            !manager
                .store
                .lock()
                .await
                .path_is_inside_settlement_root(&root)
                .unwrap()
        );
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
        .expect("replay attempts the later intent without touching refused owner");
        assert_eq!(outcome.receipt.items.len(), 2);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Refused
        );
        assert_eq!(
            outcome.receipt.items[1].phase,
            SourceWorktreeSettlementPhaseV1::Settled,
            "the earlier refusal must not fence a later eligible root: {:?}",
            outcome.receipt,
        );
        assert!(outcome.receipt.finished_at.is_some());
        assert_eq!(
            store
                .blocking_lock()
                .get_session(fixture.session_id)
                .unwrap()
                .unwrap()
                .sandbox_cleanup_state,
            Some(SandboxCleanupState::Purged),
        );
        intent_process.wait_signalled("reaped intent owner");
        refused_process.assert_alive("Refused owner is safe to relaunch and must not be signaled");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_apply_reap_failure_releases_untouched_root_before_all_git_effects() {
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

        let result = manager
            .apply_source_worktree_cohort(params)
            .await
            .expect("post-intent reap failure returns a terminal receipt");
        assert!(result.finished_at.is_some());
        assert_eq!(result.counts.unattempted, 1);
        assert!(
            result.items[0]
                .after_observation
                .as_deref()
                .unwrap()
                .contains("injected startup settlement orphan reap failure")
        );
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
        assert_eq!(receipt, "unattempted");
        assert!(
            !manager
                .store
                .lock()
                .await
                .path_is_inside_settlement_root(&root)
                .unwrap()
        );
        assert!(root.exists());
        assert_eq!(git(&repository, &["rev-parse", &source_ref]), source_oid);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn replay_pre_quarantine_proof_drift_keeps_untouched_root_retryable() {
        let mut fixture = IntegratedFixture::new("replay-pre-quarantine-drift");
        let run = fixture.commit_intent("fixture:replay-pre-quarantine-drift");
        let dirty = fixture.allocation.root.join("retained-work.txt");
        std::fs::write(&dirty, "retain this work").unwrap();

        let (outcome, store) = fixture.replay_run(run.run_id);
        let item = &outcome.receipt.items[0];
        assert_eq!(item.phase, SourceWorktreeSettlementPhaseV1::Refused);
        assert_eq!(
            item.refusal_code,
            Some(SourceWorktreeSettlementRefusalV1::CustodyDrift)
        );
        assert_eq!(
            item.after_observation.as_deref(),
            Some("original worktree failed its final pre-quarantine proof")
        );
        assert!(item.before_observation.is_none());
        assert!(outcome.receipt.finished_at.is_some());
        assert_eq!(std::fs::read_to_string(&dirty).unwrap(), "retain this work");
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
        assert!(
            !store
                .blocking_lock()
                .path_is_inside_settlement_root(&fixture.allocation.root)
                .unwrap()
        );

        fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
        std::fs::remove_file(dirty).unwrap();
        let retry = fixture.commit_intent("fixture:retry-pre-quarantine-drift");
        assert_eq!(fixture.replay_run(retry.run_id).0.receipt.counts.settled, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn branch_first_restore_ignores_target_drift_then_full_proof_retains() {
        let mut fixture = IntegratedFixture::new("branch-first-restore-target-drift");
        let run = fixture.commit_intent("fixture:branch-first-restore-target-drift");
        let quarantine_root = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine_root);
        fixture.delete_source_ref(&run);

        let repository = fixture.repository.clone();
        let target_ref = run.target_ref.clone();
        // Retention requires loss of preservation, not target-tip movement.
        // A parentless target commit does not contain the source in its history.
        let drift_oid = git(
            &fixture.repository,
            &[
                "commit-tree",
                &format!("{}^{{tree}}", run.target_oid),
                "-m",
                "target without source ancestry",
            ],
        );
        let expected_target = drift_oid.clone();
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
            expected_target,
            "source restoration must not rewrite the drifted target"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    /// #2790: a root's long Git steps (both quarantine proofs and the worktree
    /// deletion) run with no Store, active or completed guard held, so a health
    /// probe, a session read or a Store writer is never queued behind them.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_holds_no_store_or_runtime_fence_across_long_git_steps() {
        let mut fixture = IntegratedFixture::new("fences-released-2790");
        let run = fixture.commit_intent("fixture:fences-released-2790");
        let quarantine_root = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine_root);

        let receipt = fixture
            .store
            .get_source_worktree_settlement_run(run.run_id)
            .expect("read fixture replay receipt")
            .expect("fixture replay receipt");
        let idempotency_key = receipt.idempotency_key.clone();
        let plan_digest = receipt.plan_digest.as_str().to_string();
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().expect("replacement fixture Store"),
        )));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));

        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        set_unlocked_step_test_hook({
            let (store, active, completed) = (
                Arc::clone(&store),
                Arc::clone(&active),
                Arc::clone(&completed),
            );
            let observed = Arc::clone(&observed);
            move |step| {
                // A writer needs every reader gone and the Store needs its lone
                // holder gone: both succeed only when the saga holds neither.
                observed.lock().unwrap().push((
                    step,
                    store.try_lock().is_ok(),
                    active.try_write().is_ok(),
                    completed.try_write().is_ok(),
                ));
            }
        });
        let outcome = fixture
            .with_quarantine_holder_test_proc(|| {
                git_worktree::with_repository_mutation(&fixture.repository, || {
                    apply_locked(
                        ApplyStart::Replay(receipt),
                        &fixture.target.repository_identity,
                        &idempotency_key,
                        &plan_digest,
                        "",
                        "",
                        &fixture.sandbox_base,
                        &store,
                        &active,
                        &completed,
                    )
                })
            })
            .expect("replay fixture settlement");
        SETTLEMENT_UNLOCKED_STEP_TEST_HOOK.with(|slot| *slot.borrow_mut() = None);

        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled
        );
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                ("first_proof", true, true, true),
                ("final_proof", true, true, true),
                ("remove", true, true, true),
            ],
            "each long Git step must run with the Store, active and completed guards released"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn quarantine_old_first_proof_fence_revives_and_settles_or_refences_real_holder() {
        for holder in [false, true] {
            let mut fixture = IntegratedFixture::new(&format!("old-proof-revive-{holder}"));
            let run = fixture.commit_intent(&format!("fixture:old-proof-revive-{holder}"));
            let quarantine = fixture.move_to_quarantine(&run);
            fixture
                .store
                .mark_source_worktree_settlement_recovery_required(
                    run.run_id,
                    fixture.session_id,
                    SourceWorktreeSettlementPhaseV1::IntentCommitted,
                    "recovery_proof_failed",
                    "quarantined worktree failed its removal-authority proof",
                )
                .unwrap();
            let fenced = fixture
                .store
                .get_source_worktree_settlement_run(run.run_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                effect_capable_replay_session_ids(&fenced),
                vec![fixture.session_id]
            );
            if holder {
                fixture
                    .quarantine_holder_proc
                    .add_fd_holder(&quarantine.join("tracked"));
            }
            fail_next_quarantine_authority_proofs(0);
            let (outcome, store) = fixture.replay_run(run.run_id);
            assert!(
                quarantine_authority_proof_attempt_count() > 0,
                "revival runs the full removal proof"
            );
            outcome.receipt.clone().validate_wire().unwrap();
            let item = &outcome.receipt.items[0];
            if holder {
                assert_eq!(
                    item.phase,
                    SourceWorktreeSettlementPhaseV1::RecoveryRequired
                );
                assert_eq!(
                    item.refusal_code,
                    Some(SourceWorktreeSettlementRefusalV1::RecoveryProofFailed)
                );
                assert!(quarantine.exists());
                assert_eq!(
                    git(&fixture.repository, &["rev-parse", &item.source_ref]),
                    item.expected_source_oid.as_str()
                );
            } else {
                assert_eq!(item.phase, SourceWorktreeSettlementPhaseV1::Settled);
                assert_eq!(outcome.receipt.counts.settled, 1);
                assert!(
                    item.before_observation.is_some(),
                    "fresh durable authority precedes removal"
                );
            }
            let counts: (i64,i64,i64,i64) = store.blocking_lock().conn.query_row(
                "SELECT r.recovery_required_count,a.settled_count,a.recovery_required_count,
                    (SELECT count(*) FROM source_worktree_settlement_reproof_items WHERE run_id=r.run_id)
                 FROM source_worktree_settlement_runs r JOIN source_worktree_settlement_reproof_runs a USING(run_id)
                 WHERE run_id=?1", [run.run_id.to_string()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
            ).unwrap();
            assert_eq!(counts, (1, i64::from(!holder), i64::from(holder), 1));
            fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
            fail_next_quarantine_authority_proofs(0);
            let (replay, _) = fixture.replay_run(run.run_id);
            assert_eq!(replay.receipt.items[0].phase, item.phase);
            assert_eq!(
                quarantine_authority_proof_attempt_count() > 0,
                holder,
                "a retained holder is freshly proved on each recovery pass; settled items are inert"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_completion_retries_preserve_marked_holders_and_drifted_refs() {
        for branch_only in [false, true] {
            let mut fixture = IntegratedFixture::new(&format!("completion-fence-{branch_only}"));
            let run = fixture.commit_intent(&format!("fixture:completion-fence-{branch_only}"));
            let quarantine = fixture.move_to_quarantine(&run);
            fixture.persist_quarantine_marker(&run, &quarantine);
            if branch_only {
                fixture.remove_quarantine(&run, &quarantine);
                git(
                    &fixture.repository,
                    &["commit", "--allow-empty", "-m", "ref drift"],
                );
                let drift = git(&fixture.repository, &["rev-parse", "HEAD"]);
                git(
                    &fixture.repository,
                    &["update-ref", &run.items[0].source_ref, &drift],
                );
            } else {
                fixture
                    .quarantine_holder_proc
                    .add_fd_holder(&quarantine.join("tracked"));
            }
            fixture
                .store
                .mark_source_worktree_settlement_recovery_required(
                    run.run_id,
                    fixture.session_id,
                    SourceWorktreeSettlementPhaseV1::IntentCommitted,
                    if branch_only {
                        "worktree_residue"
                    } else {
                        "ref_delete_failed"
                    },
                    "interrupted completion",
                )
                .unwrap();
            let expected_ref = git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref],
            );
            let (outcome, store) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired
            );
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                expected_ref
            );
            if !branch_only {
                assert!(quarantine.exists());
                fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
                let (still_held, store) = fixture.replay_run(run.run_id);
                assert_eq!(
                    still_held.receipt.items[0].phase,
                    SourceWorktreeSettlementPhaseV1::RecoveryRequired
                );
                assert!(quarantine.exists());
                fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
                std::fs::remove_dir_all(fixture.quarantine_holder_proc.proc_root().join("991"))
                    .unwrap();
                assert_eq!(
                    fixture.replay_run(run.run_id).0.receipt.items[0].phase,
                    SourceWorktreeSettlementPhaseV1::Settled
                );
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_completion_retries_root_busy_after_quarantine() {
        let mut fixture = IntegratedFixture::new("completion-root-busy");
        let run = fixture.commit_intent("fixture:completion-root-busy");
        let quarantine = fixture.move_to_quarantine(&run);
        let custody_id = fixture.custody_id;
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = crate::store::sandbox_custody::lock_custody_root(custody_id);
            held_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap();
        });
        held_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        crate::store::custody_lock_order::set_bounded_wait_for_test(Some(
            std::time::Duration::from_millis(1),
        ));
        let (pending, store) = fixture.replay_run(run.run_id);
        crate::store::custody_lock_order::set_bounded_wait_for_test(None);
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert_eq!(
            pending.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::IntentCommitted
        );
        assert!(quarantine.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
        fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
        let (settled, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            settled.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn retained_untouched_intent_startup_reproof_settles_and_preserves_audit() {
        let mut fixture = IntegratedFixture::new("retained-untouched-startup");
        let run = fixture.commit_intent("fixture:retained-untouched-startup");
        fixture.store.mark_source_worktree_settlement_recovery_required(
            run.run_id, fixture.session_id, SourceWorktreeSettlementPhaseV1::IntentCommitted,
            "recovery_proof_failed",
            "retained intent followed an earlier terminal stop and cannot prove no prior effect",
        ).unwrap();
        let fenced = fixture
            .store
            .get_source_worktree_settlement_run(run.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            effect_capable_replay_session_ids(&fenced),
            vec![fixture.session_id]
        );
        let store = std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().unwrap(),
        );
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            store,
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
            fixture.sandbox_base.clone(),
        )
        .unwrap();
        manager.recover_source_worktree_settlements().await.unwrap();
        let receipt = manager
            .get_source_worktree_settlement_run(run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled,
            "{receipt:?}"
        );
        assert_eq!(receipt.counts.settled, 1);
        assert!(receipt.finished_at.is_some());
        assert!(
            receipt.items[0].before_observation.is_some(),
            "fresh removal authority was recorded"
        );
        let audit: (i64, String) = manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT r.recovery_required_count,a.observation
             FROM source_worktree_settlement_runs r
             JOIN source_worktree_settlement_completion_retries a USING(run_id) WHERE r.run_id=?1",
                [run.run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            audit,
            (1, fenced.items[0].after_observation.clone().unwrap())
        );
        manager.recover_source_worktree_settlements().await.unwrap();
        assert_eq!(
            manager
                .get_source_worktree_settlement_run(run.run_id)
                .await
                .unwrap()
                .unwrap(),
            receipt
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn retained_untouched_intent_startup_reproof_rechecks_automatic_policy() {
        use rsi_common::cohort_settlement::{
            SourceWorktreeSettlementAttributionV1, SourceWorktreeSettlementOriginV1,
        };
        let mut fixture = IntegratedFixture::new("retained-untouched-auto-disabled");
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "fixture:retained-untouched-auto-disabled");
        let attribution = SourceWorktreeSettlementAttributionV1 {
            origin: SourceWorktreeSettlementOriginV1::Automatic,
            agent_session_id: None,
        };
        fixture
            .store
            .insert_source_worktree_settlement_run_as(&run, &attribution)
            .unwrap();
        fixture.store.mark_source_worktree_settlement_recovery_required(
            run.run_id, fixture.session_id, SourceWorktreeSettlementPhaseV1::IntentCommitted,
            "recovery_proof_failed",
            "retained intent followed an earlier terminal stop and cannot prove no prior effect",
        ).unwrap();
        let config = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        config
            .source_worktree_auto_settlement_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let store = std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().unwrap(),
        );
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            store,
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            config,
            fixture.sandbox_base.clone(),
        )
        .unwrap();
        manager.recover_source_worktree_settlements().await.unwrap();
        let receipt = manager
            .get_source_worktree_settlement_run(run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Unattempted
        );
        assert_eq!(receipt.counts.unattempted, 1);
        assert_eq!(receipt.attribution, Some(attribution));
        assert!(fixture.allocation.root.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retained_intent_mid_run_policy_stop_releases_every_proven_root_for_fresh_apply() {
        use rsi_common::cohort_settlement::{
            SourceWorktreeSettlementAttributionV1, SourceWorktreeSettlementOriginV1,
        };
        use std::sync::atomic::Ordering;
        for startup in [false, true] {
            for drift in [false, true] {
                let label = format!("retained-policy-stop-{startup}-{drift}");
                let mut fixture = IntegratedFixture::new(&label);
                for index in 1..4 {
                    fixture.add_second_root(&format!("{label}-{index}"));
                }
                let (run, params) = fixture.commit_intent_for_all_eligible_with_params(
                    &format!("fixture:{label}"),
                    &SourceWorktreeSettlementAttributionV1 {
                        origin: SourceWorktreeSettlementOriginV1::Automatic,
                        agent_session_id: None,
                    },
                );
                assert_eq!(run.items.len(), 4);
                for item in &run.items {
                    fixture.store.mark_source_worktree_settlement_recovery_required(
                        run.run_id, item.session_id, SourceWorktreeSettlementPhaseV1::IntentCommitted,
                        "recovery_proof_failed",
                        "retained intent followed an earlier terminal stop and cannot prove no prior effect",
                    ).unwrap();
                }
                if drift {
                    std::fs::write(
                        Path::new(&run.items[3].sandbox_root).join("tracked"),
                        "retain changed work\n",
                    )
                    .unwrap();
                }
                let runtime =
                    crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
                let base = fixture.sandbox_base.clone();
                settlement_disk_samples()
                    .lock()
                    .unwrap()
                    .insert(base.clone(), (0, 192 * 1024 * 1024 * 1024));
                let (reached_tx, reached_rx) = std::sync::mpsc::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                // Pause item 2 after revival, before its live policy check.
                // Item 1 has settled; later items still have old fences.
                SETTLEMENT_PROOF_PAUSE.lock().unwrap().insert(
                    run.items[1].session_id,
                    SettlementProofPause {
                        reached: reached_tx,
                        release: release_rx,
                    },
                );
                let store = std::mem::replace(
                    &mut fixture.store,
                    crate::store::Store::open_in_memory().unwrap(),
                );
                let manager = Arc::new(
                    SessionManager::new(
                        Arc::new(crate::bus::EventBus::new(16)),
                        store,
                        false,
                        fixture._directory.path().join("daemon.sock"),
                        None,
                        Vec::new(),
                        Arc::clone(&runtime),
                        base.clone(),
                    )
                    .unwrap(),
                );
                let driver_manager = Arc::clone(&manager);
                let driver_runtime = Arc::clone(&runtime);
                let driver = tokio::spawn(async move {
                    if startup {
                        driver_manager.recover_source_worktree_settlements().await
                    } else {
                        // Use the production request path, including its
                        // fully-terminal receipt replay gate.
                        driver_manager
                            .apply_source_worktree_cohort_as(
                                params,
                                None,
                                Some(driver_runtime),
                                false,
                            )
                            .await
                            .map(|_| ())
                    }
                });
                tokio::task::spawn_blocking(move || {
                    reached_rx
                        .recv_timeout(std::time::Duration::from_secs(60))
                        .unwrap()
                })
                .await
                .unwrap();
                let before_stop = manager
                    .get_source_worktree_settlement_run(run.run_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    before_stop.items[0].phase,
                    SourceWorktreeSettlementPhaseV1::Settled
                );
                assert_eq!(
                    before_stop.items[2].phase,
                    SourceWorktreeSettlementPhaseV1::RecoveryRequired
                );
                if startup {
                    runtime
                        .source_worktree_auto_settlement_enabled
                        .store(false, Ordering::Relaxed);
                } else {
                    settlement_disk_samples()
                        .lock()
                        .unwrap()
                        .insert(base.clone(), (u64::MAX, 192 * 1024 * 1024 * 1024));
                }
                release_tx.send(()).unwrap();
                driver.await.unwrap().unwrap();
                let stopped = manager
                    .get_source_worktree_settlement_run(run.run_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(stopped.counts.settled, 1, "{stopped:?}");
                assert_eq!(
                    stopped.counts.unattempted,
                    if drift { 2 } else { 3 },
                    "{stopped:?}"
                );
                assert_eq!(
                    stopped.counts.recovery_required,
                    u32::from(drift),
                    "{stopped:?}"
                );
                assert!(stopped.finished_at.is_some());
                assert!(stopped.items.iter().all(|item| item.phase.is_terminal()));
                let audit_rows: i64 = manager.store.lock().await.conn.query_row(
                    "SELECT count(*) FROM source_worktree_settlement_completion_retries WHERE run_id=?1",
                    [run.run_id.to_string()], |row| row.get(0),
                ).unwrap();
                assert_eq!(
                    audit_rows,
                    if drift { 3 } else { 4 },
                    "each eligible old fence is audited once"
                );
                if drift {
                    assert_eq!(
                        stopped.items[3].phase,
                        SourceWorktreeSettlementPhaseV1::RecoveryRequired
                    );
                    assert_eq!(
                        std::fs::read_to_string(
                            Path::new(&run.items[3].sandbox_root).join("tracked")
                        )
                        .unwrap(),
                        "retain changed work\n"
                    );
                    assert_eq!(
                        git(
                            &fixture.repository,
                            &["rev-parse", &run.items[3].source_ref]
                        ),
                        run.items[3].source_oid
                    );
                }
                let fresh_audit = manager
                    .audit_source_worktree_cohort(run.repository_identity.clone())
                    .await
                    .unwrap();
                assert_eq!(fresh_audit.counts.eligible, if drift { 2 } else { 3 });
                let fresh = manager
                    .apply_source_worktree_cohort(ApplySourceWorktreeCohortParams {
                        repository_identity: run.repository_identity.clone(),
                        plan_digest: fresh_audit.plan_digest,
                        authorization: fresh_audit.authorization_phrase.unwrap(),
                        idempotency_key: format!("fresh:{label}"),
                    })
                    .await
                    .unwrap();
                assert_eq!(fresh.counts.settled, if drift { 2 } else { 3 }, "{fresh:?}");
                assert_eq!(fresh.counts.recovery_required, 0);
                assert!(fresh.finished_at.is_some());
                settlement_disk_samples().lock().unwrap().remove(&base);
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn retained_untouched_intent_startup_reproof_preserves_ambiguous_or_drifted_fences() {
        for case in [
            "reserved",
            "active",
            "quarantine",
            "marker",
            "ref",
            "dirty",
            "custody",
        ] {
            let mut fixture = IntegratedFixture::new(&format!("retained-untouched-{case}"));
            let run = fixture.commit_intent(&format!("fixture:retained-untouched-{case}"));
            match case {
                "reserved" | "active" => {
                    let reservation = fixture
                        .store
                        .reserve_effect(
                            fixture.custody_id,
                            run.items[0].custody_generation,
                            Uuid::new_v4(),
                        )
                        .unwrap();
                    if case == "active" {
                        fixture.store.settle_effect(reservation, true).unwrap();
                    }
                }
                "quarantine" => {
                    fixture.move_to_quarantine(&run);
                }
                "marker" => {
                    fixture
                        .store
                        .advance_source_worktree_settlement_item(
                            run.run_id,
                            fixture.session_id,
                            SourceWorktreeSettlementPhaseV1::IntentCommitted,
                            SourceWorktreeSettlementPhaseV1::IntentCommitted,
                            Some("retained effect authority"),
                            None,
                            None,
                        )
                        .unwrap();
                }
                "ref" => {
                    let tip = git(&fixture.repository, &["rev-parse", "HEAD"]);
                    git(
                        &fixture.repository,
                        &["update-ref", &run.items[0].source_ref, &tip],
                    );
                }
                "dirty" => {
                    std::fs::write(fixture.allocation.root.join("tracked"), "changed\n").unwrap();
                }
                "custody" => {
                    fixture.store.conn.execute(
                        "UPDATE sandbox_custody_roots SET validated_generation=NULL,validated_at=NULL,validation_state='unverified' WHERE custody_id=?1",
                        [fixture.custody_id.to_string()],
                    ).unwrap();
                }
                _ => unreachable!(),
            }
            fixture.store.mark_source_worktree_settlement_recovery_required(
                run.run_id, fixture.session_id, SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "recovery_proof_failed",
                "retained intent followed an earlier terminal stop and cannot prove no prior effect",
            ).unwrap();
            let fenced = fixture
                .store
                .get_source_worktree_settlement_run(run.run_id)
                .unwrap()
                .unwrap();
            let ref_before = git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref],
            );
            let store = std::mem::replace(
                &mut fixture.store,
                crate::store::Store::open_in_memory().unwrap(),
            );
            let manager = SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                store,
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                fixture.sandbox_base.clone(),
            )
            .unwrap();
            manager.recover_source_worktree_settlements().await.unwrap();
            let receipt = manager
                .get_source_worktree_settlement_run(run.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                receipt, fenced,
                "{case}: preserve the complete original fence"
            );
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                ref_before,
                "{case}"
            );
            assert!(
                if case == "quarantine" {
                    fixture.quarantine_root(run.run_id).exists()
                } else {
                    fixture.allocation.root.exists()
                },
                "{case}: preserve retained root"
            );
            let retries: i64 = manager.store.lock().await.conn.query_row(
                "SELECT count(*) FROM source_worktree_settlement_completion_retries WHERE run_id=?1",
                [run.run_id.to_string()], |row| row.get(0),
            ).unwrap();
            assert_eq!(
                retries, 0,
                "{case}: no retry without unchanged no-effect evidence"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn retained_untouched_intent_apply_replay_runs_fresh_proof_or_refuses() {
        for lost_target in [false, true] {
            let mut fixture = IntegratedFixture::new(&format!("retained-replay-{lost_target}"));
            let run = fixture.commit_intent(&format!("fixture:retained-replay-{lost_target}"));
            fixture.store.mark_source_worktree_settlement_recovery_required(
                run.run_id, fixture.session_id, SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "recovery_proof_failed",
                "retained intent followed an earlier terminal stop and cannot prove no prior effect",
            ).unwrap();
            if lost_target {
                git(&fixture.repository, &["update-ref", "-d", &run.target_ref]);
            }
            let (outcome, _) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                if lost_target {
                    SourceWorktreeSettlementPhaseV1::Refused
                } else {
                    SourceWorktreeSettlementPhaseV1::Settled
                },
                "{:?}",
                outcome.receipt
            );
            if lost_target {
                assert!(fixture.allocation.root.exists());
                assert_eq!(
                    outcome.receipt.items[0].refusal_code,
                    Some(SourceWorktreeSettlementRefusalV1::CustodyDrift)
                );
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn settlement_completion_retries_startup_registered_and_branch_only_residue() {
        for branch_only in [false, true] {
            let mut fixture = IntegratedFixture::new(&format!("completion-startup-{branch_only}"));
            let run = fixture.commit_intent(&format!("fixture:completion-startup-{branch_only}"));
            let quarantine = fixture.move_to_quarantine(&run);
            fixture.persist_quarantine_marker(&run, &quarantine);
            if branch_only {
                fixture.remove_quarantine(&run, &quarantine);
            }
            fixture.store.mark_source_worktree_settlement_recovery_required(
                run.run_id, fixture.session_id, SourceWorktreeSettlementPhaseV1::IntentCommitted,
                if branch_only { "worktree_residue" } else { "ref_delete_failed" },
                if branch_only { "dangling quarantine removal failed; the exact source ref was restored" }
                else { "source deletion returned an error; the exact source ref was synchronously restored" },
            ).unwrap();
            let store = std::mem::replace(
                &mut fixture.store,
                crate::store::Store::open_in_memory().unwrap(),
            );
            let manager = SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                store,
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                fixture.sandbox_base.clone(),
            )
            .unwrap();
            manager.recover_source_worktree_settlements().await.unwrap();
            let receipt = manager
                .get_source_worktree_settlement_run(run.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::Settled,
                "{receipt:?}"
            );
            assert!(receipt.finished_at.is_some());
            manager.recover_source_worktree_settlements().await.unwrap();
            assert_eq!(
                manager
                    .get_source_worktree_settlement_run(run.run_id)
                    .await
                    .unwrap()
                    .unwrap(),
                receipt
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_completion_retry_reproves_volatile_marker_facts() {
        for drift in ["target", "holders", "both"] {
            let mut fixture = IntegratedFixture::new(&format!("completion-volatile-{drift}"));
            let run = fixture.commit_intent(&format!("fixture:completion-volatile-{drift}"));
            let quarantine = fixture.move_to_quarantine(&run);
            let marker = fixture.build_quarantine_marker(&run, &quarantine);
            // Model a previous complete inventory with more fixed-point passes
            // and trusted processes. Today's real proof still runs in full.
            let marker = if drift != "target" {
                let mut facts = serde_json::to_value(&marker).unwrap();
                facts["holder_fixed_point_passes"] = serde_json::json!(3);
                facts["trusted_platform_exemption_count"] = serde_json::json!(6);
                facts["holder_evidence_digest"] =
                    serde_json::json!(format!("sha256:{}", "a".repeat(64)));
                facts["trusted_platform_exemption_digest"] =
                    serde_json::json!(format!("sha256:{}", "b".repeat(64)));
                QuarantineRemoveAuthorityV1::parse_canonical(
                    &serde_json::from_value::<QuarantineRemoveAuthorityV1>(facts)
                        .unwrap()
                        .to_canonical_json()
                        .unwrap(),
                )
                .unwrap()
            } else {
                marker
            };
            fixture
                .store
                .record_source_worktree_quarantine_remove_authority(
                    run.run_id,
                    fixture.session_id,
                    &marker,
                )
                .unwrap();
            if drift != "holders" {
                git(
                    &fixture.repository,
                    &["commit", "--allow-empty", "-qm", "advance target"],
                );
            }
            let current_target = git(&fixture.repository, &["rev-parse", &run.target_ref]);
            fixture.store.mark_source_worktree_settlement_recovery_required(
                run.run_id, fixture.session_id, SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "ref_delete_failed", "source deletion returned an error; the exact source ref was synchronously restored",
            ).unwrap();
            fail_next_quarantine_authority_proofs(0);

            let (outcome, store) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::Settled,
                "{drift}: {:?}",
                outcome.receipt
            );
            assert!(
                quarantine_authority_proof_attempt_count() >= 2,
                "both preliminary and final full proofs must run: {drift}"
            );
            assert_eq!(
                outcome.receipt.items[0].before_observation.as_deref(),
                Some(marker.to_canonical_json().unwrap().as_str()),
                "the historical marker stays immutable: {drift}"
            );
            assert!(
                worktree_paths_are_absent_and_unregistered(
                    &fixture.repository,
                    &fixture.allocation.root,
                    &quarantine
                )
                .unwrap()
            );
            assert_eq!(
                git_worktree::observe_direct_ref_locked(
                    &fixture.repository,
                    &run.items[0].source_ref
                )
                .unwrap(),
                git_worktree::DirectRefObservation::Missing
            );
            assert_eq!(
                git(&fixture.repository, &["rev-parse", &run.target_ref]),
                current_target
            );
            assert_eq!(
                store
                    .blocking_lock()
                    .get_session(fixture.session_id)
                    .unwrap()
                    .unwrap()
                    .sandbox_cleanup_state,
                Some(SandboxCleanupState::Purged)
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_completion_retry_fences_quarantine_identity_drift() {
        for field in ["root_inode", "stable_tree_digest", "admin_id_digest"] {
            let mut fixture = IntegratedFixture::new(&format!("completion-identity-{field}"));
            let run = fixture.commit_intent(&format!("fixture:completion-identity-{field}"));
            let quarantine = fixture.move_to_quarantine(&run);
            let marker = fixture.build_quarantine_marker(&run, &quarantine);
            let mut facts = serde_json::to_value(&marker).unwrap();
            facts[field] = if field == "root_inode" {
                serde_json::json!(facts[field].as_u64().unwrap() + 1)
            } else {
                serde_json::json!(format!("sha256:{}", "f".repeat(64)))
            };
            let marker: QuarantineRemoveAuthorityV1 = serde_json::from_value(facts).unwrap();
            fixture
                .store
                .record_source_worktree_quarantine_remove_authority(
                    run.run_id,
                    fixture.session_id,
                    &marker,
                )
                .unwrap();
            fixture
                .store
                .mark_source_worktree_settlement_recovery_required(
                    run.run_id,
                    fixture.session_id,
                    SourceWorktreeSettlementPhaseV1::IntentCommitted,
                    "ref_delete_failed",
                    "interrupted completion",
                )
                .unwrap();

            let (outcome, _) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired,
                "{field}"
            );
            assert_eq!(
                outcome.receipt.items[0].refusal_code,
                Some(SourceWorktreeSettlementRefusalV1::RecoveryProofFailed),
                "{field}"
            );
            assert!(quarantine.exists(), "{field}");
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                run.items[0].source_oid,
                "{field}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn final_quarantine_proof_uses_current_target_and_fences_new_holder() {
        for holder in [false, true] {
            let mut fixture = IntegratedFixture::new(&format!("final-proof-drift-{holder}"));
            let run = fixture.commit_intent(&format!("fixture:final-proof-drift-{holder}"));
            let quarantine = fixture.move_to_quarantine(&run);
            // Seed a harmless readable process, then change only its descriptor
            // between the preliminary and final proofs.
            fixture
                .quarantine_holder_proc
                .add_fd_holder(&fixture.repository.join("tracked"));
            fixture.persist_quarantine_marker(&run, &quarantine);
            let repository = fixture.repository.clone();
            let descriptor = fixture
                .quarantine_holder_proc
                .proc_root()
                .join("991/task/991/fd/3");
            let held_file = quarantine.join("tracked");
            set_before_final_proof_test_hook(move || {
                git(
                    &repository,
                    &["commit", "--allow-empty", "-qm", "advance during proof"],
                );
                if holder {
                    std::fs::remove_file(&descriptor).unwrap();
                    std::os::unix::fs::symlink(held_file, descriptor).unwrap();
                }
            });

            let (outcome, _) = fixture.replay_run(run.run_id);
            assert_ne!(
                git(&fixture.repository, &["rev-parse", &run.target_ref]),
                run.target_oid
            );
            if holder {
                assert_eq!(
                    outcome.receipt.items[0].phase,
                    SourceWorktreeSettlementPhaseV1::RecoveryRequired
                );
                assert_eq!(
                    outcome.receipt.items[0].refusal_code,
                    Some(SourceWorktreeSettlementRefusalV1::RecoveryProofFailed)
                );
                assert!(quarantine.exists());
                assert_eq!(
                    git(
                        &fixture.repository,
                        &["rev-parse", &run.items[0].source_ref]
                    ),
                    run.items[0].source_oid
                );
            } else {
                assert_eq!(
                    outcome.receipt.items[0].phase,
                    SourceWorktreeSettlementPhaseV1::Settled,
                    "{:?}",
                    outcome.receipt
                );
                assert!(
                    worktree_paths_are_absent_and_unregistered(
                        &fixture.repository,
                        &fixture.allocation.root,
                        &quarantine
                    )
                    .unwrap()
                );
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_restores_source_when_symref_dependency_appears_during_delete_or_remove() {
        for window in ["delete", "remove"] {
            let mut fixture = IntegratedFixture::new(&format!("symref-window-{window}"));
            let run = fixture.commit_intent(&format!("fixture:symref-window-{window}"));
            let quarantine = fixture.move_to_quarantine(&run);
            fixture.persist_quarantine_marker(&run, &quarantine);
            let repository = fixture.repository.clone();
            let source_ref = run.items[0].source_ref.clone();
            let alias = "refs/heads/keep/alias";
            let create_dependent = move || {
                git(&repository, &["symbolic-ref", alias, &source_ref]);
            };
            if window == "delete" {
                git_worktree::set_atomic_ref_pre_spawn_test_hook(create_dependent);
            } else {
                set_after_dangling_remove_test_hook(create_dependent);
            }
            let (outcome, store) = fixture.replay_run(run.run_id);
            let item = &outcome.receipt.items[0];
            assert_eq!(
                item.phase,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired,
                "{window}"
            );
            assert_eq!(
                item.refusal_code,
                Some(if window == "delete" {
                    SourceWorktreeSettlementRefusalV1::RefDeleteFailed
                } else {
                    SourceWorktreeSettlementRefusalV1::WorktreeResidue
                }),
                "{window}"
            );
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                run.items[0].source_oid
            );
            assert_eq!(
                git(
                    &fixture.repository,
                    &["symbolic-ref", "--no-recurse", alias]
                ),
                run.items[0].source_ref
            );
            assert_eq!(
                git(&fixture.repository, &["rev-parse", alias]),
                run.items[0].source_oid
            );
            if window == "delete" {
                assert!(quarantine.exists(), "delete drift retains the quarantine");
            } else {
                assert!(
                    worktree_paths_are_absent_and_unregistered(
                        &fixture.repository,
                        &fixture.allocation.root,
                        &quarantine,
                    )
                    .unwrap(),
                    "remove drift retains the restored source after path removal"
                );
            }
            // The retry is still fenced by the live dependent and preserves it.
            fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
            let (retry, _) = fixture.replay_run(run.run_id);
            assert_eq!(
                retry.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired
            );
            assert_eq!(
                git(&fixture.repository, &["rev-parse", alias]),
                run.items[0].source_oid
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_tolerates_unrelated_ref_writes_during_delete_and_remove() {
        for window in ["delete", "remove"] {
            let mut fixture = IntegratedFixture::new(&format!("unrelated-ref-window-{window}"));
            let run = fixture.commit_intent(&format!("fixture:unrelated-ref-window-{window}"));
            let quarantine = fixture.move_to_quarantine(&run);
            fixture.persist_quarantine_marker(&run, &quarantine);
            let refs = [
                "refs/heads/rsi/other",
                "refs/remotes/origin/other",
                "refs/stash",
            ];
            for reference in refs {
                git(
                    &fixture.repository,
                    &["update-ref", reference, &fixture.source_oid],
                );
            }
            git(
                &fixture.repository,
                &["symbolic-ref", "refs/heads/keep/alias", refs[0]],
            );
            let repository = fixture.repository.clone();
            let target_oid = run.target_oid.clone();
            let change_refs = move || {
                for reference in refs {
                    git(&repository, &["update-ref", reference, &target_oid]);
                }
                git(
                    &repository,
                    &["update-ref", "refs/heads/rsi/new", &target_oid],
                );
            };
            if window == "delete" {
                git_worktree::set_atomic_ref_pre_spawn_test_hook(change_refs);
            } else {
                set_after_dangling_remove_test_hook(change_refs);
            }

            let (outcome, store) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::Settled,
                "{window}: {:?}",
                outcome.receipt
            );
            assert!(
                worktree_paths_are_absent_and_unregistered(
                    &fixture.repository,
                    &fixture.allocation.root,
                    &quarantine
                )
                .unwrap()
            );
            assert!(
                git_worktree::source_ref_has_zero_registrations_locked(
                    &fixture.repository,
                    &run.items[0].source_ref
                )
                .unwrap()
            );
            assert_eq!(
                git_worktree::observe_direct_ref_locked(
                    &fixture.repository,
                    &run.items[0].source_ref
                )
                .unwrap(),
                git_worktree::DirectRefObservation::Missing
            );
            for reference in refs.into_iter().chain([
                "refs/heads/rsi/new",
                "refs/heads/keep/alias",
                run.target_ref.as_str(),
            ]) {
                assert_eq!(
                    git(&fixture.repository, &["rev-parse", reference]),
                    run.target_oid,
                    "unrelated writes must be preserved: {window} {reference}"
                );
            }
            assert_eq!(
                store
                    .blocking_lock()
                    .get_session(fixture.session_id)
                    .unwrap()
                    .unwrap()
                    .sandbox_cleanup_state,
                Some(SandboxCleanupState::Purged)
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_completion_retries_registered_quarantine_after_ref_delete_failure() {
        let mut fixture = IntegratedFixture::new("completion-ref-delete");
        let run = fixture.commit_intent("fixture:completion-ref-delete");
        let quarantine = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine);
        fixture.store.mark_source_worktree_settlement_recovery_required(
            run.run_id, fixture.session_id, SourceWorktreeSettlementPhaseV1::IntentCommitted,
            "ref_delete_failed", "source deletion returned an error; the exact source ref was synchronously restored",
        ).unwrap();
        assert!(
            git(&fixture.repository, &["worktree", "list", "--porcelain"])
                .contains(quarantine.to_str().unwrap())
        );
        assert_eq!(
            fixture
                .store
                .list_nonterminal_source_worktree_settlement_run_ids(None, 64)
                .unwrap(),
            vec![run.run_id]
        );
        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled
        );
        assert!(outcome.receipt.finished_at.is_some());
        assert!(!quarantine.exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_completion_retries_after_holder_disappears() {
        let mut fixture = IntegratedFixture::new("completion-holder");
        let run = fixture.commit_intent("fixture:completion-holder");
        let quarantine = fixture.move_to_quarantine(&run);
        fixture
            .quarantine_holder_proc
            .add_fd_holder(&quarantine.join("tracked"));
        for _ in 0..2 {
            let (outcome, store) = fixture.replay_run(run.run_id);
            assert_eq!(
                outcome.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::RecoveryRequired
            );
            assert!(quarantine.exists());
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                run.items[0].source_oid
            );
            fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
        }
        std::fs::remove_dir_all(fixture.quarantine_holder_proc.proc_root().join("991")).unwrap();
        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled
        );
        assert!(outcome.receipt.finished_at.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_completion_retries_branch_only_residue() {
        let mut fixture = IntegratedFixture::new("completion-branch-only");
        let run = fixture.commit_intent("fixture:completion-branch-only");
        let quarantine = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine);
        fixture.remove_quarantine(&run, &quarantine);
        fixture
            .store
            .mark_source_worktree_settlement_recovery_required(
                run.run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "worktree_residue",
                "dangling quarantine removal failed; the exact source ref was restored",
            )
            .unwrap();
        assert!(
            worktree_paths_are_absent_and_unregistered(
                &fixture.repository,
                &fixture.allocation.root,
                &quarantine
            )
            .unwrap()
        );
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
        assert_eq!(
            fixture
                .store
                .list_nonterminal_source_worktree_settlement_run_ids(None, 64)
                .unwrap(),
            vec![run.run_id]
        );
        let (outcome, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled
        );
        assert!(outcome.receipt.finished_at.is_some());
    }

    fn completion_retry_rows(store: &crate::store::Store, run_id: Uuid) -> i64 {
        store
            .conn
            .query_row(
                "SELECT count(*) FROM source_worktree_settlement_completion_retries WHERE run_id=?1",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .expect("count completion retry rows")
    }

    fn worktree_is_registered(repository: &Path, root: &Path) -> bool {
        git(repository, &["worktree", "list", "--porcelain"])
            .lines()
            .any(|line| line == format!("worktree {}", root.display()))
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn completion_retry_removes_the_exact_stale_quarantine_registration_and_settles() {
        let mut fixture = IntegratedFixture::new("completion-stale-registration");
        let run = fixture.commit_intent("fixture:completion-stale-registration");
        let quarantine = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine);
        // An unrelated, equally prunable registration must never be touched.
        let bystander_session = Uuid::new_v4();
        let bystander = git_worktree::allocate(
            &fixture.sandbox_base,
            bystander_session,
            &fixture.repository,
            &fixture.source_oid,
            Some(&format!("rsi/stale-bystander/{bystander_session}")),
        )
        .expect("allocate bystander worktree");
        std::fs::remove_dir_all(&bystander.root).expect("lose the bystander tree");
        // `git worktree remove` deleted the tree but left its registration.
        std::fs::remove_dir_all(&quarantine).expect("lose the quarantine tree");
        fixture
            .store
            .mark_source_worktree_settlement_recovery_required(
                run.run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "worktree_residue",
                "authorized removal left a worktree registration behind",
            )
            .unwrap();
        assert!(worktree_is_registered(&fixture.repository, &quarantine));

        let (outcome, store) = fixture.replay_run(run.run_id);
        let item = &outcome.receipt.items[0];
        assert_eq!(
            item.phase,
            SourceWorktreeSettlementPhaseV1::Settled,
            "{item:?}"
        );
        assert!(outcome.receipt.finished_at.is_some());
        assert!(!worktree_is_registered(&fixture.repository, &quarantine));
        assert!(
            worktree_is_registered(&fixture.repository, &bystander.root),
            "only the exact quarantine registration is removed"
        );
        assert_eq!(
            git_worktree::observe_direct_ref_locked(&fixture.repository, &item.source_ref).unwrap(),
            git_worktree::DirectRefObservation::Missing
        );
        assert_eq!(completion_retry_rows(&store.blocking_lock(), run.run_id), 1);
        fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
        let (replay, store) = fixture.replay_run(run.run_id);
        assert_eq!(replay.receipt, outcome.receipt);
        assert_eq!(
            completion_retry_rows(&store.blocking_lock(), run.run_id),
            1,
            "a settled item appends no further retry rows"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn completion_retry_keeps_a_stale_registration_the_marker_does_not_authenticate() {
        let mut fixture = IntegratedFixture::new("completion-stale-foreign");
        let run = fixture.commit_intent("fixture:completion-stale-foreign");
        let quarantine = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine);
        let admin = git_worktree::with_repository_mutation(&fixture.repository, || {
            git_worktree::prove_moved_worktree_exact_locked(
                &fixture.repository,
                &fixture.allocation.root,
                &quarantine,
                &run.items[0].source_ref,
                &run.items[0].source_oid,
            )
        })
        .expect("authenticate the quarantine administration");
        std::fs::remove_dir_all(&quarantine).expect("lose the quarantine tree");
        // The administration is no longer the one the marker recorded.
        let renamed = admin
            .admin_directory
            .parent()
            .unwrap()
            .join("not-the-marked-admin");
        std::fs::rename(&admin.admin_directory, &renamed).expect("rename the admin directory");
        fixture
            .store
            .mark_source_worktree_settlement_recovery_required(
                run.run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "worktree_residue",
                "authorized removal left a worktree registration behind",
            )
            .unwrap();

        let (outcome, _) = fixture.replay_run(run.run_id);
        let item = &outcome.receipt.items[0];
        assert_eq!(
            item.phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert_eq!(
            item.refusal_code,
            Some(SourceWorktreeSettlementRefusalV1::WorktreeResidue)
        );
        assert!(
            renamed.exists(),
            "an unauthenticated administration is kept"
        );
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn completion_retry_keeps_a_stale_registration_that_drifts_before_deletion() {
        let mut fixture = IntegratedFixture::new("completion-stale-drift");
        let run = fixture.commit_intent("fixture:completion-stale-drift");
        let quarantine = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine);
        let admin = git_worktree::with_repository_mutation(&fixture.repository, || {
            git_worktree::prove_moved_worktree_exact_locked(
                &fixture.repository,
                &fixture.allocation.root,
                &quarantine,
                &run.items[0].source_ref,
                &run.items[0].source_oid,
            )
        })
        .expect("authenticate the quarantine administration");
        std::fs::remove_dir_all(&quarantine).expect("lose the quarantine tree");
        fixture
            .store
            .mark_source_worktree_settlement_recovery_required(
                run.run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "worktree_residue",
                "authorized removal left a worktree registration behind",
            )
            .unwrap();
        // The administration is exactly the marked one when the daemon observes
        // it; an external writer then repoints its HEAD before the locked hook.
        let drifting_head = admin.admin_directory.join("HEAD");
        git_worktree::set_atomic_ref_pre_spawn_test_hook(move || {
            std::fs::write(&drifting_head, b"ref: refs/heads/someone-else\n")
                .expect("repoint the stale HEAD");
        });

        let (outcome, _) = fixture.replay_run(run.run_id);
        let item = &outcome.receipt.items[0];
        assert_eq!(
            item.phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert_eq!(
            item.refusal_code,
            Some(SourceWorktreeSettlementRefusalV1::WorktreeResidue)
        );
        assert_eq!(
            std::fs::read(admin.admin_directory.join("HEAD")).expect("preserved HEAD"),
            b"ref: refs/heads/someone-else\n",
            "the drifted administration is left exactly as found"
        );
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn completion_retry_with_a_held_target_lock_leaves_branch_only_then_settles() {
        let mut fixture = IntegratedFixture::new("completion-target-lock");
        let run = fixture.commit_intent("fixture:completion-target-lock");
        let quarantine = fixture.move_to_quarantine(&run);
        fixture.persist_quarantine_marker(&run, &quarantine);
        fixture
            .store
            .mark_source_worktree_settlement_recovery_required(
                run.run_id,
                fixture.session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "ref_delete_failed",
                "source deletion returned an error; the exact source ref was synchronously restored",
            )
            .unwrap();
        let target_lock = PathBuf::from(git(
            &fixture.repository,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                &run.target_ref,
            ],
        ))
        .with_extension("lock");
        std::fs::write(&target_lock, b"concurrent target writer\n").expect("hold target lock");

        let (held, store) = fixture.replay_run(run.run_id);
        let item = &held.receipt.items[0];
        assert_eq!(
            item.phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
        assert_eq!(
            item.refusal_code,
            Some(SourceWorktreeSettlementRefusalV1::RefDeleteFailed)
        );
        assert!(
            !quarantine.exists() && !worktree_is_registered(&fixture.repository, &quarantine),
            "the quarantine removal never needed the target ref lock"
        );
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
        assert!(target_lock.exists(), "the foreign target lock is untouched");

        std::fs::remove_file(&target_lock).expect("release target lock");
        fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
        let (settled, _) = fixture.replay_run(run.run_id);
        assert_eq!(
            settled.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled
        );
        assert!(settled.receipt.finished_at.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_replay_of_a_fully_terminal_run_revives_its_first_proof_fence() {
        for holder in [false, true] {
            let label = format!("rpc-replay-revive-{holder}");
            let mut fixture = IntegratedFixture::new(&label);
            let mixed = fixture
                .commit_mixed_terminal_run_with_first_proof_fence(&format!("fixture:{label}"));
            if holder {
                fixture
                    .quarantine_holder_proc
                    .add_fd_holder(&mixed.quarantine.join("tracked"));
            }
            let store = std::mem::replace(
                &mut fixture.store,
                crate::store::Store::open_in_memory().unwrap(),
            );
            let manager = SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                store,
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                fixture.sandbox_base.clone(),
            )
            .unwrap();

            // The production RPC entry, not the test-only apply_locked.
            let replay = manager
                .apply_source_worktree_cohort(mixed.params.clone())
                .await
                .unwrap();
            let fenced_item = replay
                .items
                .iter()
                .find(|item| item.session_id == fixture.session_id)
                .expect("primary item");
            assert_ne!(
                replay, mixed.fenced,
                "the replay did not return the stale receipt"
            );
            assert_eq!(replay.counts.refused, 1);
            if holder {
                assert_eq!(
                    fenced_item.phase,
                    SourceWorktreeSettlementPhaseV1::RecoveryRequired
                );
                assert_eq!(replay.counts.settled, 1);
                assert_eq!(replay.counts.recovery_required, 1);
                assert!(
                    mixed.quarantine.exists(),
                    "a real holder keeps the quarantine"
                );
            } else {
                assert_eq!(fenced_item.phase, SourceWorktreeSettlementPhaseV1::Settled);
                assert_eq!(replay.counts.settled, 2, "{replay:?}");
                assert_eq!(replay.counts.recovery_required, 0);
                assert!(!mixed.quarantine.exists());
                // Once nothing is revivable the stored receipt is returned as is.
                let again = manager
                    .apply_source_worktree_cohort(mixed.params.clone())
                    .await
                    .unwrap();
                assert_eq!(again, replay);
            }
            assert!(replay.finished_at.is_some());
            let _ = &mixed.run;
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn startup_recovery_revives_a_first_proof_fence_in_a_mixed_terminal_run() {
        let mut fixture = IntegratedFixture::new("startup-mixed-first-proof");
        let mixed =
            fixture.commit_mixed_terminal_run_with_first_proof_fence("fixture:startup-mixed");
        let run_id = mixed.run.run_id;
        assert_eq!(
            fixture
                .store
                .list_nonterminal_source_worktree_settlement_run_ids(None, 64)
                .unwrap(),
            vec![run_id],
            "startup recovery selects the fully terminal run for its revivable fence"
        );

        // The reopening pass returns the run to a partial attempt.
        assert!(
            fixture
                .store
                .reopen_source_worktree_quarantine_proof_fences(run_id)
                .unwrap()
        );
        let partial = fixture
            .store
            .get_source_worktree_settlement_run(run_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            partial.state,
            rsi_common::cohort_settlement::SourceWorktreeSettlementRunStateV1::Partial
        );
        assert!(partial.finished_at.is_none(), "{partial:?}");
        assert_eq!(
            (
                partial.counts.settled,
                partial.counts.refused,
                partial.counts.recovery_required
            ),
            (1, 1, 0)
        );

        let store = std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().unwrap(),
        );
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            store,
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
            fixture.sandbox_base.clone(),
        )
        .unwrap();
        manager.recover_source_worktree_settlements().await.unwrap();
        let terminal = manager
            .get_source_worktree_settlement_run(run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(terminal.counts.settled, 2, "{terminal:?}");
        assert_eq!(terminal.counts.refused, 1);
        assert_eq!(terminal.counts.recovery_required, 0);
        assert!(terminal.finished_at.is_some());
        assert!(terminal.items.iter().all(|item| item.phase.is_terminal()));
        assert!(!mixed.quarantine.exists());
        let attempt: (i64, i64, i64, bool, i64) = manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT a.settled_count,a.refused_count,a.recovery_required_count,a.finished_at IS NOT NULL,
                        (SELECT count(*) FROM source_worktree_settlement_reproof_items WHERE run_id=a.run_id)
                 FROM source_worktree_settlement_reproof_runs a WHERE a.run_id=?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!(attempt, (2, 1, 0, true, 1));
        manager.recover_source_worktree_settlements().await.unwrap();
        assert_eq!(
            manager
                .get_source_worktree_settlement_run(run_id)
                .await
                .unwrap()
                .unwrap(),
            terminal,
            "a terminal run is inert on the next recovery pass"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn quarantine_holder_deadline_defers_before_and_after_marker_then_replays() {
        for skip in [0, 1] {
            let mut fixture = IntegratedFixture::new(&format!("holder-deferred-{skip}"));
            let run = fixture.commit_intent(&format!("fixture:holder-deferred-{skip}"));
            let quarantine = fixture.move_to_quarantine(&run);
            fail_next_quarantine_authority_proofs(0);
            crate::session::reaper::slow_quarantine_holder_test_scan_after(skip);

            let (outcome, store) = fixture.replay_run(run.run_id);
            let item = &outcome.receipt.items[0];
            assert_eq!(item.phase, SourceWorktreeSettlementPhaseV1::IntentCommitted);
            assert_eq!(
                item.after_observation.as_deref(),
                Some("quarantine_proof_deferred")
            );
            assert_eq!(item.refusal_code, None);
            assert_eq!(item.before_observation.is_some(), skip == 1);
            assert!(outcome.receipt.finished_at.is_none());
            assert_eq!(quarantine_authority_proof_attempt_count(), skip + 1);
            assert!(quarantine.exists());
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                run.items[0].source_oid
            );
            assert!(
                store
                    .blocking_lock()
                    .list_nonterminal_source_worktree_settlement_run_ids(None, 256)
                    .unwrap()
                    .contains(&run.run_id)
            );

            fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
            let (retry, _) = fixture.replay_run(run.run_id);
            assert_eq!(
                retry.receipt.items[0].phase,
                SourceWorktreeSettlementPhaseV1::Settled,
                "after_observation: {:?}",
                retry.receipt.items[0].after_observation
            );
            assert_eq!(retry.receipt.counts.settled, 1);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn quarantine_holder_present_fences_even_after_deadline_deferral() {
        let mut fixture = IntegratedFixture::new("holder-after-deferred");
        let run = fixture.commit_intent("fixture:holder-after-deferred");
        let quarantine = fixture.move_to_quarantine(&run);
        crate::session::reaper::slow_quarantine_holder_test_scan_after(0);
        let (deferred, store) = fixture.replay_run(run.run_id);
        assert_eq!(
            deferred.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::IntentCommitted
        );
        fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
        fixture
            .quarantine_holder_proc
            .add_fd_holder(&quarantine.join("tracked"));

        let (outcome, store) = fixture.replay_run(run.run_id);
        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired,
            "after_observation: {:?}",
            outcome.receipt.items[0].after_observation
        );
        assert_eq!(
            outcome.receipt.items[0].refusal_code,
            Some(SourceWorktreeSettlementRefusalV1::RecoveryProofFailed)
        );
        assert!(quarantine.exists());
        assert_eq!(
            git(
                &fixture.repository,
                &["rev-parse", &run.items[0].source_ref]
            ),
            run.items[0].source_oid
        );
        fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
        assert_eq!(
            fixture.replay_run(run.run_id).0.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::RecoveryRequired
        );
    }

    /// #270: a fixture replay settles under a provider-cwd admission of its
    /// own, so a test holding the daemon-wide barrier cannot make it surrender.
    /// The scope's lock still excludes a second root, a nested scope gets a
    /// fresh one, and the shared barrier is selected again once it ends.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn fixture_cwd_admission_is_private_to_its_scope() {
        with_fixture_cwd_admission(|| {
            let held = wait_for_settlement_lock(acquire_settlement_cwd_exclusion())
                .expect("the scope's private exclusion is free");
            assert!(
                wait_for_settlement_lock_for(
                    acquire_settlement_cwd_exclusion(),
                    std::time::Duration::from_millis(20),
                )
                .is_none(),
                "the scope's one lock excludes a second root"
            );
            assert!(with_fixture_cwd_admission(|| {
                wait_for_settlement_lock(acquire_settlement_cwd_exclusion()).is_some()
            }));
            drop(held);
        });
        assert!(FIXTURE_CWD_ADMISSION.with(std::cell::Cell::get).is_none());
    }

    /// #274: which lock a unit test's settlement takes. A scope beats a
    /// runtime, which beats the thread, and the scope ends with its body, also
    /// when the body panics. Two runtimes, and two threads outside any runtime,
    /// never share a lock, while every thread of one runtime (the test's, its
    /// blocking pool's and its workers') does.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn test_cwd_admission_is_per_scope_then_runtime_then_thread() {
        let same = |left: TestCwdAdmission, right: TestCwdAdmission| std::ptr::eq(left, right);
        let thread_lock = test_cwd_admission();
        assert!(same(thread_lock, test_cwd_admission()), "stable per thread");
        assert!(runtime_cwd_admission().is_none());
        let other_thread = std::thread::spawn(test_cwd_admission).join().unwrap();
        assert!(!same(thread_lock, other_thread), "one lock per thread");

        let (first, second) = (
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .build()
                .unwrap(),
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
        );
        let first_lock = first.block_on(async { test_cwd_admission() });
        let second_lock = second.block_on(async { test_cwd_admission() });
        assert!(!same(first_lock, second_lock), "one lock per runtime");
        assert!(!same(first_lock, thread_lock));
        let from_blocking = first.block_on(async {
            tokio::task::spawn_blocking(test_cwd_admission)
                .await
                .unwrap()
        });
        let from_worker =
            first.block_on(async { tokio::spawn(async { test_cwd_admission() }).await.unwrap() });
        assert!(
            same(first_lock, from_blocking),
            "a blocking thread is the runtime's"
        );
        assert!(same(first_lock, from_worker), "a worker is the runtime's");
        // Entering a runtime on this thread selects its lock, and leaving it
        // returns the thread's own.
        {
            let _entered = first.enter();
            assert!(same(first_lock, test_cwd_admission()));
        }
        assert!(same(thread_lock, test_cwd_admission()));

        let scoped = new_test_cwd_admission();
        let _entered = first.enter();
        with_cwd_admission(scoped, || {
            assert!(
                same(scoped, test_cwd_admission()),
                "a scope beats a runtime"
            );
            let inner = with_fixture_cwd_admission(test_cwd_admission);
            assert!(
                !same(scoped, inner) && !same(first_lock, inner),
                "a nested scope is fresh"
            );
            assert!(
                same(scoped, test_cwd_admission()),
                "the outer scope returns"
            );
            let panicked = std::panic::catch_unwind(|| {
                with_fixture_cwd_admission(|| {
                    let _own = test_cwd_admission();
                    panic!("a scope body panics");
                })
            });
            assert!(panicked.is_err());
            assert!(
                same(scoped, test_cwd_admission()),
                "a panic ends only its own scope"
            );
        });
        assert!(same(first_lock, test_cwd_admission()), "the scope ended");
    }

    /// #274: a recovery run through a `SessionManager` (the settlement ticker
    /// and startup recovery) never holds the daemon-wide provider-cwd barrier.
    /// A test that settles on that barrier waits one second for it, so a
    /// recovery that held it through a proof made that test surrender before
    /// its own pause or fence. The recovery is paused inside its root's cwd
    /// exclusion; a shared-barrier waiter must still be admitted. It waits far
    /// past the one-second bound, so it fails only if the recovery is the holder.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manager_recovery_does_not_hold_the_daemon_wide_cwd_barrier() {
        let mut fixture = IntegratedFixture::new("manager-recovery-barrier");
        let run = fixture.commit_intent("fixture:manager-recovery-barrier");
        let session_id = fixture.session_id;
        let manager = Arc::new(fixture.into_manager());
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        SETTLEMENT_PROOF_PAUSE.lock().unwrap().insert(
            session_id,
            SettlementProofPause {
                reached: reached_tx,
                release: release_rx,
            },
        );
        let recovery_manager = Arc::clone(&manager);
        let recovery =
            tokio::spawn(
                async move { recovery_manager.recover_source_worktree_settlements().await },
            );
        tokio::task::spawn_blocking(move || {
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("recovery reached its root's proof holding the cwd exclusion")
        })
        .await
        .unwrap();

        // The exclusion is real, and it is the runtime's own lock.
        let own_lock_is_held = test_cwd_admission().try_read().is_err();
        // The guard is dropped at once: the probe never holds the barrier.
        let admitted = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            super::super::spawn_single_flight::acquire_settlement_cwd_exclusion(),
        )
        .await
        .map(drop)
        .is_ok();
        // Release before asserting: a failed probe must not leave the recovery paused.
        release_tx.send(()).unwrap();
        recovery.await.unwrap().unwrap();
        assert!(
            own_lock_is_held,
            "the recovery held its root's exclusion on the test's own lock"
        );
        assert!(
            admitted,
            "a shared-barrier waiter was not admitted while a manager recovery held a root's exclusion"
        );

        let receipt = manager
            .get_source_worktree_settlement_run(run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.counts.settled, 1, "{receipt:#?}");
    }

    /// #271: a holder-deadline deferral on a manual (operator) run, before or
    /// after its removal marker, is retried by the periodic ticker with no
    /// apply replay and no restart, and settles once a complete proof holds.
    /// An automatic run's deferral stays with the automatic pass.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn manual_run_deadline_deferral_is_retried_by_the_ticker_and_settles() {
        for skip in [0, 1] {
            let label = format!("manual-deferred-{skip}");
            let mut fixture = IntegratedFixture::new(&label);
            let run = fixture.commit_intent(&format!("fixture:{label}"));
            let quarantine = fixture.move_to_quarantine(&run);
            fail_next_quarantine_authority_proofs(0);
            crate::session::reaper::slow_quarantine_holder_test_scan_after(skip);

            let (deferred, store) = fixture.replay_run(run.run_id);
            let item = &deferred.receipt.items[0];
            assert_eq!(
                item.phase,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "{label}: after_observation: {:?}",
                item.after_observation
            );
            assert_eq!(
                item.after_observation.as_deref(),
                Some(QUARANTINE_PROOF_DEFERRED_OBSERVATION)
            );
            assert_eq!(item.before_observation.is_some(), skip == 1);
            assert!(quarantine.exists());
            fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
            assert_eq!(
                retryable_deferred_settlement_runs(&fixture.store).unwrap(),
                vec![run.run_id]
            );

            let executor = paused_settlement_runtime();
            let _runtime_context = executor.enter();
            let manager = fixture.into_manager();
            let retried = executor
                .block_on(async {
                    drain_background_startup_tick().await;
                    manager.retry_deferred_settlements().await
                })
                .unwrap();
            assert_eq!(retried, vec![run.run_id], "{label}");
            let after = executor
                .block_on(manager.get_source_worktree_settlement_run(run.run_id))
                .unwrap()
                .unwrap();
            assert_eq!(
                after.items[0].phase,
                SourceWorktreeSettlementPhaseV1::Settled,
                "{label}: after_observation: {:?}",
                after.items[0].after_observation
            );
            assert_eq!(after.counts.settled, 1);
            assert!(after.finished_at.is_some());
            assert!(!quarantine.exists());
            assert!(
                executor
                    .block_on(manager.retry_deferred_settlements())
                    .unwrap()
                    .is_empty(),
                "a settled run is no longer a retry candidate"
            );
        }

        let mut fixture = IntegratedFixture::new("automatic-deferred");
        let audit = fixture.audit();
        let run = fixture.run_from_audit(&audit, "auto:automatic-deferred");
        assert!(matches!(
            fixture
                .store
                .insert_source_worktree_settlement_run_as(
                    &run,
                    &rsi_common::cohort_settlement::SourceWorktreeSettlementAttributionV1 {
                        origin:
                            rsi_common::cohort_settlement::SourceWorktreeSettlementOriginV1::Automatic,
                        agent_session_id: None,
                    },
                )
                .expect("commit automatic settlement intent"),
            InsertSettlementRunOutcome::Inserted
        ));
        fixture
            .store
            .advance_source_worktree_settlement_item(
                run.run_id,
                run.items[0].session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                None,
                Some(QUARANTINE_PROOF_DEFERRED_OBSERVATION),
                None,
            )
            .unwrap();
        assert_eq!(
            fixture
                .journal_item(run.run_id)
                .after_observation
                .as_deref(),
            Some(QUARANTINE_PROOF_DEFERRED_OBSERVATION)
        );
        assert_eq!(
            retryable_deferred_settlement_runs(&fixture.store).unwrap(),
            Vec::<Uuid>::new(),
            "the automatic pass owns an automatic run's deferral"
        );
    }

    /// #271 on rolling's detached apply (#2866): a retryable run this process
    /// is already driving is left to its driver, whose claim survives the
    /// refused one, and is retried once the driver has ended.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn deferred_retry_leaves_a_run_this_process_is_driving_to_its_driver() {
        let mut fixture = IntegratedFixture::new("deferred-in-flight");
        let run = fixture.commit_intent("fixture:deferred-in-flight");
        let quarantine = fixture.move_to_quarantine(&run);
        fail_next_quarantine_authority_proofs(0);
        crate::session::reaper::slow_quarantine_holder_test_scan_after(0);
        let (deferred, store) = fixture.replay_run(run.run_id);
        assert_eq!(
            deferred.receipt.items[0].after_observation.as_deref(),
            Some(QUARANTINE_PROOF_DEFERRED_OBSERVATION)
        );
        fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
        let executor = paused_settlement_runtime();
        let _runtime_context = executor.enter();
        let manager = fixture.into_manager();
        let driver = try_mark_settlement_run_in_flight(run.run_id).expect("claim the run");
        let retried = executor
            .block_on(async {
                drain_background_startup_tick().await;
                manager.retry_deferred_settlements().await
            })
            .unwrap();
        assert!(retried.is_empty(), "a driven run is left to its driver");
        assert!(
            settlement_run_in_flight(run.run_id),
            "a refused claim leaves the driver's claim in place"
        );
        let held = executor
            .block_on(manager.get_source_worktree_settlement_run(run.run_id))
            .unwrap()
            .unwrap();
        assert_eq!(
            held.items[0].phase,
            SourceWorktreeSettlementPhaseV1::IntentCommitted
        );
        drop(driver);
        assert_eq!(
            executor
                .block_on(manager.retry_deferred_settlements())
                .unwrap(),
            vec![run.run_id]
        );
        let after = executor
            .block_on(manager.get_source_worktree_settlement_run(run.run_id))
            .unwrap()
            .unwrap();
        assert_eq!(
            after.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Settled,
            "after_observation: {:?}",
            after.items[0].after_observation
        );
        assert!(!quarantine.exists());
    }

    /// The ticker's retry resumes only unfinished items; a retained intent
    /// that can be reproved is resumed by the startup pass alone.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn deferred_retry_resumes_only_unfinished_items() {
        let mut fixture = IntegratedFixture::new("deferred-retry-scope");
        let run = fixture.commit_intent("fixture:deferred-retry-scope");
        let unfinished = fixture.journal_item(run.run_id);
        assert!(RecoveryPass::DeferredRetry.resumes(&unfinished));
        assert!(RecoveryPass::Startup.resumes(&unfinished));
        let mut retained = unfinished;
        retained.phase = SourceWorktreeSettlementPhaseV1::RecoveryRequired;
        retained.refusal_code = Some("recovery_proof_failed".into());
        retained.after_observation = Some(
            "retained intent followed an earlier terminal stop and cannot prove no prior effect"
                .into(),
        );
        retained.before_observation = None;
        retained.reserved_effects = 0;
        retained.active_effects = 0;
        assert!(retained_intent_can_be_reproved(&retained));
        assert!(RecoveryPass::Startup.resumes(&retained));
        assert!(!RecoveryPass::DeferredRetry.resumes(&retained));
    }

    /// #273: a manual run's first attempt that surrenders on a quarantined
    /// root, behind the provider-cwd admission or on a busy root lock, records
    /// why on the item and keeps its phase and quarantine. The periodic ticker
    /// then settles it once the wait clears, with no apply replay or restart.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn quarantined_surrender_is_recorded_and_settled_by_the_ticker() {
        for reason in [
            SettlementSurrender::RuntimeStateContended,
            SettlementSurrender::RootBusy,
        ] {
            let label = format!("surrender-{reason:?}").to_lowercase();
            let mut fixture = IntegratedFixture::new(&label);
            let run = fixture.commit_intent(&format!("fixture:{label}"));
            let quarantine = fixture.move_to_quarantine(&run);

            let (surrendered, store) = match reason {
                SettlementSurrender::RuntimeStateContended => {
                    fixture.replay_run_with_cwd_admission_held(run.run_id)
                }
                SettlementSurrender::RootBusy => {
                    let custody_id = fixture.custody_id;
                    let (held_tx, held_rx) = std::sync::mpsc::channel();
                    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
                    let holder = std::thread::spawn(move || {
                        let _guard = crate::store::sandbox_custody::lock_custody_root(custody_id);
                        held_tx.send(()).unwrap();
                        release_rx
                            .recv_timeout(std::time::Duration::from_secs(30))
                            .unwrap();
                    });
                    held_rx
                        .recv_timeout(std::time::Duration::from_secs(30))
                        .unwrap();
                    crate::store::custody_lock_order::set_bounded_wait_for_test(Some(
                        std::time::Duration::from_millis(1),
                    ));
                    let replayed = fixture.replay_run(run.run_id);
                    crate::store::custody_lock_order::set_bounded_wait_for_test(None);
                    release_tx.send(()).unwrap();
                    holder.join().unwrap();
                    replayed
                }
            };
            let item = &surrendered.receipt.items[0];
            assert_eq!(
                item.phase,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "{label}: after_observation: {:?}",
                item.after_observation
            );
            assert_eq!(
                item.after_observation.as_deref(),
                Some(reason.observation().as_str()),
                "{label}"
            );
            assert_eq!(item.refusal_code, None);
            assert!(surrendered.receipt.finished_at.is_none());
            assert!(quarantine.exists());
            assert_eq!(
                git(
                    &fixture.repository,
                    &["rev-parse", &run.items[0].source_ref]
                ),
                run.items[0].source_oid
            );
            fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
            assert_eq!(
                retryable_deferred_settlement_runs(&fixture.store).unwrap(),
                vec![run.run_id]
            );

            let executor = paused_settlement_runtime();
            let _runtime_context = executor.enter();
            let manager = fixture.into_manager();
            let retried = executor
                .block_on(async {
                    drain_background_startup_tick().await;
                    manager.retry_deferred_settlements().await
                })
                .unwrap();
            assert_eq!(retried, vec![run.run_id], "{label}");
            let after = executor
                .block_on(manager.get_source_worktree_settlement_run(run.run_id))
                .unwrap()
                .unwrap();
            assert_eq!(
                after.items[0].phase,
                SourceWorktreeSettlementPhaseV1::Settled,
                "{label}: after_observation: {:?}",
                after.items[0].after_observation
            );
            assert_eq!(after.counts.settled, 1);
            assert!(after.finished_at.is_some());
            assert!(!quarantine.exists());
            assert!(
                executor
                    .block_on(manager.retry_deferred_settlements())
                    .unwrap()
                    .is_empty(),
                "a settled run is no longer a retry candidate"
            );
        }
    }

    /// #283: settlement and a provider launch contend on the one real
    /// daemon-wide barrier, as in production. Every other unit test settles on a
    /// lock of its own (#274), so this is the one that proves the pairing: a
    /// replay of a quarantined root surrenders and records
    /// `settlement_surrendered` while a launch holds the barrier, and settles
    /// once the launch releases it. The test lock override is not in play
    /// ([`with_real_cwd_barrier`] selects the real barrier and serialises its
    /// users), and the launch side is `acquire_provider_cwd_admission`, the call
    /// every launch path makes.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_surrenders_while_a_launch_holds_the_real_barrier_and_settles_after() {
        let mut fixture = IntegratedFixture::new("real-barrier-launch");
        let run = fixture.commit_intent("fixture:real-barrier-launch");
        let quarantine = fixture.move_to_quarantine(&run);
        with_real_cwd_barrier(|| {
            let launch = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("launch runtime");
            let launching = launch
                .block_on(super::super::spawn_single_flight::acquire_provider_cwd_admission());

            let (surrendered, store) = fixture.replay_run(run.run_id);
            let item = &surrendered.receipt.items[0];
            assert_eq!(
                item.phase,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                "after_observation: {:?}",
                item.after_observation
            );
            assert_eq!(
                item.after_observation.as_deref(),
                Some(
                    SettlementSurrender::RuntimeStateContended
                        .observation()
                        .as_str()
                ),
                "a launch holding the real barrier makes settlement surrender"
            );
            assert!(surrendered.receipt.finished_at.is_none());
            assert!(quarantine.exists(), "the surrender leaves the quarantine");
            fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
            assert_eq!(
                retryable_deferred_settlement_runs(&fixture.store).unwrap(),
                vec![run.run_id]
            );

            drop(launching);
            let (settled, _store) = fixture.replay_run(run.run_id);
            let item = &settled.receipt.items[0];
            assert_eq!(
                item.phase,
                SourceWorktreeSettlementPhaseV1::Settled,
                "after_observation: {:?}",
                item.after_observation
            );
            assert!(settled.receipt.finished_at.is_some());
            assert!(!quarantine.exists());
        });
    }

    /// #273: a surrender keeps a deadline deferral already on record.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn surrender_keeps_a_recorded_deferral() {
        let mut fixture = IntegratedFixture::new("surrender-keeps-deferral");
        let run = fixture.commit_intent("fixture:surrender-keeps-deferral");
        fixture.move_to_quarantine(&run);
        fixture
            .store
            .advance_source_worktree_settlement_item(
                run.run_id,
                run.items[0].session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                None,
                Some(QUARANTINE_PROOF_DEFERRED_OBSERVATION),
                None,
            )
            .unwrap();
        record_settlement_surrenders(&fixture.store, run.run_id, |_| {
            Some(SettlementSurrender::RootBusy)
        })
        .unwrap();
        assert_eq!(
            fixture
                .journal_item(run.run_id)
                .after_observation
                .as_deref(),
            Some(QUARANTINE_PROOF_DEFERRED_OBSERVATION)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn apply_continues_after_a_middle_item_refusal_and_replays_terminal_receipt() {
        let mut fixture = IntegratedFixture::new("middle-refusal");
        fixture.add_second_root("middle-refusal-second");
        fixture.add_second_root("middle-refusal-third");
        let run = fixture.commit_intent_for_all_eligible("fixture:middle-refusal");
        let journal = fixture
            .store
            .list_source_worktree_settlement_journal_items(run.run_id)
            .unwrap();
        let middle = &journal[1];
        let dirty = Path::new(&middle.sandbox_root).join("retained-work.txt");
        std::fs::write(&dirty, "retain this work").unwrap();
        let (outcome, store) = fixture.replay_run(run.run_id);
        assert_eq!(outcome.receipt.counts.settled, 2, "{:?}", outcome.receipt);
        assert_eq!(outcome.receipt.counts.refused, 1);
        assert!(outcome.receipt.finished_at.is_some());
        assert!(
            outcome
                .receipt
                .items
                .iter()
                .all(|item| item.phase.is_terminal())
        );
        assert_eq!(std::fs::read_to_string(dirty).unwrap(), "retain this work");
        assert!(
            !store
                .blocking_lock()
                .path_is_inside_settlement_root(Path::new(&middle.sandbox_root))
                .unwrap()
        );
        fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
        let (replay, _) = fixture.replay_run(run.run_id);
        assert_eq!(replay.receipt, outcome.receipt);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn startup_recovers_open_partial_run_past_runtime_refusal() {
        let mut fixture = IntegratedFixture::new("partial-runtime-refusal");
        fixture.add_second_root("partial-second");
        fixture.add_second_root("partial-third");
        let run = fixture.commit_intent_for_all_eligible("fixture:partial-runtime-refusal");
        let journal = fixture
            .store
            .list_source_worktree_settlement_journal_items(run.run_id)
            .unwrap();
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().unwrap(),
        )));
        fixture.with_quarantine_holder_test_proc(|| {
            git_worktree::with_repository_mutation(&fixture.repository, || {
                settle_one_item_locked(
                    &store,
                    &Arc::new(tokio::sync::RwLock::new(HashMap::new())),
                    &Arc::new(tokio::sync::RwLock::new(HashMap::new())),
                    run.run_id,
                    &run.target_ref,
                    &fixture.repository,
                    &fixture.sandbox_base,
                    &journal[0],
                    false,
                )
            })
            .unwrap()
        });
        let store = Arc::try_unwrap(store).ok().unwrap().into_inner();
        store
            .mark_source_worktree_settlement_refused(
                run.run_id,
                journal[1].session_id,
                SourceWorktreeSettlementPhaseV1::IntentCommitted,
                SourceWorktreeSettlementRefusalV1::RuntimeStateContended.as_str(),
                "active runtime map was contended before effect",
            )
            .unwrap();
        let partial = store
            .get_source_worktree_settlement_run(run.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(partial.counts.settled, 1);
        assert_eq!(partial.counts.refused, 1);
        assert!(partial.finished_at.is_none());
        assert_eq!(
            store
                .list_nonterminal_source_worktree_settlement_run_ids(None, 64)
                .unwrap(),
            vec![run.run_id]
        );
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        executor.block_on(async {
            let manager = SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                store,
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                fixture.sandbox_base.clone(),
            )
            .unwrap();
            tokio::task::yield_now().await;
            manager.recover_source_worktree_settlements().await.unwrap();
            let receipt = manager
                .get_source_worktree_settlement_run(run.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(receipt.counts.settled, 2, "{receipt:?}");
            assert_eq!(receipt.counts.refused, 1);
            assert!(receipt.finished_at.is_some());
            assert!(receipt.items.iter().all(|item| item.phase.is_terminal()));
            assert!(Path::new(&journal[1].sandbox_root).is_dir());
            assert!(
                manager
                    .store
                    .lock()
                    .await
                    .list_nonterminal_source_worktree_settlement_run_ids(None, 64)
                    .unwrap()
                    .is_empty()
            );
            manager.recover_source_worktree_settlements().await.unwrap();
            assert_eq!(
                manager
                    .get_source_worktree_settlement_run(run.run_id)
                    .await
                    .unwrap()
                    .unwrap(),
                receipt
            );
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_apply_keeps_cwd_exclusion_and_finishes_projection() {
        let fixture = IntegratedFixture::new("cancelled-apply");
        let audit = fixture.audit();
        let params = ApplySourceWorktreeCohortParams {
            repository_identity: fixture.target.repository_identity.clone(),
            plan_digest: audit.report.plan_digest.clone(),
            authorization: audit.report.authorization_phrase.clone().unwrap(),
            idempotency_key: "fixture:cancelled-apply".into(),
        };
        let bus = Arc::new(crate::bus::EventBus::new(16));
        let mut events = bus.subscribe();
        let manager = Arc::new(
            SessionManager::new(
                bus,
                fixture.store,
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                fixture.sandbox_base,
            )
            .unwrap(),
        );
        manager.restore_sessions().await.unwrap();
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        SETTLEMENT_PROOF_PAUSE.lock().unwrap().insert(
            fixture.session_id,
            SettlementProofPause {
                reached: reached_tx,
                release: release_rx,
            },
        );
        let client_manager = Arc::clone(&manager);
        let client_params = params.clone();
        let client = tokio::spawn(async move {
            client_manager
                .apply_source_worktree_cohort(client_params)
                .await
        });
        tokio::task::spawn_blocking(move || {
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .unwrap()
        })
        .await
        .unwrap();
        client.abort();
        assert!(client.await.unwrap_err().is_cancelled());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                test_launch_cwd_admission(),
            )
            .await
            .is_err(),
            "cancelled client must not release the effect's cwd fence"
        );
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let event = events.recv().await.unwrap();
                if matches!(&*event, DaemonEvent::SessionArchived { session_id, .. } if *session_id == fixture.session_id) {
                    break;
                }
            }
        }).await.expect("daemon driver completes runtime projection after client cancellation");
        let replay = manager.apply_source_worktree_cohort(params).await.unwrap();
        assert_eq!(replay.counts.settled, 1);
        assert!(replay.finished_at.is_some());
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .get_session(fixture.session_id)
                .unwrap()
                .unwrap()
                .sandbox_cleanup_state,
            Some(rsi_common::types::SandboxCleanupState::Purged)
        );
    }

    /// A bounded custody wait leaves only that item pending. Later roots still
    /// settle, and replay finishes the untouched root after contention clears.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn stripe_timeout_retains_one_item_and_finishes_later_items() {
        let mut fixture = IntegratedFixture::new("stripe-timeout-first-item");
        fixture.add_second_root_in_other_stripe("stripe-timeout-second-item");
        let run = fixture.commit_intent_for_all_eligible("fixture:stripe-timeout-multi");
        let item_count = run.items.len();
        let receipt = fixture
            .store
            .get_source_worktree_settlement_run(run.run_id)
            .expect("read fixture receipt")
            .expect("fixture receipt");
        let journal_custodies = fixture
            .store
            .list_source_worktree_settlement_journal_items(run.run_id)
            .expect("journal")
            .iter()
            .map(|item| item.custody_id)
            .collect::<Vec<_>>();
        let first_custody = *journal_custodies.first().expect("first journal item");
        // Holding the first item's stripe must not also contend the other roots:
        // the test pins the typed retry on the FIRST item only.
        let first_shard = crate::store::sandbox_custody::custody_root_lock_shard(first_custody);
        assert!(
            journal_custodies[1..].iter().all(|id| {
                crate::store::sandbox_custody::custody_root_lock_shard(*id) != first_shard
            }),
            "fixture roots must use distinct custody lock stripes"
        );
        let idempotency_key = receipt.idempotency_key.clone();
        let plan_digest = receipt.plan_digest.as_str().to_string();
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().expect("replacement fixture Store"),
        )));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let replay = |start: ApplyStart| {
            fixture.with_quarantine_holder_test_proc(|| {
                git_worktree::with_repository_mutation(&fixture.repository, || {
                    apply_locked(
                        start,
                        &fixture.target.repository_identity,
                        &idempotency_key,
                        &plan_digest,
                        "",
                        "",
                        &fixture.sandbox_base,
                        &store,
                        &active,
                        &completed,
                    )
                })
            })
        };

        crate::store::custody_lock_order::set_bounded_wait_for_test(Some(
            std::time::Duration::from_millis(60),
        ));
        let contended = crate::store::sandbox_custody::lock_custody_root(first_custody);
        // The first item's stripe is held elsewhere; `lock_custody_root` on this
        // same thread is non-reentrant, so the bounded acquisition surrenders.
        let outcome = replay(ApplyStart::Replay(receipt.clone()))
            .expect("contended root does not abort the run");
        drop(contended);
        crate::store::custody_lock_order::set_bounded_wait_for_test(None);

        assert_eq!(
            outcome.receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::IntentCommitted
        );
        assert_eq!(outcome.receipt.counts.recovery_required, 0);
        assert_eq!(outcome.receipt.counts.settled, item_count as u32 - 1);
        assert!(outcome.receipt.finished_at.is_none());
        let replayed =
            replay(ApplyStart::Replay(outcome.receipt.clone())).expect("retry pending root");
        assert_eq!(replayed.receipt.counts.settled, item_count as u32);
        assert!(replayed.receipt.finished_at.is_some());
        for item in &replayed.receipt.items {
            let root = fixture.sandbox_base.join(item.session_id.to_string());
            assert_eq!(item.phase, SourceWorktreeSettlementPhaseV1::Settled);
            assert!(!root.exists(), "retried root was reclaimed");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn settlement_lock_waits_out_brief_runtime_and_store_contention() {
        let runtime = tokio::sync::RwLock::new(());
        let store = tokio::sync::Mutex::new(());
        std::thread::scope(|scope| {
            let runtime_guard = runtime.blocking_write();
            let store_guard = store.blocking_lock();
            let release = scope.spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                drop(runtime_guard);
                drop(store_guard);
            });
            let _runtime =
                wait_for_settlement_lock(runtime.read()).expect("wait out runtime writer");
            let _store = wait_for_settlement_lock(store.lock()).expect("wait out Store writer");
            release.join().unwrap();
        });
    }

    /// A Store held past the settlement lock bound must retain the root: the
    /// item is refused `RuntimeStateContended` before any filesystem or Git
    /// effect, and the receipt (not a count) is what the failure shows.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn store_contention_under_custody_fence_retains_untouched_root() {
        // Longer than the one-second settlement lock bound, with slack.
        const SETTLEMENT_STORE_HOLD: std::time::Duration = std::time::Duration::from_secs(3);
        let mut fixture = IntegratedFixture::new("store-contention-fence");
        let run = fixture.commit_intent("fixture:store-contention-fence");
        let item = fixture
            .store
            .list_source_worktree_settlement_journal_items(run.run_id)
            .unwrap()
            .remove(0);
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().unwrap(),
        )));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        SETTLEMENT_PROOF_PAUSE.lock().unwrap().insert(
            item.session_id,
            SettlementProofPause {
                reached: reached_tx,
                release: release_rx,
            },
        );
        let store_holder = store.blocking_lock();
        let settled = std::thread::scope(|scope| {
            // #270/#273: the subject is Store contention. A private cwd
            // admission keeps a co-tenant holding the daemon-wide barrier from
            // making this root surrender before it reaches the fence, and keeps
            // this test's long hold off that barrier.
            let settle = scope.spawn(|| {
                with_fixture_cwd_admission(|| {
                    git_worktree::with_repository_mutation(&fixture.repository, || {
                        settle_one_item_locked(
                            &store,
                            &active,
                            &completed,
                            run.run_id,
                            &run.target_ref,
                            &fixture.repository,
                            &fixture.sandbox_base,
                            &item,
                            false,
                        )
                    })
                })
            });
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("settlement reached the custody fence");
            release_tx.send(()).unwrap();
            // The fence's Store wait is bounded at one second from here; hold
            // the Store well past it so the refusal path runs, then release
            // so the refusal can be journaled.
            std::thread::sleep(SETTLEMENT_STORE_HOLD);
            drop(store_holder);
            settle.join().unwrap().unwrap()
        });
        assert!(!settled);
        let receipt = store
            .blocking_lock()
            .get_source_worktree_settlement_run(run.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            receipt.items[0].phase,
            SourceWorktreeSettlementPhaseV1::Refused,
            "{receipt:#?}"
        );
        assert_eq!(
            receipt.items[0].refusal_code,
            Some(SourceWorktreeSettlementRefusalV1::RuntimeStateContended),
            "{receipt:#?}"
        );
        assert_eq!(receipt.counts.settled, 0, "{receipt:#?}");
        assert!(fixture.allocation.root.is_dir());
        assert!(
            !store
                .blocking_lock()
                .path_is_inside_settlement_root(&fixture.allocation.root)
                .unwrap()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn replay_contention_never_terminalizes_a_prior_effect() {
        for effect_started in [false, true] {
            let mut fixture = IntegratedFixture::new("replay-runtime-contention");
            let run = fixture.commit_intent("fixture:replay-runtime-contention");
            if effect_started {
                let quarantine = fixture.move_to_quarantine(&run);
                fixture.persist_quarantine_marker(&run, &quarantine);
            }
            let item = fixture
                .store
                .list_source_worktree_settlement_journal_items(run.run_id)
                .unwrap()
                .remove(0);
            let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
                &mut fixture.store,
                crate::store::Store::open_in_memory().unwrap(),
            )));
            let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
            let writer = active.blocking_write();
            let settled = git_worktree::with_repository_mutation(&fixture.repository, || {
                settle_one_item_locked(
                    &store,
                    &active,
                    &completed,
                    run.run_id,
                    &run.target_ref,
                    &fixture.repository,
                    &fixture.sandbox_base,
                    &item,
                    true,
                )
            })
            .unwrap();
            drop(writer);
            assert!(!settled);
            let receipt = store
                .blocking_lock()
                .get_source_worktree_settlement_run(run.run_id)
                .unwrap()
                .unwrap();
            assert_eq!(receipt.counts.recovery_required, 0);
            if effect_started {
                assert_eq!(
                    receipt.items[0].phase,
                    SourceWorktreeSettlementPhaseV1::IntentCommitted
                );
                assert!(receipt.finished_at.is_none());
                fixture.store = Arc::try_unwrap(store).ok().unwrap().into_inner();
                assert_eq!(fixture.replay_run(run.run_id).0.receipt.counts.settled, 1);
            } else {
                assert_eq!(
                    receipt.items[0].phase,
                    SourceWorktreeSettlementPhaseV1::Refused
                );
                assert_eq!(
                    receipt.items[0].refusal_code,
                    Some(SourceWorktreeSettlementRefusalV1::RuntimeStateContended)
                );
                assert!(fixture.allocation.root.is_dir());
                assert!(
                    !store
                        .blocking_lock()
                        .path_is_inside_settlement_root(&fixture.allocation.root)
                        .unwrap()
                );
            }
        }
    }

    /// Readiness restoration must leave an unproven journal-owned root
    /// protected from every public lifecycle entry point until recovery runs.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readiness_restore_keeps_unproven_roots_fenced_for_launch_cleanup_and_archive() {
        let mut fixture = IntegratedFixture::new("readiness-pending-proof");
        for index in 1..140 {
            fixture.add_second_root(&format!("readiness-{index}"));
        }
        let run = fixture.commit_intent_for_all_eligible("fixture:readiness-pending-proof");
        let root = fixture.allocation.root.clone();
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            std::mem::replace(
                &mut fixture.store,
                crate::store::Store::open_in_memory().unwrap(),
            ),
            false,
            fixture._directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
            fixture.sandbox_base.clone(),
        )
        .unwrap();
        let started = std::time::Instant::now();
        manager.restore_sessions().await.unwrap();
        eprintln!(
            "RPC-gating restore with 140 proof-pending roots: {:?}",
            started.elapsed()
        );
        let receipt = manager
            .get_source_worktree_settlement_run(run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.items.len(), 140);
        assert!(
            receipt
                .items
                .iter()
                .all(|item| item.phase == SourceWorktreeSettlementPhaseV1::IntentCommitted)
        );
        assert!(
            manager
                .store
                .lock()
                .await
                .session_or_custody_has_settlement_fence(fixture.session_id)
                .unwrap()
        );
        assert!(
            manager
                .store
                .lock()
                .await
                .path_is_inside_settlement_root(&root.join("future-child"))
                .unwrap()
        );
        let launch = manager
            .continue_session(fixture.session_id, "continue".into())
            .await
            .unwrap_err();
        assert!(
            launch.to_string().contains("settlement journal fences"),
            "{launch}"
        );
        let archive = manager
            .archive_session(fixture.session_id)
            .await
            .unwrap_err();
        assert!(
            archive.to_string().contains("settlement journal fences"),
            "{archive}"
        );
        let cleanup = super::super::lifecycle::classify_cleanup_session_runtime(
            fixture.session_id,
            &manager.active,
            &manager.completed,
            &manager.store,
        )
        .await;
        assert_eq!(
            cleanup,
            crate::sandbox::cleanup::CleanupDecision::Blocked(
                crate::sandbox::cleanup::CleanupBlockedReason::ConcurrentDrift,
            )
        );
        assert!(root.is_dir());
        assert_eq!(
            manager
                .store
                .lock()
                .await
                .get_session(fixture.session_id)
                .unwrap()
                .unwrap()
                .status,
            SessionStatus::Completed
        );
        assert_eq!(
            manager
                .get_source_worktree_settlement_run(run.run_id)
                .await
                .unwrap()
                .unwrap(),
            receipt
        );
    }

    /// Like the large retained hub run: committed intents, one real runtime
    /// contention miss and one transient probe error. A queued provider launch
    /// also proves the global cwd barrier opens between roots during recovery.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_many_intents_keep_untouched_roots_retryable_and_admit_providers_between_roots()
    {
        let mut fixture = IntegratedFixture::new("startup-many-intents");
        for index in 1..24 {
            fixture.add_second_root(&format!("retained-{index}"));
        }
        let run = fixture.commit_intent_for_all_eligible("fixture:startup-many-intents");
        let journal = fixture
            .store
            .list_source_worktree_settlement_journal_items(run.run_id)
            .unwrap();
        SETTLEMENT_RECOVERY_PROBE_FAILURE
            .lock()
            .unwrap()
            .insert(journal[10].session_id);
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        SETTLEMENT_PROOF_PAUSE.lock().unwrap().insert(
            journal[0].session_id,
            SettlementProofPause {
                reached: reached_tx,
                release: release_rx,
            },
        );
        let manager = Arc::new(
            SessionManager::new(
                Arc::new(crate::bus::EventBus::new(16)),
                std::mem::replace(
                    &mut fixture.store,
                    crate::store::Store::open_in_memory().unwrap(),
                ),
                false,
                fixture._directory.path().join("daemon.sock"),
                None,
                Vec::new(),
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default()),
                fixture.sandbox_base.clone(),
            )
            .unwrap(),
        );
        let recovery_manager = Arc::clone(&manager);
        let recovery =
            tokio::spawn(
                async move { recovery_manager.recover_source_worktree_settlements().await },
            );
        tokio::task::spawn_blocking(move || {
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .unwrap()
        })
        .await
        .unwrap();
        let active_writer = manager.active.write().await;
        let admission = test_launch_cwd_admission();
        tokio::pin!(admission);
        assert!(
            futures::poll!(&mut admission).is_pending(),
            "root effect excludes provider admission"
        );
        release_tx.send(()).unwrap();
        let admission = tokio::time::timeout(std::time::Duration::from_secs(60), admission)
            .await
            .expect("queued provider admitted between recovery roots");
        // A real admitted launch also needs the repository mutex. Recovery
        // must release it before waiting for the next root's CWD exclusion.
        let repository = fixture.repository.clone();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking(move || {
                git_worktree::with_repository_mutation(&repository, || Ok(()))
            }),
        )
        .await
        .expect("admitted provider can acquire the repository mutex between recovery roots")
        .expect("repository observer task")
        .expect("repository observer");
        // The first root had to surrender its runtime wait before the provider
        // could get the cwd guard. The cohort is still running at this point.
        let partial = manager
            .get_source_worktree_settlement_run(run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            partial.items[0].refusal_code,
            Some(SourceWorktreeSettlementRefusalV1::RuntimeStateContended)
        );
        assert!(partial.finished_at.is_none());
        drop(active_writer);
        drop(admission);
        recovery.await.unwrap().unwrap();
        let receipt = manager
            .get_source_worktree_settlement_run(run.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.counts.settled, 22, "{receipt:?}");
        assert_eq!(receipt.counts.refused, 1);
        assert_eq!(receipt.counts.unattempted, 1);
        assert_eq!(receipt.counts.recovery_required, 0);
        assert!(receipt.finished_at.is_some());
        for item in &receipt.items {
            let root = fixture.sandbox_base.join(item.session_id.to_string());
            if item.phase != SourceWorktreeSettlementPhaseV1::Settled {
                assert!(
                    !manager
                        .store
                        .lock()
                        .await
                        .path_is_inside_settlement_root(&root)
                        .unwrap(),
                    "untouched root remains eligible for a fresh audit"
                );
                assert!(root.is_dir());
                assert_eq!(
                    git(&fixture.repository, &["rev-parse", &item.source_ref]),
                    item.expected_source_oid.as_str()
                );
            }
        }
        // Both untouched roots can be selected again by an ordinary audit.
        let audit = manager
            .audit_source_worktree_cohort(fixture.target.repository_identity.clone())
            .await
            .unwrap();
        assert_eq!(audit.counts.eligible, 2);
    }

    /// Issue #606: a settlement item paused while holding the repository mutex
    /// and its custody stripe must not pin the Store for a contended admission
    /// on the same stripe, and must finish once released.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn paused_settlement_proof_neither_pins_store_nor_deadlocks_contended_admission() {
        let mut fixture = IntegratedFixture::new("pause-settlement-proof");
        let run = fixture.commit_intent("fixture:pause-settlement-proof");
        let receipt = fixture
            .store
            .get_source_worktree_settlement_run(run.run_id)
            .expect("read fixture receipt")
            .expect("fixture receipt");
        let idempotency_key = receipt.idempotency_key.clone();
        let plan_digest = receipt.plan_digest.as_str().to_string();
        let custody_id = fixture.custody_id;
        let session_id = fixture.session_id;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().expect("replacement fixture Store"),
        )));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        SETTLEMENT_PROOF_PAUSE.lock().unwrap().insert(
            session_id,
            SettlementProofPause {
                reached: reached_tx,
                release: release_rx,
            },
        );
        let repository = fixture.repository.clone();
        let repository_identity = fixture.target.repository_identity.clone();
        let sandbox_base = fixture.sandbox_base.clone();
        let holder_root = fixture.quarantine_holder_proc.proc_root().to_path_buf();
        let holder_uid = fixture.quarantine_holder_proc.uid();
        std::thread::scope(|scope| {
            let settle = scope.spawn(|| {
                crate::session::reaper::with_quarantine_holder_test_proc(
                    &holder_root,
                    holder_uid,
                    || {
                        git_worktree::with_repository_mutation(&repository, || {
                            apply_locked(
                                ApplyStart::Replay(receipt),
                                &repository_identity,
                                &idempotency_key,
                                &plan_digest,
                                "",
                                "",
                                &sandbox_base,
                                &store,
                                &active,
                                &completed,
                            )
                        })
                    },
                )
            });
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("settlement paused holding the repository and stripe");

            let contended =
                crate::store::custody_lock_order::admission_contention_signal(custody_id);
            let admission_store = Arc::clone(&store);
            let admission = runtime.spawn(async move {
                let (_store, _root) = crate::store::custody_lock_order::lock_store_then_root(
                    &admission_store,
                    custody_id,
                )
                .await;
            });
            contended
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("admission reached the busy stripe");
            assert!(
                !admission.is_finished(),
                "admission waits for the stripe the paused settlement holds"
            );
            runtime.block_on(async {
                for _ in 0..5 {
                    let store = Arc::clone(&store);
                    tokio::time::timeout(std::time::Duration::from_secs(5), async move {
                        store.lock().await.get_session(Uuid::new_v4())
                    })
                    .await
                    .expect("an unrelated Store RPC is not pinned by the queued admission")
                    .expect("unrelated query");
                }
            });

            release_tx.send(()).expect("release the proof");
            settle
                .join()
                .expect("settlement thread")
                .expect("settlement settles after the proof resumes");
            runtime
                .block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(60), admission).await
                })
                .expect("admission settles after the stripe frees")
                .expect("admission task");
        });
    }

    /// Issue #2081: a provider launch holds cwd admission and then waits for
    /// the repository mutex (launch.rs, archive_cleanup.rs). While a cohort
    /// run is mid-flight such a launch must not starve the remaining roots:
    /// apply takes the cwd exclusion before the mutex, per root, so the launch
    /// is admitted between roots and every root still settles.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn overlapping_launch_does_not_refuse_remaining_cohort_roots() {
        let mut fixture = IntegratedFixture::new("overlap-launch-first-root");
        fixture.add_second_root_in_other_stripe("overlap-launch-second-root");
        fixture.add_second_root("overlap-launch-third-root");
        let run = fixture.commit_intent_for_all_eligible("fixture:overlap-launch");
        let item_count = run.items.len();
        assert_eq!(item_count, 3);
        let receipt = fixture
            .store
            .get_source_worktree_settlement_run(run.run_id)
            .expect("read fixture receipt")
            .expect("fixture receipt");
        let first_session = fixture
            .store
            .list_source_worktree_settlement_journal_items(run.run_id)
            .expect("journal")
            .first()
            .expect("first journal item")
            .session_id;
        let idempotency_key = receipt.idempotency_key.clone();
        let plan_digest = receipt.plan_digest.as_str().to_string();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
            &mut fixture.store,
            crate::store::Store::open_in_memory().expect("replacement fixture Store"),
        )));
        let active = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let completed = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        SETTLEMENT_PROOF_PAUSE.lock().unwrap().insert(
            first_session,
            SettlementProofPause {
                reached: reached_tx,
                release: release_rx,
            },
        );
        let repository = fixture.repository.clone();
        let repository_identity = fixture.target.repository_identity.clone();
        let sandbox_base = fixture.sandbox_base.clone();
        let holder_root = fixture.quarantine_holder_proc.proc_root().to_path_buf();
        let holder_uid = fixture.quarantine_holder_proc.uid();
        let launch_in_mutex = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // #274: the settlement and the launch contend on one lock of this test's
        // own, standing in for the daemon-wide barrier they share in production.
        let cwd_admission = new_test_cwd_admission();
        std::thread::scope(|scope| {
            let settle = scope.spawn(|| {
                with_cwd_admission(cwd_admission, || {
                    crate::session::reaper::with_quarantine_holder_test_proc(
                        &holder_root,
                        holder_uid,
                        || {
                            apply_with_root_scheduling(
                                &repository,
                                ApplyStart::Replay(receipt),
                                &repository_identity,
                                &idempotency_key,
                                &plan_digest,
                                "",
                                "",
                                &sandbox_base,
                                &store,
                                &active,
                                &completed,
                                false,
                                None,
                                None,
                                None,
                            )
                        },
                    )
                })
            });
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("the first root is paused holding the exclusion and the mutex");

            // A launch: cwd admission first, then the repository mutex, which
            // it holds for longer than the old 1 s inverse-wait bound.
            let launch_repository = repository.clone();
            let flag = Arc::clone(&launch_in_mutex);
            let launch = runtime.spawn(async move {
                let _admission = cwd_admission.read().await;
                tokio::task::spawn_blocking(move || {
                    git_worktree::with_repository_mutation(&launch_repository, || {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        Ok(())
                    })
                })
                .await
                .expect("launch blocking task")
                .expect("launch allocation");
            });
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert!(!launch.is_finished(), "launch queues behind the first root");
            release_tx.send(()).expect("release the first root");

            let outcome = settle
                .join()
                .expect("settlement thread")
                .expect("the overlapping launch must not abort the cohort");
            runtime
                .block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(60), launch).await
                })
                .expect("launch completes")
                .expect("launch task");
            assert!(launch_in_mutex.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(outcome.receipt.counts.settled, item_count as u32);
            assert_eq!(outcome.receipt.counts.refused, 0, "{:?}", outcome.receipt);
            assert_eq!(outcome.receipt.counts.recovery_required, 0);
            assert!(outcome.receipt.finished_at.is_some());
        });
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
        let mut session = rsid_store::test_support::make_test_session();
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
            retained: Vec::new(),
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
        manager
            .recover_startup_source_worktree_settlements()
            .await
            .expect("uncertain orphan proof retains durable fences after readiness");
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
            .expect("next startup restores authority without running proofs");
        manager
            .recover_startup_source_worktree_settlements()
            .await
            .expect("post-readiness recovery reaps orphan then resumes retained intent");
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

    impl IntegratedFixture {
        fn audit_fetching_target(&self, fetch_target: bool) -> AuditSnapshot {
            let inventory = self.inventory();
            git_worktree::with_repository_mutation(&self.repository, || {
                build_audit_with_target_fetch_locked(
                    &self.target.repository_identity,
                    self.target
                        .canonical_repo_dir
                        .to_str()
                        .expect("UTF-8 canonical repository"),
                    &self.sandbox_base,
                    inventory,
                    &HashSet::new(),
                    &[],
                    fetch_target,
                )
            })
            .expect("audit integrated settlement fixture")
        }
    }

    /// #2040: local `refs/heads/<branch>` is never advanced on a long-lived hub;
    /// the fetched `origin` tip is the settlement ancestry authority.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn stale_local_target_settles_against_fetched_origin_tip() {
        let fixture = IntegratedFixture::new("stale-local-fetched-tip");
        let remote = fixture._directory.path().join("remote.git");
        std::fs::create_dir_all(&remote).expect("remote directory");
        git(&remote, &["init", "--bare", "-q"]);
        git(
            &fixture.repository,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("UTF-8 remote"),
            ],
        );
        // The source root's branch gains a commit that only the fetched origin
        // tip (a merge of it) contains.
        std::fs::write(fixture.allocation.root.join("source-only"), "source\n")
            .expect("source commit file");
        git(&fixture.allocation.root, &["add", "source-only"]);
        git(&fixture.allocation.root, &["commit", "-qm", "source work"]);
        git(
            &fixture.repository,
            &["merge", "--no-ff", "-q", "-m", "integrate", &fixture.branch],
        );
        git(&fixture.repository, &["push", "-q", "origin", "main"]);
        let merged = git(&fixture.repository, &["rev-parse", "refs/heads/main"]);
        git(&fixture.repository, &["reset", "-q", "--hard", "HEAD~1"]);
        // Drop the push-written tracking ref so only an explicit fetch can set it.
        git(
            &fixture.repository,
            &["update-ref", "-d", "refs/remotes/origin/main"],
        );
        let stale_local = git(&fixture.repository, &["rev-parse", "refs/heads/main"]);
        assert_ne!(stale_local, merged);

        // Apply-side recheck and a missing tracking ref fail closed, no fetch.
        let unfetched = fixture.audit_fetching_target(false);
        assert!(!unfetched.report.applyable);
        assert!(unfetched.report.target_oid.is_none());

        let audit = fixture.audit_fetching_target(true);
        assert!(audit.report.applyable, "{:?}", audit.report.refusal);
        assert_eq!(
            audit.report.target_oid.as_ref().map(|oid| oid.as_str()),
            Some(merged.as_str())
        );
        assert_eq!(audit.report.target_ref.as_deref(), Some("refs/heads/main"));
        assert_eq!(audit.eligible.len(), 1);
        assert_eq!(
            git(&fixture.repository, &["rev-parse", "refs/heads/main"]),
            stale_local,
            "the audit never moves the local branch"
        );

        // Apply's no-fetch recheck binds the same tip the plan digest named.
        let recheck = fixture.audit_fetching_target(false);
        assert!(recheck.report.applyable, "{:?}", recheck.report.refusal);
        assert_eq!(recheck.report.plan_digest, audit.report.plan_digest);

        // A newer origin tip changes the digest, so a stale plan cannot apply.
        git(&fixture.repository, &["merge", "-q", "--ff-only", &merged]);
        std::fs::write(fixture.repository.join("later"), "later\n").expect("later file");
        git(&fixture.repository, &["add", "later"]);
        git(&fixture.repository, &["commit", "-qm", "later"]);
        git(&fixture.repository, &["push", "-q", "origin", "main"]);
        git(
            &fixture.repository,
            &["reset", "-q", "--hard", &stale_local],
        );
        let newer = fixture.audit_fetching_target(true);
        assert!(newer.report.applyable, "{:?}", newer.report.refusal);
        assert_ne!(newer.report.plan_digest, audit.report.plan_digest);

        // A failed fetch refuses with a typed reason and never uses the local tip.
        git(
            &fixture.repository,
            &[
                "remote",
                "set-url",
                "origin",
                fixture
                    ._directory
                    .path()
                    .join("missing.git")
                    .to_str()
                    .expect("UTF-8 missing remote"),
            ],
        );
        let failed = fixture.audit_fetching_target(true);
        assert!(!failed.report.applyable);
        assert!(failed.report.target_oid.is_none());
        assert!(
            failed
                .report
                .refusal
                .as_deref()
                .is_some_and(|refusal| refusal.starts_with("target_fetch_failed:")),
            "{:?}",
            failed.report.refusal
        );
    }

    /// #2650: the audit plans a root eligible only when the quarantine fence's
    /// runtime check (`runtime_completed_matches`) would also accept it, so a
    /// stored session that moved without its runtime copy is not eligible-then-
    /// refused `custody_drift` on every pass.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn audit_leaves_out_a_root_whose_runtime_completed_entry_drifted_2650() {
        let fixture = IntegratedFixture::new("audit-runtime-parity");
        let stored = fixture
            .store
            .get_session(fixture.session_id)
            .unwrap()
            .expect("fixture session");
        let audit_with = |entry: Option<rsi_common::types::Session>| {
            let mut inventory = fixture.inventory();
            let mut completed = HashMap::new();
            if let Some(session) = entry {
                completed.insert(
                    session.id,
                    crate::session::types::CompletedSession::for_test(session),
                );
            }
            exclude_runtime_completed_drift(&mut inventory, &completed);
            git_worktree::with_repository_mutation(&fixture.repository, || {
                build_audit_locked(
                    &fixture.target.repository_identity,
                    fixture
                        .target
                        .canonical_repo_dir
                        .to_str()
                        .expect("UTF-8 canonical repository"),
                    &fixture.sandbox_base,
                    inventory,
                    &HashSet::new(),
                    &[],
                )
            })
            .expect("audit with runtime parity")
        };
        let eligible = |audit: &AuditSnapshot| {
            audit.report.applyable
                && audit.report.items[0].disposition
                    == SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        };

        // No runtime entry, and an entry equal to the stored session, are both
        // what the fence accepts: still eligible.
        assert!(eligible(&audit_with(None)));
        assert!(eligible(&audit_with(Some(stored.clone()))));

        // A stored `updated_at` the runtime copy never saw (pending-archive and
        // archive writes bump it in the Store only), or a status it still
        // holds as Completed, is the fence's `custody_drift`: not eligible.
        let mut stale_clock = stored.clone();
        stale_clock.updated_at = stored.updated_at - chrono::Duration::seconds(5);
        let drifted = audit_with(Some(stale_clock));
        assert!(!drifted.report.applyable);
        assert_eq!(drifted.report.counts.eligible, 0);
        assert_eq!(
            drifted.report.items[0].disposition,
            SourceWorktreeDispositionV1::CustodyUnverified
        );
        // #2731: the audit names the field that disagreed, stored vs observed.
        let detail = drifted.report.items[0]
            .diagnostic
            .as_deref()
            .expect("custody_unverified explains itself");
        assert!(
            detail.starts_with("check=runtime_completed_updated_at stored=")
                && detail.contains(" observed="),
            "{detail}"
        );
        let mut stale_status = stored.clone();
        stale_status.status = if stored.status == SessionStatus::Completed {
            SessionStatus::Failed
        } else {
            SessionStatus::Completed
        };
        let stale_status_audit = audit_with(Some(stale_status));
        assert!(!stale_status_audit.report.applyable);
        assert!(
            stale_status_audit.report.items[0]
                .diagnostic
                .as_deref()
                .is_some_and(|text| text.starts_with("check=runtime_completed_status stored="))
        );
        let mut moved_branch = stored;
        moved_branch.sandbox_branch = Some("rsi/elsewhere".into());
        let moved = audit_with(Some(moved_branch));
        assert!(!moved.report.applyable);
        assert!(
            moved.report.items[0]
                .diagnostic
                .as_deref()
                .is_some_and(
                    |text| text.starts_with("check=runtime_completed_sandbox_branch")
                        && text.ends_with("observed=rsi/elsewhere")
                ),
            "{:?}",
            moved.report.items[0].diagnostic
        );
    }

    /// #252 (hub #2731): the finalizer stamps the runtime completed entry
    /// before its Store writes stamp the row, so a legitimately terminal root's
    /// stored `updated_at` is a few ms later (and spelled `+00:00`, not `Z`).
    /// That is one terminal transition: eligible in the audit and accepted by
    /// the fence. A runtime entry later than the row, a stored write past the
    /// skew window, and status or branch drift still fail closed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn terminal_write_skew_is_not_runtime_completed_drift_252() {
        let fixture = IntegratedFixture::new("audit-terminal-write-skew");
        let stored = fixture
            .store
            .get_session(fixture.session_id)
            .unwrap()
            .expect("fixture session");
        // The observed pair from session 838b45fe.
        let stored_updated_at = "2026-10-16T00:38:52.973356835+00:00";
        let observed = chrono::DateTime::parse_from_rfc3339("2026-10-16T00:38:52.962142350Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let audit_with =
            |runtime_updated_at: chrono::DateTime<chrono::Utc>,
             mutate: &dyn Fn(&mut rsi_common::types::Session)| {
                let mut inventory = fixture.inventory();
                inventory[0].session_updated_at = Some(stored_updated_at.to_string());
                let mut session = stored.clone();
                session.updated_at = runtime_updated_at;
                mutate(&mut session);
                let mut completed = HashMap::new();
                completed.insert(
                    session.id,
                    crate::session::types::CompletedSession::for_test(session),
                );
                exclude_runtime_completed_drift(&mut inventory, &completed);
                let audit = git_worktree::with_repository_mutation(&fixture.repository, || {
                    build_audit_locked(
                        &fixture.target.repository_identity,
                        fixture
                            .target
                            .canonical_repo_dir
                            .to_str()
                            .expect("UTF-8 canonical repository"),
                        &fixture.sandbox_base,
                        inventory,
                        &HashSet::new(),
                        &[],
                    )
                })
                .expect("audit with runtime parity");
                (audit, completed)
            };
        let unchanged = |_: &mut rsi_common::types::Session| {};
        let assert_retained = |audit: &AuditSnapshot, check: &str| {
            assert!(!audit.report.applyable);
            assert_eq!(audit.report.counts.eligible, 0);
            assert_eq!(
                audit.report.items[0].disposition,
                SourceWorktreeDispositionV1::CustodyUnverified
            );
            let detail = audit.report.items[0].diagnostic.as_deref().unwrap_or("");
            assert!(detail.starts_with(check), "{detail}");
        };

        // The observed 11.2 ms finalizer skew: eligible, and the fence's
        // predicate accepts the same pair.
        let (skewed, completed) = audit_with(observed, &unchanged);
        assert!(
            skewed.report.applyable,
            "{:?}",
            skewed.report.items[0].diagnostic
        );
        assert_eq!(
            skewed.report.items[0].disposition,
            SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        );
        let entry = &completed[&fixture.session_id];
        assert!(completed_entry_matches_stored(
            entry,
            &format!("{:?}", entry.session.status),
            stored_updated_at,
            entry
                .session
                .sandbox_root
                .as_deref()
                .unwrap()
                .to_str()
                .unwrap(),
            entry.session.sandbox_branch.as_deref().unwrap(),
        ));
        // The exact instant, in either offset spelling, is still the same write.
        let stored_instant = chrono::DateTime::parse_from_rfc3339(stored_updated_at)
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(audit_with(stored_instant, &unchanged).0.report.applyable);

        // A runtime completion later than the stored row is a newer
        // transition the Store has not recorded: drift, even by 3 ms.
        let (later, _) = audit_with(
            stored_instant + chrono::Duration::milliseconds(3),
            &unchanged,
        );
        assert_retained(&later, "check=runtime_completed_updated_at stored=");
        // A stored write past the skew window is an unmirrored Store change.
        let (stale, _) = audit_with(
            stored_instant
                - RUNTIME_COMPLETED_TERMINAL_WRITE_SKEW
                - chrono::Duration::milliseconds(1),
            &unchanged,
        );
        assert_retained(&stale, "check=runtime_completed_updated_at stored=");
        // Inside the window, status and branch drift are still exact.
        let (status_drift, _) = audit_with(observed, &|session| {
            session.status = if session.status == SessionStatus::Completed {
                SessionStatus::Failed
            } else {
                SessionStatus::Completed
            };
        });
        assert_retained(&status_drift, "check=runtime_completed_status stored=");
        let (reopened, _) = audit_with(observed, &|session| {
            session.status = SessionStatus::Running;
        });
        assert_retained(&reopened, "check=runtime_completed_status stored=");
        let (moved_branch, _) = audit_with(observed, &|session| {
            session.sandbox_branch = Some("rsi/elsewhere".into());
        });
        assert_retained(&moved_branch, "check=runtime_completed_sandbox_branch");
        // An unparseable stored timestamp never matches.
        assert!(!completed_entry_matches_stored(
            entry,
            &format!("{:?}", entry.session.status),
            "not-a-timestamp",
            entry
                .session
                .sandbox_root
                .as_deref()
                .unwrap()
                .to_str()
                .unwrap(),
            entry.session.sandbox_branch.as_deref().unwrap(),
        ));
    }

    /// #257: an async title/description refinement (or any mirrored metadata
    /// write) that lands well past the #252 skew window advances the runtime
    /// completed entry to exactly the stamp it stored, so the root stays
    /// eligible without widening the window. The mirror is compare-and-advance:
    /// an unmirrored Store change before the write, a newer runtime completion,
    /// an unparseable prior stamp or a failed write leave the entry behind and
    /// the root retained, and status or branch drift after the mirror still
    /// retains.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn late_metadata_write_mirrors_runtime_completed_stamp_257() {
        use crate::store::SessionWriteStamp;
        let fixture = IntegratedFixture::new("audit-late-metadata-write");
        let stored = fixture
            .store
            .get_session(fixture.session_id)
            .unwrap()
            .expect("fixture session");
        let at = |text: &str| {
            chrono::DateTime::parse_from_rfc3339(text)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        // Finalization: runtime copy, then the finalizer's own Store write.
        let finalized = at("2026-10-16T00:38:52.962142350Z");
        let finalizer_write = at("2026-10-16T00:38:52.973356835+00:00");
        // The refinement's model call returns 41 s later: the first stamped
        // write since finalization, so it starts a run from the finalizer's
        // own Store write (#265).
        let refinement = SessionWriteStamp {
            previous_updated_at: Some(finalizer_write),
            stamped_at: at("2026-10-16T00:39:34.101000000+00:00"),
            write_seq: 1,
            run_seq: 1,
            run_base: Some(finalizer_write),
        };
        let refined_row = refinement.stamped_at.to_rfc3339();
        let entry_at = |runtime_updated_at: chrono::DateTime<chrono::Utc>| {
            let mut session = stored.clone();
            session.updated_at = runtime_updated_at;
            crate::session::types::CompletedSession::for_test(session)
        };
        // The audit sees the runtime entry's session as it stands.
        let audit = |entry: &crate::session::types::CompletedSession, stored_updated_at: &str| {
            let mut inventory = fixture.inventory();
            inventory[0].session_updated_at = Some(stored_updated_at.to_string());
            let mut completed = HashMap::new();
            completed.insert(
                fixture.session_id,
                crate::session::types::CompletedSession::for_test(entry.session.clone()),
            );
            exclude_runtime_completed_drift(&mut inventory, &completed);
            git_worktree::with_repository_mutation(&fixture.repository, || {
                build_audit_locked(
                    &fixture.target.repository_identity,
                    fixture
                        .target
                        .canonical_repo_dir
                        .to_str()
                        .expect("UTF-8 canonical repository"),
                    &fixture.sandbox_base,
                    inventory,
                    &HashSet::new(),
                    &[],
                )
            })
            .expect("audit with runtime parity")
        };
        let assert_eligible = |audit: &AuditSnapshot| {
            assert!(
                audit.report.applyable,
                "{:?}",
                audit.report.items[0].diagnostic
            );
            assert_eq!(
                audit.report.items[0].disposition,
                SourceWorktreeDispositionV1::EligibleIntegratedAncestor
            );
        };
        let assert_retained = |audit: &AuditSnapshot, check: &str| {
            assert!(!audit.report.applyable);
            assert_eq!(
                audit.report.items[0].disposition,
                SourceWorktreeDispositionV1::CustodyUnverified
            );
            let detail = audit.report.items[0].diagnostic.as_deref().unwrap_or("");
            assert!(detail.starts_with(check), "{detail}");
        };
        let updated_at_check = "check=runtime_completed_updated_at stored=";

        // Without the mirror the late refinement retains the root (the bug).
        let unmirrored = entry_at(finalized);
        assert_retained(&audit(&unmirrored, &refined_row), updated_at_check);

        // Mirrored: exactly the stored stamp, eligible, and the fence's
        // predicate accepts the same pair.
        let mut mirrored = entry_at(finalized);
        assert!(mirror_completed_store_write(
            &mut mirrored,
            Some(refinement)
        ));
        assert_eq!(mirrored.session.updated_at, refinement.stamped_at);
        assert_eligible(&audit(&mirrored, &refined_row));
        assert!(completed_entry_matches_stored(
            &mirrored,
            &format!("{:?}", mirrored.session.status),
            &refined_row,
            mirrored
                .session
                .sandbox_root
                .as_deref()
                .unwrap()
                .to_str()
                .unwrap(),
            mirrored.session.sandbox_branch.as_deref().unwrap(),
        ));
        // A second write chains from the first: the same run.
        let description = SessionWriteStamp {
            previous_updated_at: Some(refinement.stamped_at),
            stamped_at: refinement.stamped_at + chrono::Duration::seconds(30),
            write_seq: 2,
            ..refinement
        };
        assert!(mirror_completed_store_write(
            &mut mirrored,
            Some(description)
        ));
        assert_eligible(&audit(&mirrored, &description.stamped_at.to_rfc3339()));

        // An unmirrored Store change (5 s after finalization) before the
        // refinement: the write starts its run from that change, so it does
        // not hide it.
        let mut behind = entry_at(finalized);
        let after_unseen = SessionWriteStamp {
            previous_updated_at: Some(finalized + chrono::Duration::seconds(5)),
            run_base: Some(finalized + chrono::Duration::seconds(5)),
            ..refinement
        };
        assert!(!mirror_completed_store_write(
            &mut behind,
            Some(after_unseen)
        ));
        assert_eq!(behind.session.updated_at, finalized);
        assert_retained(&audit(&behind, &refined_row), updated_at_check);
        // A newer runtime completion than the row the write replaced.
        let mut newer = entry_at(finalizer_write + chrono::Duration::milliseconds(3));
        assert!(!mirror_completed_store_write(&mut newer, Some(refinement)));
        assert_retained(&audit(&newer, &refined_row), updated_at_check);
        // An unparseable prior stamp, and a failed or no-op write.
        let mut unparsed = entry_at(finalized);
        let unparsed_prior = SessionWriteStamp {
            previous_updated_at: None,
            run_base: None,
            ..refinement
        };
        assert!(!mirror_completed_store_write(
            &mut unparsed,
            Some(unparsed_prior)
        ));
        assert!(!mirror_completed_store_write(&mut unparsed, None));
        assert_retained(&audit(&unparsed, &refined_row), updated_at_check);

        // After a good mirror, status, reopen and branch drift still retain.
        let mirrored_at = mirrored.session.updated_at;
        let mut status_drift = entry_at(mirrored_at);
        status_drift.session.status = if status_drift.session.status == SessionStatus::Completed {
            SessionStatus::Failed
        } else {
            SessionStatus::Completed
        };
        let current_row = description.stamped_at.to_rfc3339();
        assert_retained(
            &audit(&status_drift, &current_row),
            "check=runtime_completed_status stored=",
        );
        let mut reopened = entry_at(mirrored_at);
        reopened.session.status = SessionStatus::Running;
        assert_retained(
            &audit(&reopened, &current_row),
            "check=runtime_completed_status stored=",
        );
        let mut moved_branch = entry_at(mirrored_at);
        moved_branch.session.sandbox_branch = Some("rsi/elsewhere".into());
        assert_retained(
            &audit(&moved_branch, &current_row),
            "check=runtime_completed_sandbox_branch",
        );
        let mut moved_root = entry_at(mirrored_at);
        moved_root.session.sandbox_root = Some(std::path::PathBuf::from("/elsewhere/root"));
        assert_retained(
            &audit(&moved_root, &current_row),
            "check=runtime_completed_sandbox_root",
        );
    }

    /// #260, adapted for #265: Store metadata writes mirrored in any order
    /// converge the entry to the row, and the root is eligible. The mirrors
    /// are ordered by the Store (`write_seq` and the write's run of stamped
    /// writes), so nothing is held: the newest write of the run moves the
    /// entry at once and an older one that arrives later is a no-op. A run
    /// that does not start from the entry (an unmirrored, unstamped Store
    /// change between) never applies, and the root stays retained.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn reordered_metadata_mirrors_converge_and_unlinked_jumps_retain_260() {
        use crate::store::SessionWriteStamp;
        let fixture = IntegratedFixture::new("audit-reordered-metadata-mirrors");
        let stored = fixture
            .store
            .get_session(fixture.session_id)
            .unwrap()
            .expect("fixture session");
        let at = |text: &str| {
            chrono::DateTime::parse_from_rfc3339(text)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let finalized = at("2026-10-16T00:38:52.962142350Z");
        let finalizer_write = at("2026-10-16T00:38:52.973356835+00:00");
        // Store commit order: refinement (41 s on), rename, then a parent
        // move; one run from the finalizer's own Store write.
        let refinement = SessionWriteStamp {
            previous_updated_at: Some(finalizer_write),
            stamped_at: at("2026-10-16T00:39:34.101000000+00:00"),
            write_seq: 1,
            run_seq: 1,
            run_base: Some(finalizer_write),
        };
        let rename = SessionWriteStamp {
            previous_updated_at: Some(refinement.stamped_at),
            stamped_at: refinement.stamped_at + chrono::Duration::seconds(30),
            write_seq: 2,
            ..refinement
        };
        let parent_move = SessionWriteStamp {
            previous_updated_at: Some(rename.stamped_at),
            stamped_at: rename.stamped_at + chrono::Duration::seconds(30),
            write_seq: 3,
            ..refinement
        };
        let row = parent_move.stamped_at.to_rfc3339();
        let entry_at = |runtime_updated_at: chrono::DateTime<chrono::Utc>| {
            let mut session = stored.clone();
            session.updated_at = runtime_updated_at;
            crate::session::types::CompletedSession::for_test(session)
        };
        let audit = |entry: &crate::session::types::CompletedSession, stored_updated_at: &str| {
            let mut inventory = fixture.inventory();
            inventory[0].session_updated_at = Some(stored_updated_at.to_string());
            let mut completed = HashMap::new();
            completed.insert(
                fixture.session_id,
                crate::session::types::CompletedSession::for_test(entry.session.clone()),
            );
            exclude_runtime_completed_drift(&mut inventory, &completed);
            git_worktree::with_repository_mutation(&fixture.repository, || {
                build_audit_locked(
                    &fixture.target.repository_identity,
                    fixture
                        .target
                        .canonical_repo_dir
                        .to_str()
                        .expect("UTF-8 canonical repository"),
                    &fixture.sandbox_base,
                    inventory,
                    &HashSet::new(),
                    &[],
                )
            })
            .expect("audit with runtime parity")
        };
        let assert_retained = |entry: &crate::session::types::CompletedSession, row: &str| {
            let audit = audit(entry, row);
            assert!(!audit.report.applyable);
            assert_eq!(
                audit.report.items[0].disposition,
                SourceWorktreeDispositionV1::CustodyUnverified
            );
            let detail = audit.report.items[0].diagnostic.as_deref().unwrap_or("");
            assert!(
                detail.starts_with("check=runtime_completed_updated_at stored="),
                "{detail}"
            );
        };
        let assert_eligible = |entry: &crate::session::types::CompletedSession| {
            let audit = audit(entry, &row);
            assert!(
                audit.report.applyable,
                "{:?}",
                audit.report.items[0].diagnostic
            );
            assert_eq!(
                audit.report.items[0].disposition,
                SourceWorktreeDispositionV1::EligibleIntegratedAncestor
            );
        };

        // The reported interleaving: the parent move and the rename reach the
        // mirror before the refinement. The newest write converges the entry;
        // the older ones arriving later change nothing.
        let mut reordered = entry_at(finalized);
        assert!(mirror_completed_store_write(
            &mut reordered,
            Some(parent_move)
        ));
        assert_eq!(reordered.session.updated_at, parent_move.stamped_at);
        assert!(!mirror_completed_store_write(&mut reordered, Some(rename)));
        assert!(!mirror_completed_store_write(
            &mut reordered,
            Some(refinement)
        ));
        assert_eq!(reordered.session.updated_at, parent_move.stamped_at);
        assert_eligible(&reordered);
        // In-order arrival reaches the same entry.
        let mut in_order = entry_at(finalized);
        for stamp in [refinement, rename, parent_move] {
            assert!(mirror_completed_store_write(&mut in_order, Some(stamp)));
        }
        assert_eq!(in_order.session.updated_at, reordered.session.updated_at);
        assert_eq!(in_order.store_writes, reordered.store_writes);

        // An unmirrored Store change 5 s after the refinement starts a new
        // run: the rename and the parent move after it do not chain from the
        // entry in any order, the refinement does, and the root stays
        // retained behind the change.
        let unseen = refinement.stamped_at + chrono::Duration::seconds(5);
        let rename_after_unseen = SessionWriteStamp {
            previous_updated_at: Some(unseen),
            run_seq: 2,
            run_base: Some(unseen),
            ..rename
        };
        let move_after_unseen = SessionWriteStamp {
            previous_updated_at: Some(rename_after_unseen.stamped_at),
            stamped_at: parent_move.stamped_at,
            write_seq: 3,
            ..rename_after_unseen
        };
        let mut gapped = entry_at(finalized);
        assert!(!mirror_completed_store_write(
            &mut gapped,
            Some(move_after_unseen)
        ));
        assert!(!mirror_completed_store_write(
            &mut gapped,
            Some(rename_after_unseen)
        ));
        assert!(mirror_completed_store_write(&mut gapped, Some(refinement)));
        assert!(!mirror_completed_store_write(
            &mut gapped,
            Some(move_after_unseen)
        ));
        assert_eq!(gapped.session.updated_at, refinement.stamped_at);
        assert_retained(&gapped, &row);

        // A newer runtime completion than the run's start: nothing applies.
        let mut newer = entry_at(finalizer_write + chrono::Duration::milliseconds(3));
        assert!(!mirror_completed_store_write(&mut newer, Some(parent_move)));
        assert!(!mirror_completed_store_write(&mut newer, Some(refinement)));
        assert_eq!(
            newer.session.updated_at,
            finalizer_write + chrono::Duration::milliseconds(3)
        );
        assert_retained(&newer, &row);
    }

    /// #2731: every stored custody/session identity check that retains a root
    /// as `custody_unverified` (or generation drift) says which check failed and
    /// the stored vs observed values, so one audit run explains the retention.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn custody_unverified_audit_item_names_the_failed_check_2731() {
        let fixture = IntegratedFixture::new("audit-custody-detail");
        let audit_mutated = |mutate: &dyn Fn(&mut SourceWorktreeInventoryRow)| {
            let mut inventory = fixture.inventory();
            mutate(&mut inventory[0]);
            git_worktree::with_repository_mutation(&fixture.repository, || {
                build_audit_locked(
                    &fixture.target.repository_identity,
                    fixture
                        .target
                        .canonical_repo_dir
                        .to_str()
                        .expect("UTF-8 canonical repository"),
                    &fixture.sandbox_base,
                    inventory,
                    &HashSet::new(),
                    &[],
                )
            })
            .expect("audit with mutated row")
            .report
            .items
            .remove(0)
        };
        let baseline = audit_mutated(&|_| {});
        assert_eq!(
            baseline.disposition,
            SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        );
        assert_eq!(baseline.diagnostic, None);

        let cases: Vec<(
            Box<dyn Fn(&mut SourceWorktreeInventoryRow)>,
            SourceWorktreeDispositionV1,
            &str,
        )> = vec![
            (
                Box::new(|row| row.validation_state = "pending".into()),
                SourceWorktreeDispositionV1::CustodyUnverified,
                "check=validation_state stored=pending expected=verified",
            ),
            (
                Box::new(|row| row.owner_session_id = Some(Uuid::new_v4())),
                SourceWorktreeDispositionV1::CustodyUnverified,
                "check=owner_session stored_owner=",
            ),
            (
                Box::new(|row| row.session_working_dir = Some("/elsewhere".into())),
                SourceWorktreeDispositionV1::CustodyUnverified,
                "check=session_working_dir stored_session=/elsewhere",
            ),
            (
                Box::new(|row| row.session_sandbox_branch = None),
                SourceWorktreeDispositionV1::CustodyUnverified,
                "check=session_sandbox_branch stored_session=none",
            ),
            (
                Box::new(|row| row.session_cleanup_state = Some("Failed".into())),
                SourceWorktreeDispositionV1::CustodyUnverified,
                "check=session_cleanup_state stored=Failed expected=Live",
            ),
            (
                Box::new(|row| row.validated_generation = row.generation + 1),
                SourceWorktreeDispositionV1::CustodyGenerationDrift,
                "check=validated_generation stored_generation=",
            ),
        ];
        for (mutate, disposition, expected) in cases {
            let item = audit_mutated(&*mutate);
            assert_eq!(item.disposition, disposition, "{expected}");
            let detail = item.diagnostic.as_deref().unwrap_or_default();
            assert!(detail.starts_with(expected), "{expected} vs {detail}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn describe_settlement_target_flags_a_stale_remote_tracking_ref_2731() {
        let freshness =
            |compared_ref: &str, updated: Option<u64>| git_worktree::SettlementTargetFreshness {
                compared_ref: compared_ref.into(),
                ref_updated_secs_ago: updated,
                tip_committed_secs_ago: Some(7),
            };
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let old = STALE_TARGET_REF_SECS + 1;
        let stale = describe_settlement_target(
            &freshness("refs/remotes/origin/rolling", Some(old)),
            oid,
            false,
        );
        assert_eq!(
            stale,
            format!(
                "compared_ref=refs/remotes/origin/rolling@0123456789ab ref_updated_secs_ago={old} tip_committed_secs_ago=7 fetched_this_pass=false stale_ref"
            )
        );
        // Fetched by this pass, recently moved, unknown age, or a local branch
        // (no remote authority) are never reported stale.
        for (reference, updated, fetched) in [
            ("refs/remotes/origin/rolling", Some(old), true),
            ("refs/remotes/origin/rolling", Some(5), false),
            ("refs/remotes/origin/rolling", None, false),
            ("refs/heads/rolling", Some(old), false),
        ] {
            let text = describe_settlement_target(&freshness(reference, updated), oid, fetched);
            assert!(!text.contains("stale_ref"), "{text}");
        }
    }

    impl IntegratedFixture {
        /// #247: bind the fixture Session as the live worker of an Issue that
        /// is `InProgress` with no handoff, the way a manager launch journals
        /// it. Returns the Issue and its project.
        fn bind_to_in_progress_issue(&self) -> (Uuid, Uuid) {
            let now = chrono::Utc::now();
            let project = rsi_common::types::Project {
                id: Uuid::new_v4(),
                name: "paused worker".into(),
                path: None,
                description: None,
                color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            };
            self.store.insert_project(&project).expect("project");
            let issue = self
                .store
                .create_issue(&rsi_common::types::NewIssue {
                    project_id: project.id,
                    title: "paused worker Issue".into(),
                    body: String::new(),
                    priority: None,
                    labels: Vec::new(),
                    created_by_session_id: None,
                    assignee: None,
                    idea_id: None,
                    source_event_id: None,
                    source_finding_ref: None,
                })
                .expect("Issue");
            self.store
                .conn
                .execute(
                    "UPDATE issues SET status='InProgress' WHERE id=?1",
                    [issue.id.to_string()],
                )
                .expect("start Issue");
            let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            let payload = serde_json::json!({
                "request": {"operation": {"action": "create_session"}},
                "issue_binding": {"issue_id": issue.id.to_string()},
            });
            self.store
                .conn
                .execute(
                    "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
                        scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
                        state,target_session_id,outcome_json,not_before,created_at,updated_at)
                     VALUES(?1,?2,?3,1,1,?1,'fixture','lifecycle_action',?4,'succeeded',?3,'{}',?5,?5,?5)",
                    rusqlite::params![
                        Uuid::new_v4().to_string(),
                        project.id.to_string(),
                        self.session_id.to_string(),
                        payload.to_string(),
                        stamp,
                    ],
                )
                .expect("journal the Issue-bound launch");
            (project.id, issue.id)
        }

        /// #2683: place the fixture Session under an Epic that only the project
        /// policy pauses: no per-worker operator marker and no Issue binding.
        fn place_under_policy_paused_epic(&self) {
            use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
            use rsi_common::harness_manager_v2::{
                ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerPolicyV2,
            };
            use rsi_common::types::SessionKind;
            let now = chrono::Utc::now();
            let project = rsi_common::types::Project {
                id: Uuid::new_v4(),
                name: "paused epic".into(),
                path: None,
                description: None,
                color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            };
            self.store.insert_project(&project).expect("project");
            let mut manager = rsid_store::test_support::make_test_session();
            manager.project_id = Some(project.id);
            manager.session_kind = SessionKind::Standard;
            manager.status = SessionStatus::Completed;
            self.store.insert_session(&manager).expect("manager");
            let mut group = manager.clone();
            group.id = Uuid::new_v4();
            group.session_kind = SessionKind::Group;
            self.store.insert_session(&group).expect("group");
            let mut epic = manager.clone();
            epic.id = Uuid::new_v4();
            epic.session_kind = SessionKind::Epic;
            epic.status = SessionStatus::Running;
            epic.parent_id = Some(group.id);
            self.store.insert_session(&epic).expect("epic");
            self.store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    group_ids: Vec::new(),
                    project_id: project.id,
                    session_id: manager.id,
                    epic_ids: Some(vec![epic.id]),
                    expected_row_version: 0,
                })
                .expect("appoint the manager");
            self.store
                .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: project.id,
                    expected_scope_version: 1,
                    expected_policy_version: 0,
                    idempotency_key: "epic-pause".into(),
                    policy: ManagerPolicyV2 {
                        capabilities: vec![ManagerCapabilityV2::WorkPlan],
                        paused_epic_ids: vec![epic.id],
                        ..Default::default()
                    },
                })
                .expect("pause the Epic by policy");
            self.store
                .conn
                .execute(
                    "UPDATE sessions SET project_id=?2, parent_id=?3 WHERE id=?1",
                    rusqlite::params![
                        self.session_id.to_string(),
                        project.id.to_string(),
                        epic.id.to_string()
                    ],
                )
                .expect("place the Session under the paused Epic");
        }

        /// The worker reports on its Issue: an append-only handoff record.
        fn publish_issue_handoff(&self, project: Uuid, issue: Uuid) {
            self.store
                .conn
                .execute(
                    "INSERT INTO issue_handoffs
                         (issue_id, sequence, project_id, author_session_id, idempotency_key,
                          body, created_at)
                     VALUES (?1, 1, ?2, ?3, 'fixture-handoff', 'finished', ?4)",
                    rusqlite::params![
                        issue.to_string(),
                        project.to_string(),
                        self.session_id.to_string(),
                        "2026-10-14T12:00:00.000000000Z",
                    ],
                )
                .expect("publish the worker handoff");
        }

        /// A second handoff, stamped after any fixture-written event.
        fn publish_newer_issue_handoff(&self, project: Uuid, issue: Uuid) {
            self.store
                .conn
                .execute(
                    "INSERT INTO issue_handoffs
                         (issue_id, sequence, project_id, author_session_id, idempotency_key,
                          body, created_at)
                     VALUES (?1, 2, ?2, ?3, 'fixture-handoff-2', 'finished', ?4)",
                    rusqlite::params![
                        issue.to_string(),
                        project.to_string(),
                        self.session_id.to_string(),
                        "2099-01-01T00:00:00.000000000Z",
                    ],
                )
                .expect("publish the current worker handoff");
        }

        /// Settle the first journal item of `run` the way apply does, with the
        /// repository lock held; returns whether the root was settled.
        fn settle_first_item(&mut self, run: &NewSettlementRun) -> bool {
            let journal = self
                .store
                .list_source_worktree_settlement_journal_items(run.run_id)
                .expect("settlement journal");
            let store = Arc::new(tokio::sync::Mutex::new(std::mem::replace(
                &mut self.store,
                crate::store::Store::open_in_memory().expect("placeholder Store"),
            )));
            let settled = self.with_quarantine_holder_test_proc(|| {
                git_worktree::with_repository_mutation(&self.repository, || {
                    settle_one_item_locked(
                        &store,
                        &Arc::new(tokio::sync::RwLock::new(HashMap::new())),
                        &Arc::new(tokio::sync::RwLock::new(HashMap::new())),
                        run.run_id,
                        &run.target_ref,
                        &self.repository,
                        &self.sandbox_base,
                        &journal[0],
                        false,
                    )
                })
                .expect("settle the fixture item")
            });
            self.store = Arc::try_unwrap(store)
                .ok()
                .expect("sole Store owner")
                .into_inner();
            settled
        }
    }

    /// #247: a manager-halted worker bound to an unfinished Issue is paused
    /// work. The audit never plans its sandbox, whatever Issue handoffs it
    /// published; a worker whose latest turn carries its final report is
    /// eligible again.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_paused_issue_worker_is_retained_by_the_audit_until_it_reports() {
        let fixture = IntegratedFixture::new("paused-issue-worker-audit");
        assert_eq!(
            fixture.audit().eligible.len(),
            1,
            "an unbound worker settles"
        );

        let (project, issue) = fixture.bind_to_in_progress_issue();
        fixture
            .store
            .update_session_status(fixture.session_id, SessionStatus::Interrupted)
            .expect("manager halt");
        let paused = fixture.audit();
        assert!(paused.eligible.is_empty());
        assert!(!paused.report.applyable);
        assert_eq!(
            paused.report.items[0].disposition,
            SourceWorktreeDispositionV1::NonterminalStatus
        );
        paused
            .report
            .validate_wire()
            .expect("a retained paused worker is wire-valid");

        // An Issue handoff (even an old pre-verification one, then a manager
        // Continue) is progress, not completion: the audit still retains it.
        fixture.publish_issue_handoff(project, issue);
        let message_event =
            |sequence: i32, role, content: &str| rsi_common::types::ConversationEvent {
                id: 0,
                session_id: fixture.session_id,
                sequence,
                event_type: rsi_common::types::EventType::Message,
                role: Some(role),
                content: content.into(),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            };
        fixture
            .store
            .insert_event(&message_event(
                1,
                rsi_common::types::Role::User,
                "Continue: correct the review finding",
            ))
            .expect("manager Continue");
        fixture.publish_newer_issue_handoff(project, issue);
        let handed_off = fixture.audit();
        assert!(handed_off.eligible.is_empty());
        assert_eq!(
            handed_off.report.items[0].disposition,
            SourceWorktreeDispositionV1::NonterminalStatus
        );

        // The worker's final report in its current turn finishes it.
        fixture
            .store
            .insert_event(&message_event(
                2,
                rsi_common::types::Role::Assistant,
                "PIPELINE HANDOFF — IMPLEMENTATION:\nRESULT: corrected",
            ))
            .expect("final report");
        let finished = fixture.audit();
        assert_eq!(finished.eligible.len(), 1);
        assert_eq!(
            finished.report.items[0].disposition,
            SourceWorktreeDispositionV1::EligibleIntegratedAncestor
        );
    }

    /// #247: the last fence before the first effect. A Session that became an
    /// unfinished Issue worker after its intent was committed keeps its
    /// untouched root, Session row and branch; the same fixture without the
    /// binding settles.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_worker_bound_after_intent_keeps_its_untouched_sandbox_and_a_finished_one_settles() {
        let mut held = IntegratedFixture::new("paused-issue-worker-fence");
        let run = held.commit_intent("fixture:paused-issue-worker-fence");
        held.bind_to_in_progress_issue();
        assert!(
            !held.settle_first_item(&run),
            "the held root is not settled"
        );
        assert_eq!(
            held.journal_item(run.run_id).phase,
            SourceWorktreeSettlementPhaseV1::Unattempted
        );
        assert!(held.allocation.root.exists(), "the sandbox root stays");
        assert_eq!(
            git(
                &held.repository,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("refs/heads/{}", held.branch)
                ],
            ),
            held.source_oid
        );
        let session = held
            .store
            .get_session(held.session_id)
            .expect("read Session")
            .expect("Session row");
        assert_eq!(session.status, SessionStatus::Completed);
        assert_eq!(
            session.sandbox_cleanup_state,
            Some(SandboxCleanupState::Live)
        );

        let mut finished = IntegratedFixture::new("finished-worker-fence");
        let run = finished.commit_intent("fixture:finished-worker-fence");
        assert!(
            finished.settle_first_item(&run),
            "a finished worker settles"
        );
        assert!(!finished.allocation.root.exists());
        let session = finished
            .store
            .get_session(finished.session_id)
            .expect("read Session")
            .expect("Session row");
        assert_eq!(session.status, SessionStatus::Archived);
        assert_eq!(
            session.sandbox_cleanup_state,
            Some(SandboxCleanupState::Purged)
        );
    }

    /// #2683: an Epic paused only by policy (no per-worker marker, no Issue
    /// binding) retains its finished-looking worker in the audit; the same
    /// fixture outside a paused Epic is eligible.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn a_worker_under_a_policy_paused_epic_is_retained_by_the_audit() {
        let fixture = IntegratedFixture::new("paused-epic-audit");
        assert_eq!(fixture.audit().eligible.len(), 1, "unpaused settles");

        fixture.place_under_policy_paused_epic();
        let paused = fixture.audit();
        assert!(paused.eligible.is_empty());
        assert!(!paused.report.applyable);
        assert_eq!(
            paused.report.items[0].disposition,
            SourceWorktreeDispositionV1::NonterminalStatus
        );
        paused
            .report
            .validate_wire()
            .expect("a retained paused worker is wire-valid");
    }

    /// #2683: a pause that arrives after the intent was committed is caught by
    /// the last fence before the first effect: the root, branch and Session
    /// stay untouched.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn an_epic_paused_after_intent_keeps_the_untouched_sandbox() {
        let mut held = IntegratedFixture::new("paused-epic-fence");
        let run = held.commit_intent("fixture:paused-epic-fence");
        held.place_under_policy_paused_epic();
        assert!(
            !held.settle_first_item(&run),
            "the paused Epic's root is not settled"
        );
        assert_eq!(
            held.journal_item(run.run_id).phase,
            SourceWorktreeSettlementPhaseV1::Unattempted
        );
        assert!(held.allocation.root.exists(), "the sandbox root stays");
        assert_eq!(
            git(
                &held.repository,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("refs/heads/{}", held.branch)
                ],
            ),
            held.source_oid
        );
        let session = held
            .store
            .get_session(held.session_id)
            .expect("read Session")
            .expect("Session row");
        assert_eq!(session.status, SessionStatus::Completed);
        assert_eq!(
            session.sandbox_cleanup_state,
            Some(SandboxCleanupState::Live)
        );
    }
}
