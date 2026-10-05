//! Sealed sandbox-custody vocabulary and the pre-effect classifier.
//!
//! This module deliberately does not perform provider, context, tool, or
//! filesystem effects. Later H1 continuations wire those seams through the
//! handles defined here; until then this is a compile-clean domain boundary.

use crate::error::{DaemonError, Result, sandbox_custody_error};
use crate::sandbox::git_worktree;
use crate::sandbox::target_reclaim::{
    PinnedSandboxRoot, RegisteredTargetIntent, TargetReclaimKind, TargetReclaimOutcome,
};
use crate::store::Store;
use crate::store::custody_lock_order::{
    admission_wait, is_lock_order_busy_error, lock_store_then_optional_root, lock_store_then_root,
    lock_store_then_root_within, lock_store_then_session_root,
};
use crate::store::sandbox_custody::{
    ArchivedRotationRestoration, CustodyCause, CustodyRootGuard, EffectReservation,
    LegacyStartupRoot, LegacyTerminalRoot, RetryAuthorityCapture, RetryAuthorityFence,
    RotationAuthorityCapture, RotationAuthorityFence, SessionCustodyBinding, StartupCustodyGroup,
    lock_custody_root,
};
use crate::store::target_reclaim_sweep::{
    PrepareTargetReclaimIntentResult, TargetReclaimIntentTerminalState,
};
use rsi_common::sandbox_storage::SandboxBuildCacheReclaimSkipReason as ReclaimSkipReason;
use rsi_common::types::{
    SandboxCleanupState, SandboxCustodyErrorCodeV1, SandboxCustodyErrorV1,
    SandboxCustodyRecoveryV1, SandboxCustodyTransitionV1, SandboxKind, SandboxSpec, Session,
    SessionStatus,
};
use rusqlite::OptionalExtension;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "test-seam"))]
use std::sync::atomic::AtomicU8;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use uuid::Uuid;

/// Shared only with the custody boundary. An effect retains this handle, never
/// a mutable Store borrow, so provider/task lifetimes cannot pin SQLite.
pub(crate) type CustodyStore = Arc<Mutex<Store>>;

/// Reclaim queues each short SQL phase on Tokio's FIFO Store mutex. Filesystem
/// and Git work runs between phases without either Store or a root stripe.
struct PhasedReclaimStore {
    store: CustodyStore,
    pass: Arc<crate::sandbox::target_reclaim::ReclaimPassState>,
}

trait ReclaimStoreAccess {
    fn with_store<T>(&mut self, action: impl FnOnce(&Store) -> T) -> Option<T>;

    fn root_already_locked(&self) -> bool {
        false
    }
}

impl ReclaimStoreAccess for &Store {
    fn with_store<T>(&mut self, action: impl FnOnce(&Store) -> T) -> Option<T> {
        Some(action(self))
    }

    fn root_already_locked(&self) -> bool {
        true
    }
}

impl ReclaimStoreAccess for PhasedReclaimStore {
    fn with_store<T>(&mut self, action: impl FnOnce(&Store) -> T) -> Option<T> {
        let wait = self.pass.remaining_duration();
        if wait.is_zero() {
            return None;
        }
        let store = tokio::runtime::Handle::current().block_on(async {
            tokio::time::timeout(wait, Arc::clone(&self.store).lock_owned())
                .await
                .ok()
        })?;
        Some(action(&store))
    }
}

#[cfg(any(test, feature = "test-seam"))]
type ReclaimBeforeDeleteHook = (PathBuf, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>);

#[cfg(any(test, feature = "test-seam"))]
fn reclaim_before_delete_hook() -> &'static std::sync::Mutex<Option<ReclaimBeforeDeleteHook>> {
    static HOOK: std::sync::OnceLock<std::sync::Mutex<Option<ReclaimBeforeDeleteHook>>> =
        std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(any(test, feature = "test-seam"))]
pub fn set_reclaim_before_delete_hook(hook: Option<ReclaimBeforeDeleteHook>) {
    *reclaim_before_delete_hook()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

#[cfg(any(test, feature = "test-seam"))]
fn pause_reclaim_before_delete_for_test(base: &Path) {
    let hook = reclaim_before_delete_hook()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some((hook_base, reached, release)) = hook
        && hook_base == base
    {
        reached.wait();
        release.wait();
    }
}

#[cfg(not(any(test, feature = "test-seam")))]
const fn pause_reclaim_before_delete_for_test(_base: &Path) {}

/// Cloneable, opaque execution runtime carried only through the manager-owned
/// monitor/rotation recursion. It retains the existing Store, settlement
/// producer, allocator base, and daemon boot identity without reconstructing
/// any of them from raw session metadata.
#[derive(Clone)]
pub struct CustodyExecutionRuntime {
    store: CustodyStore,
    settlements: CustodySettlementService,
    sandbox_base: PathBuf,
    boot_id: Uuid,
}

impl CustodyExecutionRuntime {
    pub fn new(
        store: CustodyStore,
        settlements: CustodySettlementService,
        sandbox_base: PathBuf,
        boot_id: Uuid,
    ) -> Self {
        Self {
            store,
            settlements,
            sandbox_base,
            boot_id,
        }
    }

    /// Authenticate only the exact ordinary tuple or the current persisted
    /// V83 owner/generation before a same-ID handoff resume touches grants,
    /// tokens, admission, config, provider dispatch, or active state.
    pub async fn prepare_handoff_resume(&self, session: &Session) -> Result<PreparedLaunch> {
        let transition = SandboxCustodyTransitionV1::HandoffResume;
        let custody = match CustodyService::classify_for_transition(session, transition) {
            Ok(CustodyClassification::OrdinaryUnsandboxed) => {
                CustodyService::authorize_ordinary_for_transition(session, transition)
            }
            Ok(CustodyClassification::RequiresPersistedAuthentication) => {
                let (mut store, root) = lock_store_then_session_root(&self.store, session.id).await;
                CustodyService::authorize_live_holding(
                    session,
                    &mut store,
                    &self.sandbox_base,
                    transition,
                    root,
                )
            }
            Err(error) => Err(error),
        }?;
        Ok(PreparedLaunch::new(custody))
    }

    /// The transitional raw-config seam is ContextRead, not ProviderLaunch.
    /// Permit drop queues settlement; provider/process lifetime custody remains
    /// deliberately later work.
    pub async fn begin_handoff_context_read(
        &self,
        prepared: &PreparedLaunch,
        session: &Session,
    ) -> Result<CustodyEffectPermit> {
        CustodyService::begin_effect_for_transition(
            &self.store,
            &self.settlements,
            prepared.custody(),
            session,
            &self.sandbox_base,
            self.boot_id,
            EffectKind::ContextRead,
            SandboxCustodyTransitionV1::HandoffResume,
        )
        .await
    }

    /// Authenticate a completed rotation predecessor before the caller creates
    /// a successor id, controller reservation, token, admission, durable row,
    /// context, provider, or active-map entry.  The candidate deliberately
    /// keeps the persisted root/generation private to this module.
    pub async fn prepare_rotation_successor(
        &self,
        predecessor: &Session,
    ) -> Result<RotationCustodyCandidate> {
        self.prepare_rotation_successor_from(predecessor, RotationPredecessorSource::Completed)
            .await
    }

    /// `prepare_rotation_successor` with an explicit predecessor source.
    /// Only restart recovery passes `RecoveredOpenIntent`.
    pub async fn prepare_rotation_successor_from(
        &self,
        predecessor: &Session,
        source: RotationPredecessorSource,
    ) -> Result<RotationCustodyCandidate> {
        let transition = SandboxCustodyTransitionV1::Rotation;
        let (mut store, mut root) = lock_store_then_session_root(&self.store, predecessor.id).await;
        let captured = match source {
            RotationPredecessorSource::Completed => {
                store.capture_completed_rotation_authority(predecessor)
            }
            RotationPredecessorSource::RecoveredOpenIntent => {
                store.capture_recovered_rotation_authority(predecessor)
            }
        };
        let durable_predecessor = match captured {
            Ok(RotationAuthorityCapture::Captured(fence)) => fence,
            Ok(RotationAuthorityCapture::Missing) => {
                return Err(CustodyService::refusal(
                    SandboxCustodyErrorCodeV1::OwnershipMissing,
                    Some(predecessor.id),
                    transition,
                ));
            }
            Ok(RotationAuthorityCapture::Changed) => {
                return Err(CustodyService::refusal(
                    SandboxCustodyErrorCodeV1::CustodyChanged,
                    Some(predecessor.id),
                    transition,
                ));
            }
            Err(_) => {
                return Err(CustodyService::refusal(
                    SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
                    Some(predecessor.id),
                    transition,
                ));
            }
        };
        let custody = match CustodyService::classify_for_transition(predecessor, transition) {
            Ok(CustodyClassification::OrdinaryUnsandboxed) => {
                CustodyService::authorize_ordinary_for_transition(predecessor, transition)
            }
            Ok(CustodyClassification::RequiresPersistedAuthentication) => {
                CustodyService::authorize_live_holding(
                    predecessor,
                    &mut store,
                    &self.sandbox_base,
                    transition,
                    root.take(),
                )
            }
            Err(error) => Err(error),
        }?;
        Ok(RotationCustodyCandidate {
            predecessor_id: predecessor.id,
            durable_predecessor,
            custody,
        })
    }

    /// Apply only the authenticated predecessor shape.  An ordinary rotation
    /// is exactly all-null; a live rotation is an exact same-root Transfer.
    pub fn apply_rotation_successor_tuple(
        candidate: &RotationCustodyCandidate,
        predecessor: &Session,
        successor: &mut Session,
    ) -> Result<()> {
        if candidate.predecessor_id != predecessor.id {
            return Err(CustodyService::refusal(
                SandboxCustodyErrorCodeV1::CustodyChanged,
                Some(predecessor.id),
                SandboxCustodyTransitionV1::Rotation,
            ));
        }
        match &candidate.custody {
            CustodyHandle::Ordinary(_) => {
                successor.sandbox_kind = None;
                successor.sandbox_root = None;
                successor.sandbox_branch = None;
                successor.sandbox_cleanup_state = None;
            }
            CustodyHandle::Sandboxed(_) => {
                successor.sandbox_kind = predecessor.sandbox_kind;
                successor.sandbox_root = predecessor.sandbox_root.clone();
                successor.sandbox_branch = predecessor.sandbox_branch.clone();
                successor.sandbox_cleanup_state = predecessor.sandbox_cleanup_state;
            }
        }
        Ok(())
    }

    /// Bind the already-durable reserved successor and return a phase-explicit
    /// result.  A losing transfer CAS exposes only whether the predecessor is
    /// still current/restorable or has already been superseded by another
    /// winner; it never exposes root identity or generation to the caller.
    pub async fn bind_rotation_successor(
        &self,
        candidate: RotationCustodyCandidate,
        predecessor: &Session,
        successor: &Session,
    ) -> std::result::Result<BoundRotationCustody, RotationBindFailure> {
        if candidate.predecessor_id != predecessor.id
            || successor.continued_from != Some(predecessor.id)
        {
            let store = self.store.lock().await;
            return Err(self
                .settle_and_classify_rotation_refusal(store, &candidate, predecessor, successor.id)
                .await);
        }
        let binding = match &candidate.custody {
            CustodyHandle::Ordinary(_) => SessionCustodyBinding::Ordinary,
            CustodyHandle::Sandboxed(sandboxed) if sandboxed.session_id == predecessor.id => {
                SessionCustodyBinding::Transfer {
                    custody_id: sandboxed.custody_id,
                    from_session_id: predecessor.id,
                    generation: sandboxed.owner_generation,
                    cause: CustodyCause::Rotation,
                    origin_session_id: Some(predecessor.id),
                    scheduled_job_id: None,
                }
            }
            CustodyHandle::Sandboxed(_) => {
                let store = self.store.lock().await;
                return Err(self
                    .settle_and_classify_rotation_refusal(
                        store,
                        &candidate,
                        predecessor,
                        successor.id,
                    )
                    .await);
            }
        };
        let disposition = match binding {
            SessionCustodyBinding::Ordinary => RotationCustodyDisposition::Ordinary,
            SessionCustodyBinding::Transfer { .. } => RotationCustodyDisposition::Transferred,
            _ => unreachable!("rotation only binds ordinary or transfer custody"),
        };
        // Store first, then the transfer root's stripe without ever waiting for
        // it under the Store: a long maintenance proof on a colliding stripe
        // delays only this bind, never an unrelated Store RPC (#1172).
        let (mut store, held) =
            lock_store_then_optional_root(&self.store, binding.custody_id()).await;
        if !store
            .completed_rotation_authority_matches(&candidate.durable_predecessor, predecessor.id)
            .unwrap_or(false)
        {
            drop(held);
            return Err(self
                .settle_and_classify_rotation_refusal(store, &candidate, predecessor, successor.id)
                .await);
        }
        if store
            .bind_reserved_rotation_session_custody_holding(
                successor.id,
                binding,
                &candidate.durable_predecessor,
                held,
            )
            .is_ok()
        {
            Ok(BoundRotationCustody {
                disposition,
                expected: match candidate.custody {
                    CustodyHandle::Ordinary(_) => BoundRotationIdentity::Ordinary {
                        predecessor_id: predecessor.id,
                        durable_predecessor: candidate.durable_predecessor,
                    },
                    CustodyHandle::Sandboxed(sandboxed) => BoundRotationIdentity::Transfer {
                        custody_id: sandboxed.custody_id,
                        predecessor_id: predecessor.id,
                        generation: sandboxed.owner_generation + 1,
                        durable_predecessor: candidate.durable_predecessor,
                    },
                },
            })
        } else {
            Err(self
                .settle_and_classify_rotation_refusal(store, &candidate, predecessor, successor.id)
                .await)
        }
    }

    /// Settle the refused successor and classify the refusal. Takes the Store
    /// guard by value: re-proving a sandboxed predecessor needs its stripe, and
    /// a busy stripe must drop the Store and wait asynchronously instead of
    /// blocking the runtime thread under it (#1172).
    async fn settle_and_classify_rotation_refusal(
        &self,
        mut store: tokio::sync::MutexGuard<'_, Store>,
        candidate: &RotationCustodyCandidate,
        predecessor: &Session,
        successor_id: Uuid,
    ) -> RotationBindFailure {
        if let Err(error) = store.settle_reserved_rotation_custody_failure(
            successor_id,
            SandboxCustodyErrorCodeV1::CustodyChanged,
        ) {
            return RotationBindFailure::SettlementFailed(error);
        }

        if candidate.predecessor_id != predecessor.id {
            return RotationBindFailure::Superseded;
        }
        if !store
            .completed_rotation_authority_matches(&candidate.durable_predecessor, predecessor.id)
            .unwrap_or(false)
        {
            return RotationBindFailure::Superseded;
        }
        let restorable = match &candidate.custody {
            CustodyHandle::Ordinary(_) => true,
            CustodyHandle::Sandboxed(expected) if expected.session_id == predecessor.id => {
                // Atomic with the settlement while the stripe is free; a busy
                // stripe releases the Store and re-proves Store-first.
                let proof = match CustodyService::authorize_live_try(
                    predecessor,
                    &mut store,
                    &self.sandbox_base,
                    SandboxCustodyTransitionV1::Rotation,
                ) {
                    Err(error) if is_lock_order_busy_error(&error) => {
                        drop(store);
                        let (mut store, root) =
                            lock_store_then_session_root(&self.store, predecessor.id).await;
                        // The Store was free while the stripe was busy, so the
                        // authority fence checked above is stale: another path
                        // may have changed the predecessor's status, model,
                        // prompt, routing or invocation without moving the root.
                        // Repeat the full fence under this guard before any
                        // proof, so a stale snapshot is never classified
                        // `Restorable` (and republished). The successor's
                        // settlement above is committed and is not rerun.
                        if !store
                            .completed_rotation_authority_matches(
                                &candidate.durable_predecessor,
                                predecessor.id,
                            )
                            .unwrap_or(false)
                        {
                            return RotationBindFailure::Superseded;
                        }
                        CustodyService::authorize_live_holding(
                            predecessor,
                            &mut store,
                            &self.sandbox_base,
                            SandboxCustodyTransitionV1::Rotation,
                            root,
                        )
                    }
                    proof => proof,
                };
                proof.is_ok_and(|current| current == candidate.custody)
            }
            CustodyHandle::Sandboxed(_) => false,
        };
        if restorable {
            RotationBindFailure::Restorable
        } else {
            RotationBindFailure::Superseded
        }
    }

    /// Drive the refusal settlement and classification directly, as a bind that
    /// lost for an unrelated SQL reason would (#1172 held-stripe test).
    #[cfg(any(test, feature = "test-seam"))]
    pub async fn settle_and_classify_rotation_refusal_for_test(
        &self,
        candidate: &RotationCustodyCandidate,
        predecessor: &Session,
        successor_id: Uuid,
    ) -> RotationBindFailure {
        let store = self.store.lock().await;
        self.settle_and_classify_rotation_refusal(store, candidate, predecessor, successor_id)
            .await
    }

    #[cfg(any(test, feature = "test-seam"))]
    pub fn replace_rotation_candidate_predecessor_for_test(
        candidate: &mut RotationCustodyCandidate,
        predecessor_id: Uuid,
    ) {
        candidate.predecessor_id = predecessor_id;
    }

    #[cfg(any(test, feature = "test-seam"))]
    pub fn replace_rotation_candidate_handle_session_for_test(
        candidate: &mut RotationCustodyCandidate,
        session_id: Uuid,
    ) {
        if let CustodyHandle::Sandboxed(sandboxed) = &mut candidate.custody {
            sandboxed.session_id = session_id;
        }
    }

    pub async fn begin_rotation_context_read(
        &self,
        bound: &BoundRotationCustody,
        successor: &Session,
    ) -> Result<CustodyEffectPermit> {
        let custody = self
            .authenticate_bound_rotation_successor(bound, successor)
            .await?;
        CustodyService::begin_effect_for_transition(
            &self.store,
            &self.settlements,
            &custody,
            successor,
            &self.sandbox_base,
            self.boot_id,
            EffectKind::ContextRead,
            SandboxCustodyTransitionV1::Rotation,
        )
        .await
    }

    /// A bind proves only the durable transition.  Every later effect must
    /// authenticate the *successor* from persisted facts, including the exact
    /// post-transfer generation; retaining the predecessor handle would make
    /// a valid live successor look stale.
    async fn authenticate_bound_rotation_successor(
        &self,
        bound: &BoundRotationCustody,
        successor: &Session,
    ) -> Result<CustodyHandle> {
        let transition = SandboxCustodyTransitionV1::Rotation;
        match &bound.expected {
            BoundRotationIdentity::Ordinary { .. } => {
                match CustodyService::classify_for_transition(successor, transition)? {
                    CustodyClassification::OrdinaryUnsandboxed => {
                        CustodyService::authorize_ordinary_for_transition(successor, transition)
                    }
                    CustodyClassification::RequiresPersistedAuthentication => {
                        Err(CustodyService::refusal(
                            SandboxCustodyErrorCodeV1::CustodyChanged,
                            Some(successor.id),
                            transition,
                        ))
                    }
                }
            }
            BoundRotationIdentity::Transfer {
                custody_id,
                generation,
                ..
            } => {
                let (mut store, root) =
                    lock_store_then_session_root(&self.store, successor.id).await;
                let custody = CustodyService::authorize_live_holding(
                    successor,
                    &mut store,
                    &self.sandbox_base,
                    transition,
                    root,
                )?;
                match &custody {
                    CustodyHandle::Sandboxed(sandboxed)
                        if sandboxed.session_id == successor.id
                            && sandboxed.custody_id == *custody_id
                            && sandboxed.owner_generation == *generation =>
                    {
                        Ok(custody)
                    }
                    _ => Err(CustodyService::refusal(
                        SandboxCustodyErrorCodeV1::CustodyChanged,
                        Some(successor.id),
                        transition,
                    )),
                }
            }
        }
    }

    pub async fn settle_bound_rotation_failure(
        &self,
        successor_id: Uuid,
        bound: &BoundRotationCustody,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        let expected = match &bound.expected {
            BoundRotationIdentity::Ordinary { predecessor_id, .. } => {
                crate::store::sandbox_custody::BoundRotationCustody::Ordinary {
                    predecessor_id: *predecessor_id,
                }
            }
            BoundRotationIdentity::Transfer {
                custody_id,
                predecessor_id,
                generation,
                ..
            } => crate::store::sandbox_custody::BoundRotationCustody::Transfer {
                custody_id: *custody_id,
                predecessor_id: *predecessor_id,
                generation: *generation,
            },
        };
        // Store first, then the transfer root's stripe without waiting for it
        // under the Store (#1172).
        let (mut store, held) =
            lock_store_then_optional_root(&self.store, expected.custody_id()).await;
        store.fail_bound_rotation_successor_holding(successor_id, expected, code, held)
    }

    /// Preserve the exact refusal written by ContextRead revalidation.  A
    /// typed custody refusal carries the bounded code already persisted on an
    /// invalid/quarantined projection; unrelated errors use the transition's
    /// persistence failure class without altering authority state.
    pub async fn settle_rotation_context_failure(
        &self,
        successor_id: Uuid,
        bound: &BoundRotationCustody,
        error: &DaemonError,
    ) -> Result<()> {
        self.settle_bound_rotation_failure(
            successor_id,
            bound,
            custody_error_code(error)
                .unwrap_or(SandboxCustodyErrorCodeV1::PersistenceTransitionFailed),
        )
        .await
    }

    /// Restore an ordinary parent only through the exact Archived-phase Store
    /// transaction. A transfer-bound predecessor remains forward-only.
    pub async fn restore_archived_rotation_predecessor(
        &self,
        expected_parent_id: Uuid,
        bound: &BoundRotationCustody,
    ) -> Result<Option<Session>> {
        let BoundRotationIdentity::Ordinary {
            predecessor_id,
            durable_predecessor,
        } = &bound.expected
        else {
            return Ok(None);
        };
        if *predecessor_id != expected_parent_id {
            return Ok(None);
        }
        let mut store = self.store.lock().await;
        match store.restore_archived_rotation_predecessor(durable_predecessor)? {
            ArchivedRotationRestoration::Restored(session) => Ok(Some(session)),
            ArchivedRotationRestoration::Refused => Ok(None),
        }
    }

    /// The success callback has established the provider. Keep receipt and
    /// archival inside the same guarded Store transition for either disposition.
    pub async fn finalize_rotation_predecessor(
        &self,
        successor_id: Uuid,
        bound: &BoundRotationCustody,
    ) -> Result<bool> {
        let (durable_predecessor, expected) = match &bound.expected {
            BoundRotationIdentity::Ordinary {
                predecessor_id,
                durable_predecessor,
            } => (
                durable_predecessor,
                crate::store::sandbox_custody::BoundRotationCustody::Ordinary {
                    predecessor_id: *predecessor_id,
                },
            ),
            BoundRotationIdentity::Transfer {
                custody_id,
                predecessor_id,
                generation,
                durable_predecessor,
            } => (
                durable_predecessor,
                crate::store::sandbox_custody::BoundRotationCustody::Transfer {
                    custody_id: *custody_id,
                    predecessor_id: *predecessor_id,
                    generation: *generation,
                },
            ),
        };
        // Store first, then the transfer root's stripe without waiting for it
        // under the Store (#1172).
        let (mut store, held) =
            lock_store_then_optional_root(&self.store, expected.custody_id()).await;
        store.finalize_rotation_predecessor_holding(
            durable_predecessor,
            successor_id,
            expected,
            held,
        )
    }

    /// Authenticate the complete durable Failed retry source before a child
    /// UUID, controller reservation, token, model admission, context read, or
    /// provider effect can exist.
    pub async fn prepare_retry_successor(&self, source: &Session) -> Result<RetryCustodyCandidate> {
        let transition = SandboxCustodyTransitionV1::Retry;
        let (mut store, mut root) = lock_store_then_session_root(&self.store, source.id).await;
        let durable_source = match store.capture_failed_retry_authority(source) {
            Ok(RetryAuthorityCapture::Captured(fence)) => fence,
            Ok(RetryAuthorityCapture::Missing) => {
                return Err(CustodyService::refusal(
                    SandboxCustodyErrorCodeV1::OwnershipMissing,
                    Some(source.id),
                    transition,
                ));
            }
            Ok(RetryAuthorityCapture::Changed) => {
                return Err(CustodyService::refusal(
                    SandboxCustodyErrorCodeV1::CustodyChanged,
                    Some(source.id),
                    transition,
                ));
            }
            Err(_) => {
                return Err(CustodyService::refusal(
                    SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
                    Some(source.id),
                    transition,
                ));
            }
        };
        let custody = match CustodyService::classify_for_transition(source, transition) {
            Ok(CustodyClassification::OrdinaryUnsandboxed) => {
                CustodyService::authorize_ordinary_for_transition(source, transition)
            }
            Ok(CustodyClassification::RequiresPersistedAuthentication) => {
                CustodyService::authorize_live_nonmutating_holding(
                    source,
                    &mut store,
                    &self.sandbox_base,
                    transition,
                    root.take(),
                )
            }
            Err(error) => Err(error),
        }?;
        Ok(RetryCustodyCandidate {
            source_id: source.id,
            durable_source,
            custody,
        })
    }

    pub fn apply_retry_successor_tuple(
        candidate: &RetryCustodyCandidate,
        source: &Session,
        successor: &mut Session,
    ) -> Result<()> {
        if candidate.source_id != source.id || successor.continued_from != Some(source.id) {
            return Err(CustodyService::refusal(
                SandboxCustodyErrorCodeV1::CustodyChanged,
                Some(source.id),
                SandboxCustodyTransitionV1::Retry,
            ));
        }
        match &candidate.custody {
            CustodyHandle::Ordinary(_) => {
                successor.sandbox_kind = None;
                successor.sandbox_root = None;
                successor.sandbox_branch = None;
                successor.sandbox_cleanup_state = None;
            }
            CustodyHandle::Sandboxed(_) => {
                successor.sandbox_kind = source.sandbox_kind;
                successor.sandbox_root = source.sandbox_root.clone();
                successor.sandbox_branch = source.sandbox_branch.clone();
                successor.sandbox_cleanup_state = source.sandbox_cleanup_state;
            }
        }
        Ok(())
    }

    /// Bind the C5-reserved retry child.  A live success is forward-only; any
    /// bind loser is durably Failed without changing the source/root/event
    /// chain.
    pub async fn bind_retry_successor(
        &self,
        candidate: &RetryCustodyCandidate,
        source: &Session,
        successor: &Session,
    ) -> std::result::Result<BoundRetryCustody, RetryBindFailure> {
        if candidate.source_id != source.id || successor.continued_from != Some(source.id) {
            return Err(self
                .settle_retry_bind_refusal(candidate, successor.id)
                .await);
        }
        let binding = match &candidate.custody {
            CustodyHandle::Ordinary(_) => SessionCustodyBinding::Ordinary,
            CustodyHandle::Sandboxed(sandboxed) if sandboxed.session_id == source.id => {
                SessionCustodyBinding::Transfer {
                    custody_id: sandboxed.custody_id,
                    from_session_id: source.id,
                    generation: sandboxed.owner_generation,
                    cause: CustodyCause::Retry,
                    origin_session_id: Some(source.id),
                    scheduled_job_id: None,
                }
            }
            CustodyHandle::Sandboxed(_) => {
                return Err(self
                    .settle_retry_bind_refusal(candidate, successor.id)
                    .await);
            }
        };
        let disposition = match binding {
            SessionCustodyBinding::Ordinary => RetryCustodyDisposition::Ordinary,
            SessionCustodyBinding::Transfer { .. } => RetryCustodyDisposition::Transferred,
            _ => unreachable!("retry only binds ordinary or transfer custody"),
        };
        let bind_result = {
            // Hold the same root stripe from persisted/filesystem/Git
            // reauthentication through the SQL Transfer CAS. Ordinary retry
            // has no exclusive root and therefore needs no stripe. The stripe
            // is never waited for while holding the Store (Issue #606).
            let (mut store, held) = match &candidate.custody {
                CustodyHandle::Ordinary(_) => (self.store.lock().await, None),
                CustodyHandle::Sandboxed(expected) => {
                    let (store, guard) =
                        lock_store_then_root(&self.store, expected.custody_id).await;
                    (store, Some(guard))
                }
            };
            let _root_guard = match (&candidate.custody, held) {
                (CustodyHandle::Ordinary(_), _) | (CustodyHandle::Sandboxed(_), None) => None,
                (CustodyHandle::Sandboxed(_), Some(guard)) => {
                    let current = CustodyService::authorize_live_locked_with_policy(
                        source,
                        &mut store,
                        &self.sandbox_base,
                        SandboxCustodyTransitionV1::Retry,
                        false,
                    );
                    if !current.is_ok_and(|current| current == candidate.custody) {
                        None
                    } else {
                        Some(guard)
                    }
                }
            };
            match (&candidate.custody, &_root_guard) {
                (CustodyHandle::Sandboxed(_), None) => Err(DaemonError::Store(
                    "retry source authority changed before custody bind".into(),
                )),
                _ => store.bind_reserved_retry_session_custody_locked(
                    successor,
                    binding,
                    &candidate.durable_source,
                ),
            }
        };
        if bind_result.is_err() {
            return Err(self
                .settle_retry_bind_refusal(candidate, successor.id)
                .await);
        }
        Ok(BoundRetryCustody {
            disposition,
            expected: match &candidate.custody {
                CustodyHandle::Ordinary(_) => BoundRetryIdentity::Ordinary {
                    source_id: source.id,
                },
                CustodyHandle::Sandboxed(sandboxed) => BoundRetryIdentity::Transfer {
                    custody_id: sandboxed.custody_id,
                    source_id: source.id,
                    generation: sandboxed.owner_generation + 1,
                },
            },
        })
    }

    async fn settle_retry_bind_refusal(
        &self,
        candidate: &RetryCustodyCandidate,
        successor_id: Uuid,
    ) -> RetryBindFailure {
        match self
            .store
            .lock()
            .await
            .settle_reserved_retry_custody_failure(
                successor_id,
                candidate.source_id,
                SandboxCustodyErrorCodeV1::CustodyChanged,
            ) {
            Ok(()) => RetryBindFailure::Superseded,
            Err(error) => RetryBindFailure::SettlementFailed(error),
        }
    }

    pub async fn begin_retry_context_read(
        &self,
        bound: &BoundRetryCustody,
        successor: &Session,
    ) -> Result<CustodyEffectPermit> {
        let transition = SandboxCustodyTransitionV1::Retry;
        let custody = match &bound.expected {
            BoundRetryIdentity::Ordinary { .. } => {
                match CustodyService::classify_for_transition(successor, transition)? {
                    CustodyClassification::OrdinaryUnsandboxed => {
                        CustodyService::authorize_ordinary_for_transition(successor, transition)?
                    }
                    CustodyClassification::RequiresPersistedAuthentication => {
                        return Err(CustodyService::refusal(
                            SandboxCustodyErrorCodeV1::CustodyChanged,
                            Some(successor.id),
                            transition,
                        ));
                    }
                }
            }
            BoundRetryIdentity::Transfer {
                custody_id,
                generation,
                ..
            } => {
                let (mut store, root) =
                    lock_store_then_session_root(&self.store, successor.id).await;
                let custody = CustodyService::authorize_live_nonmutating_holding(
                    successor,
                    &mut store,
                    &self.sandbox_base,
                    transition,
                    root,
                )?;
                match &custody {
                    CustodyHandle::Sandboxed(sandboxed)
                        if sandboxed.session_id == successor.id
                            && sandboxed.custody_id == *custody_id
                            && sandboxed.owner_generation == *generation => {}
                    _ => {
                        return Err(CustodyService::refusal(
                            SandboxCustodyErrorCodeV1::CustodyChanged,
                            Some(successor.id),
                            transition,
                        ));
                    }
                }
                custody
            }
        };
        CustodyService::begin_effect_for_transition_nonmutating(
            &self.store,
            &self.settlements,
            &custody,
            successor,
            &self.sandbox_base,
            self.boot_id,
            EffectKind::ContextRead,
            transition,
        )
        .await
    }

    pub async fn settle_bound_retry_failure(
        &self,
        successor_id: Uuid,
        bound: &BoundRetryCustody,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        let expected = match bound.expected {
            BoundRetryIdentity::Ordinary { source_id } => {
                crate::store::sandbox_custody::BoundRotationCustody::Ordinary {
                    predecessor_id: source_id,
                }
            }
            BoundRetryIdentity::Transfer {
                custody_id,
                source_id,
                generation,
            } => crate::store::sandbox_custody::BoundRotationCustody::Transfer {
                custody_id,
                predecessor_id: source_id,
                generation,
            },
        };
        // Store first, then the transfer root's stripe without waiting for it
        // under the Store (#1172).
        let (mut store, held) =
            lock_store_then_optional_root(&self.store, expected.custody_id()).await;
        store.fail_bound_rotation_successor_holding(successor_id, expected, code, held)
    }

    pub async fn settle_retry_context_failure(
        &self,
        successor_id: Uuid,
        bound: &BoundRetryCustody,
        error: &DaemonError,
    ) -> Result<()> {
        self.settle_bound_retry_failure(
            successor_id,
            bound,
            custody_error_code(error)
                .unwrap_or(SandboxCustodyErrorCodeV1::PersistenceTransitionFailed),
        )
        .await
    }

    /// H1-04 (F-011): authenticate the emitting session's custody and capture
    /// its exact clean HEAD OID before any child-side effect exists (no spawn
    /// state transition past the durable reservation, no allocation, no child
    /// session row, no provider/context/config/target work).
    ///
    /// A live sandboxed emitter authenticates through its persisted
    /// root/branch/Git identity (`authorize_live_nonmutating`); an ordinary
    /// emitter authenticates through the verified `ordinary_unsandboxed`
    /// tuple classification. The clean-worktree fence and HEAD capture then
    /// run inside the authenticated effective cwd — the emitter's own sandbox
    /// root for live custody, the canonical working directory for ordinary
    /// custody. Canonical HEAD is never an implicit fallback: a dirty tree is
    /// `source_worktree_dirty` and an unreadable revision is
    /// `source_revision_unavailable`, both refused before effect.
    /// Manager replacement uses the same persisted-root authentication and
    /// clean committed source proof as a fork. Manager authority is checked by
    /// the action journal; this method grants no agent/lead authority.
    pub async fn prepare_manager_action_fork(
        &self,
        source: &Session,
    ) -> Result<SpawnForkCandidate> {
        self.prepare_spawn_fork(source).await
    }

    /// #1133/#1144: like [`Self::prepare_manager_action_fork`] for a launch
    /// pinned at prepare time to `pinned_commit` on `frozen_branch` (the
    /// symbolic ref HEAD was attached to then). A source whose HEAD moved is
    /// accepted only as a verified published fast-forward on that same branch
    /// (the operator's `git pull` of `origin/rolling`; see
    /// [`git_worktree::is_verified_published_fast_forward`]) and still forks
    /// from the pinned commit. Any other move (rewrite, detached HEAD,
    /// switched branch, a clean but unpublished commit, an unrecorded branch)
    /// or a dirty tree is returned as observed so the caller refuses.
    pub async fn prepare_manager_action_fork_pinned(
        &self,
        source: &Session,
        pinned_commit: &str,
        frozen_branch: Option<&str>,
    ) -> Result<SpawnForkCandidate> {
        let fork = self.prepare_spawn_fork(source).await?;
        if fork.fork_commit == pinned_commit {
            return Ok(fork);
        }
        let origin = self.authenticated_fork_origin(source).await?;
        let published = git_worktree::is_verified_published_fast_forward(
            &origin,
            frozen_branch,
            pinned_commit,
            &fork.fork_commit,
        );
        if published {
            return Ok(SpawnForkCandidate {
                fork_origin: fork.fork_origin,
                fork_commit: pinned_commit.to_string(),
            });
        }
        Ok(fork)
    }

    /// Authenticate the same source custody as a normal manager action while
    /// selecting an earlier exact commit already observed and stored by the
    /// daemon. DB-native review uses this after reservation so the author may
    /// advance independently without changing the reviewer's source.
    pub async fn prepare_manager_action_fork_at(
        &self,
        source: &Session,
        expected_commit: &str,
    ) -> Result<SpawnForkCandidate> {
        let source_dir = self.authenticated_fork_origin(source).await?;
        git_worktree::require_commit_object_bounded(&source_dir, expected_commit)?;
        Ok(SpawnForkCandidate {
            fork_origin: source.working_dir.clone(),
            fork_commit: expected_commit.to_string(),
        })
    }

    /// Private filesystem proof for root-manager admission. Unlike an ordinary
    /// child fork, committed source selection does not require a clean tree.
    pub async fn prepare_manager_handoff(
        &self,
        predecessor: &Session,
        handoff: &rsi_common::harness_manager_v2::ManagerCommittedHandoffV2,
    ) -> Result<(
        crate::store::manager_successions::VerifiedManagerHandoff,
        String,
    )> {
        let transition = SandboxCustodyTransitionV1::AgentSpawnChild;
        let authenticate = |store: &mut Store, root: Option<CustodyRootGuard>| -> Result<_> {
            match CustodyService::classify_for_transition(predecessor, transition)? {
                CustodyClassification::OrdinaryUnsandboxed => {
                    CustodyService::authorize_ordinary_for_transition(predecessor, transition)?;
                    Ok(None)
                }
                CustodyClassification::RequiresPersistedAuthentication => {
                    CustodyService::authorize_live_nonmutating_holding(
                        predecessor,
                        store,
                        &self.sandbox_base,
                        transition,
                        root,
                    )?;
                    Ok(Some(store.live_custody_for_session(predecessor.id)?))
                }
            }
        };
        let source_error = |error: crate::error::DaemonError| {
            tracing::warn!(session_id=%predecessor.id, %error, "manager handoff custody authentication refused");
            crate::store::harness_manager_v2::refused("manager_succession_source_custody_changed")
        };
        let before = {
            let (mut store, root) = lock_store_then_session_root(&self.store, predecessor.id).await;
            authenticate(&mut store, root)
        }
        .map_err(source_error)?;
        let cwd = before.as_ref().map_or_else(
            || predecessor.working_dir.clone(),
            |c| PathBuf::from(&c.sandbox_root),
        );
        let frozen = handoff.clone();
        let content =
            tokio::task::spawn_blocking(move || git_worktree::read_manager_handoff(&cwd, &frozen))
                .await
                .map_err(|_| {
                    crate::store::harness_manager_v2::refused(
                        "manager_succession_handoff_unavailable",
                    )
                })??;
        let after = {
            let (mut store, root) = lock_store_then_session_root(&self.store, predecessor.id).await;
            authenticate(&mut store, root)
        }
        .map_err(source_error)?;
        if before
            .as_ref()
            .map(crate::store::manager_successions::ManagerRootCustody::from)
            != after
                .as_ref()
                .map(crate::store::manager_successions::ManagerRootCustody::from)
        {
            return Err(crate::store::harness_manager_v2::refused(
                "manager_succession_source_custody_changed",
            ));
        }
        Ok((
            crate::store::manager_successions::VerifiedManagerHandoff::from_authenticated_source(
                predecessor,
                handoff,
                after.as_ref(),
            )?,
            content,
        ))
    }

    /// Revalidate the allocated candidate's real root/branch/repository and
    /// generation without transferring or rewriting either custody aggregate.
    pub async fn authenticate_manager_candidate(
        &self,
        candidate: &Session,
        expected: &crate::store::manager_successions::ManagerRootCustody,
    ) -> Result<()> {
        let (mut store, root) = lock_store_then_session_root(&self.store, candidate.id).await;
        CustodyService::authorize_live_nonmutating_holding(
            candidate,
            &mut store,
            &self.sandbox_base,
            SandboxCustodyTransitionV1::AgentSpawnChild,
            root,
        )
        .map_err(|error| {
            tracing::warn!(session_id=%candidate.id, %error, "manager candidate custody authentication refused");
            crate::store::harness_manager_v2::refused(
                "manager_succession_candidate_custody_changed",
            )
        })?;
        if crate::store::manager_successions::ManagerRootCustody::from(
            &store.live_custody_for_session(candidate.id)?,
        ) != *expected
        {
            return Err(crate::store::harness_manager_v2::refused(
                "manager_succession_candidate_custody_changed",
            ));
        }
        Ok(())
    }

    pub async fn prepare_spawn_fork(&self, emitter: &Session) -> Result<SpawnForkCandidate> {
        let source_dir = self.authenticated_fork_origin(emitter).await?;
        let (clean, fork_commit) =
            git_worktree::observe_clean_head_bounded(&source_dir).map_err(|_| {
                CustodyService::refusal(
                    SandboxCustodyErrorCodeV1::SourceRevisionUnavailable,
                    Some(emitter.id),
                    SandboxCustodyTransitionV1::AgentSpawnChild,
                )
            })?;
        if !clean {
            return Err(CustodyService::refusal(
                SandboxCustodyErrorCodeV1::SourceWorktreeDirty,
                Some(emitter.id),
                SandboxCustodyTransitionV1::AgentSpawnChild,
            ));
        }
        Ok(SpawnForkCandidate {
            fork_origin: emitter.working_dir.clone(),
            fork_commit,
        })
    }

    async fn authenticated_fork_origin(&self, emitter: &Session) -> Result<PathBuf> {
        let transition = SandboxCustodyTransitionV1::AgentSpawnChild;
        let custody = match CustodyService::classify_for_transition(emitter, transition)? {
            CustodyClassification::OrdinaryUnsandboxed => {
                CustodyService::authorize_ordinary_for_transition(emitter, transition)?
            }
            CustodyClassification::RequiresPersistedAuthentication => {
                let (mut store, root) = lock_store_then_session_root(&self.store, emitter.id).await;
                CustodyService::authorize_live_nonmutating_holding(
                    emitter,
                    &mut store,
                    &self.sandbox_base,
                    transition,
                    root,
                )?
            }
        };
        let source_dir = match &custody {
            CustodyHandle::Ordinary(_) => emitter.working_dir.clone(),
            CustodyHandle::Sandboxed(_) => emitter.sandbox_root.clone().ok_or_else(|| {
                CustodyService::refusal(
                    SandboxCustodyErrorCodeV1::TupleIncomplete,
                    Some(emitter.id),
                    transition,
                )
            })?,
        };
        Ok(source_dir)
    }

    /// H1-04 (F-011): settle a refused/failed AgentSpawnChild custody
    /// transition. The reserved spawn row fails closed with
    /// `safe_error_class = sandbox_custody.<code>` and the reserved child
    /// session id becomes a durably Failed, non-executable row with
    /// `stop_reason = sandbox_custody:<code>` and an invalid projection. The
    /// emitter's custody, root history, and event chain are untouched.
    pub async fn settle_agent_spawn_custody_failure(
        &self,
        spawn_request_id: Uuid,
        child: &Session,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        self.store
            .lock()
            .await
            .settle_failed_agent_spawn_custody(spawn_request_id, child, code)
    }
}

/// H1-04 (F-011): opaque authenticated fork source for one AgentSpawnChild
/// establishment. Constructed only by
/// [`CustodyExecutionRuntime::prepare_spawn_fork`]; the spawn coordinator and
/// general launch code never see raw custody identity — only the exact
/// (origin repo, clean HEAD OID) pair the sandbox allocator consumes.
#[derive(Debug, Clone)]
pub struct SpawnForkCandidate {
    fork_origin: PathBuf,
    fork_commit: String,
}

impl SpawnForkCandidate {
    /// Repository handle the allocator forks from. This is the emitter's
    /// canonical repository directory used strictly as an object-store
    /// handle; revision authority is exclusively [`Self::fork_commit`].
    pub fn fork_origin(&self) -> &Path {
        &self.fork_origin
    }

    /// Exact clean emitter HEAD OID captured at authentication time.
    pub fn fork_commit(&self) -> &str {
        &self.fork_commit
    }
}

pub struct RotationCustodyCandidate {
    predecessor_id: Uuid,
    durable_predecessor: RotationAuthorityFence,
    custody: CustodyHandle,
}

pub struct BoundRotationCustody {
    disposition: RotationCustodyDisposition,
    expected: BoundRotationIdentity,
}

enum BoundRotationIdentity {
    Ordinary {
        predecessor_id: Uuid,
        durable_predecessor: RotationAuthorityFence,
    },
    Transfer {
        custody_id: Uuid,
        predecessor_id: Uuid,
        generation: u64,
        durable_predecessor: RotationAuthorityFence,
    },
}

impl BoundRotationCustody {
    pub fn disposition(&self) -> RotationCustodyDisposition {
        self.disposition
    }
}

/// Which durable predecessor state a rotation decision authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RotationPredecessorSource {
    /// Live rotation: the predecessor settled `Completed`.
    Completed,
    /// Restart recovery: restore reconciled the predecessor `Failed`
    /// (process died) while its latest rotation intent was still open.
    RecoveredOpenIntent,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RotationCustodyDisposition {
    Ordinary,
    Transferred,
}

#[derive(Debug)]
pub enum RotationBindFailure {
    Restorable,
    Superseded,
    SettlementFailed(DaemonError),
}

pub struct RetryCustodyCandidate {
    source_id: Uuid,
    durable_source: RetryAuthorityFence,
    custody: CustodyHandle,
}

#[derive(Clone)]
pub struct BoundRetryCustody {
    disposition: RetryCustodyDisposition,
    expected: BoundRetryIdentity,
}

#[derive(Clone, Copy)]
enum BoundRetryIdentity {
    Ordinary {
        source_id: Uuid,
    },
    Transfer {
        custody_id: Uuid,
        source_id: Uuid,
        generation: u64,
    },
}

impl BoundRetryCustody {
    pub fn disposition(&self) -> RetryCustodyDisposition {
        self.disposition
    }

    /// The bound identity of a retry successor that owns `custody_id`'s
    /// transferred root at `generation` (the post-transfer generation), as
    /// `bind_retry_successor` returns it, for tests that bind the transfer
    /// directly (#1179).
    #[cfg(any(test, feature = "test-seam"))]
    pub fn transfer_for_test(custody_id: Uuid, source_id: Uuid, generation: u64) -> Self {
        Self {
            disposition: RetryCustodyDisposition::Transferred,
            expected: BoundRetryIdentity::Transfer {
                custody_id,
                source_id,
                generation,
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RetryCustodyDisposition {
    Ordinary,
    Transferred,
}

#[derive(Debug)]
pub enum RetryBindFailure {
    Superseded,
    SettlementFailed(DaemonError),
}

pub fn custody_error_code(error: &DaemonError) -> Option<SandboxCustodyErrorCodeV1> {
    let DaemonError::StructuredRpc { data, .. } = error else {
        return None;
    };
    serde_json::from_value::<SandboxCustodyErrorV1>(data.get("error")?.clone())
        .ok()
        .map(|error| error.code)
}

/// The number of effects that can be admitted while their terminal release is
/// awaiting persistence.  A permit reserves one of these slots *before* the
/// durable effect counter is incremented, so Drop never has to choose between
/// blocking and discarding a release because a queue filled after admission.
const EFFECT_SETTLEMENT_CAPACITY: usize = 64;

#[cfg(any(test, feature = "test-seam"))]
static STARTUP_DIAGNOSTICS: std::sync::Mutex<Vec<(String, String)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn take_startup_diagnostics_for_test() -> Vec<(String, String)> {
    std::mem::take(
        &mut *STARTUP_DIAGNOSTICS
            .lock()
            .expect("startup diagnostics lock"),
    )
}

/// Deterministic startup settlement seam. The injected error is consumed at
/// the real classification-to-invalidation boundary, never at startup entry.
#[cfg(any(test, feature = "test-seam"))]
static STARTUP_SETTLEMENT_FAILURE: AtomicU8 = AtomicU8::new(0);

#[cfg(any(test, feature = "test-seam"))]
#[derive(Clone, Copy)]
pub(crate) enum StartupSettlementFailureForTest {
    CustodyChanged,
    PersistenceTransitionFailed,
    UnknownStructured,
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn fail_next_startup_settlement_for_test(failure: StartupSettlementFailureForTest) {
    let encoded = match failure {
        StartupSettlementFailureForTest::CustodyChanged => 1,
        StartupSettlementFailureForTest::PersistenceTransitionFailed => 2,
        StartupSettlementFailureForTest::UnknownStructured => 3,
    };
    STARTUP_SETTLEMENT_FAILURE.store(encoded, Ordering::Release);
}

#[cfg(any(test, feature = "test-seam"))]
fn startup_settlement_failure_for_test() -> Option<DaemonError> {
    match STARTUP_SETTLEMENT_FAILURE.swap(0, Ordering::AcqRel) {
        1 => Some(CustodyService::refusal(
            SandboxCustodyErrorCodeV1::CustodyChanged,
            None,
            SandboxCustodyTransitionV1::StartupReconciliation,
        )),
        2 => Some(CustodyService::refusal(
            SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
            None,
            SandboxCustodyTransitionV1::StartupReconciliation,
        )),
        3 => Some(DaemonError::StructuredRpc {
            rpc_code: -32603,
            message: "sandbox_custody:unknown_structured_startup_failure".into(),
            data: serde_json::Value::Null,
        }),
        _ => None,
    }
}

#[cfg(not(any(test, feature = "test-seam")))]
fn startup_settlement_failure_for_test() -> Option<DaemonError> {
    None
}

/// Test seam: run once for one sandbox base between the concurrent prefetch
/// and the serial pass, to change the filesystem inside that window.
#[cfg(any(test, feature = "test-seam"))]
type AfterStartupPrefetchHook = (PathBuf, Box<dyn FnOnce() + Send>);

#[cfg(any(test, feature = "test-seam"))]
static AFTER_STARTUP_PREFETCH: std::sync::Mutex<Option<AfterStartupPrefetchHook>> =
    std::sync::Mutex::new(None);

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn after_startup_prefetch_for_test(sandbox_base: &Path) {
    let hook = {
        let mut slot = AFTER_STARTUP_PREFETCH
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match slot.as_ref() {
            Some((base, _)) if base == sandbox_base => slot.take(),
            _ => None,
        }
    };
    if let Some((_, hook)) = hook {
        hook();
    }
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn set_after_startup_prefetch_for_test(
    sandbox_base: &Path,
    hook: Box<dyn FnOnce() + Send>,
) {
    *AFTER_STARTUP_PREFETCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((sandbox_base.to_path_buf(), hook));
}

#[cfg(not(any(test, feature = "test-seam")))]
const fn after_startup_prefetch_for_test(_sandbox_base: &Path) {}

/// Test seam: for one sandbox base, delay each prefetched probe so later jobs
/// finish first, and record the completion order.
#[cfg(any(test, feature = "test-seam"))]
static STARTUP_PROBE_REVERSAL: std::sync::Mutex<Option<(PathBuf, Vec<Uuid>)>> =
    std::sync::Mutex::new(None);

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn reverse_startup_probe_completion_for_test(sandbox_base: Option<&Path>) -> Vec<Uuid> {
    let mut hook = STARTUP_PROBE_REVERSAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = hook.take().map(|(_, order)| order).unwrap_or_default();
    *hook = sandbox_base.map(|base| (base.to_path_buf(), Vec::new()));
    previous
}

#[cfg(any(test, feature = "test-seam"))]
fn startup_probe_completed_for_test(
    sandbox_base: &Path,
    index: usize,
    jobs: usize,
    custody_id: Uuid,
) {
    let applies = STARTUP_PROBE_REVERSAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|(base, _)| base == sandbox_base);
    if !applies {
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(40 * (jobs - index) as u64));
    if let Some((_, order)) = STARTUP_PROBE_REVERSAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        order.push(custody_id);
    }
}

#[cfg(not(any(test, feature = "test-seam")))]
const fn startup_probe_completed_for_test(
    _sandbox_base: &Path,
    _index: usize,
    _jobs: usize,
    _custody_id: Uuid,
) {
}

fn retained_unowned_diagnostic(sandbox_root: Option<&str>, reason: &'static str) {
    tracing::warn!(
        sandbox_root = sandbox_root.unwrap_or(""),
        reason,
        retained_unowned = true,
        "retained_unowned sandbox custody diagnostic"
    );
    #[cfg(any(test, feature = "test-seam"))]
    STARTUP_DIAGNOSTICS
        .lock()
        .expect("startup diagnostics lock")
        .push((
            sandbox_root.unwrap_or_default().to_owned(),
            reason.to_owned(),
        ));
}

#[derive(Debug, Clone)]
pub enum CustodyIntent {
    Ordinary,
    Allocate {
        spec: SandboxSpec,
        source: WorktreeSource,
    },
    Reuse {
        owner_session_id: Uuid,
    },
    Transfer {
        from_session_id: Uuid,
    },
}

#[derive(Debug, Clone)]
pub enum WorktreeSource {
    CanonicalHead { canonical_working_dir: PathBuf },
    SessionHead { source_session_id: Uuid },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustodyHandle {
    Ordinary(OrdinaryCustody),
    Sandboxed(SandboxedCustody),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrdinaryCustody {
    canonical_working_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SandboxedCustody {
    session_id: Uuid,
    custody_id: Uuid,
    owner_generation: u64,
}

/// Provider launch input reserved for the sealed boundary. It intentionally
/// contains no caller-readable filesystem path.
#[derive(Debug)]
pub struct PreparedLaunch {
    custody: CustodyHandle,
}

impl PreparedLaunch {
    pub fn new(custody: CustodyHandle) -> Self {
        Self { custody }
    }

    pub fn custody(&self) -> &CustodyHandle {
        &self.custody
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    ContextRead,
    ProviderLaunch,
    ProviderTurn,
    ToolExecution,
    BuildCacheReclaim,
}

/// Non-cloneable authorization for exactly one daemon-controlled effect.
pub struct CustodyEffectPermit {
    effective_cwd: PathBuf,
    cargo_target_dir: Option<PathBuf>,
    kind: EffectKind,
    settlement: Option<PendingSettlement>,
}

struct EffectSettlement {
    reservation: EffectReservation,
    _capacity: OwnedSemaphorePermit,
}

struct PendingSettlement {
    handoff: mpsc::OwnedPermit<SettlementMessage>,
    settlement: EffectSettlement,
}

enum SettlementMessage {
    Release(EffectSettlement),
    Flush(oneshot::Sender<()>),
}

enum SettlementControl {
    Retry(oneshot::Sender<std::result::Result<usize, String>>),
    Drain(oneshot::Sender<std::result::Result<(), String>>),
}

/// Manager-owned same-boot settlement service.  It deliberately shares the
/// SessionManager's async Store ownership; custody never creates a second
/// synchronous Store regime.  Failed releases retain their admission slot in
/// the bounded retry deque until a later recovery pass settles them.
#[derive(Clone)]
pub struct CustodySettlementService {
    producer: Arc<CustodySettlementProducer>,
}

struct CustodySettlementProducer {
    // Taking this sender is the atomic admission seal. The worker never owns
    // this producer, so its receiver cannot retain its own sender or Store.
    sender: std::sync::Mutex<Option<mpsc::Sender<SettlementMessage>>>,
    slots: Arc<Semaphore>,
    state: Arc<CustodySettlementState>,
}

struct CustodySettlementState {
    store: CustodyStore,
    failed: Mutex<VecDeque<EffectSettlement>>,
    accepting: AtomicBool,
    sealed: AtomicBool,
}

/// The manager-owned half of settlement. It is intentionally non-cloneable:
/// producer handles can outlive effects, but only the manager owns joining and
/// retry/drain control of the receiver task.
pub struct CustodySettlementWorker {
    state: Arc<CustodySettlementState>,
    control: mpsc::Sender<SettlementControl>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CustodySettlementService {
    pub fn new(store: CustodyStore) -> Result<(Self, CustodySettlementWorker)> {
        let (sender, mut receiver) = mpsc::channel(EFFECT_SETTLEMENT_CAPACITY);
        let state = Arc::new(CustodySettlementState {
            store,
            failed: Mutex::new(VecDeque::new()),
            accepting: AtomicBool::new(true),
            sealed: AtomicBool::new(false),
        });
        let producer = Arc::new(CustodySettlementProducer {
            sender: std::sync::Mutex::new(Some(sender)),
            slots: Arc::new(Semaphore::new(EFFECT_SETTLEMENT_CAPACITY)),
            state: Arc::clone(&state),
        });
        let (control, mut controls) = mpsc::channel(2);
        let worker_state = Arc::clone(&state);
        let task = tokio::runtime::Handle::try_current()
            .map_err(|error| {
                DaemonError::Store(format!("custody settlement runtime unavailable: {error}"))
            })?
            .spawn(async move {
                let mut releases_open = true;
                let mut draining: Option<oneshot::Sender<std::result::Result<(), String>>> = None;
                loop {
                    if !releases_open {
                        if let Some(done) = draining.take() {
                            match retry_failed_effects(&worker_state).await {
                                Ok(_) => {
                                    let _ = done.send(Ok(()));
                                    break;
                                }
                                Err(error) => {
                                    let _ = done.send(Err(error.to_string()));
                                }
                            }
                        }
                    }
                    tokio::select! {
                        message = receiver.recv(), if releases_open => match message {
                            Some(SettlementMessage::Release(settlement)) => {
                                retain_or_settle_effect(&worker_state, settlement).await;
                            }
                            Some(SettlementMessage::Flush(done)) => { let _ = done.send(()); }
                            None => {
                                releases_open = false;
                                worker_state.accepting.store(false, Ordering::Release);
                            }
                        },
                        control = controls.recv() => match control {
                            Some(SettlementControl::Retry(done)) => {
                                let result = retry_failed_effects(&worker_state).await
                                    .map_err(|error| error.to_string());
                                let _ = done.send(result);
                            }
                            Some(SettlementControl::Drain(done)) => {
                                draining = Some(done);
                            }
                            None => break,
                        },
                    }
                }
            });
        Ok((
            Self { producer },
            CustodySettlementWorker {
                state,
                control,
                task: Mutex::new(Some(task)),
            },
        ))
    }

    fn reserve(&self) -> Result<(OwnedSemaphorePermit, mpsc::OwnedPermit<SettlementMessage>)> {
        if !self.producer.state.accepting.load(Ordering::Acquire) {
            return Err(DaemonError::Store(
                "custody effect settlement service is degraded".into(),
            ));
        }
        let capacity = Arc::clone(&self.producer.slots)
            .try_acquire_owned()
            .map_err(|_| {
                DaemonError::Store("custody effect settlement capacity exhausted".into())
            })?;
        // The sender lock is also held by seal(). A successful owned permit is
        // a physical queue slot, so Drop has no Full/Closed failure path.
        let sender = self
            .producer
            .sender
            .lock()
            .expect("settlement sender poisoned")
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                DaemonError::Store("custody effect settlement service is sealed".into())
            })?;
        let handoff = sender.try_reserve_owned().map_err(|_| {
            DaemonError::Store("custody effect settlement queue capacity exhausted".into())
        })?;
        if !self.producer.state.accepting.load(Ordering::Acquire) {
            return Err(DaemonError::Store(
                "custody effect settlement service is degraded".into(),
            ));
        }
        Ok((capacity, handoff))
    }

    pub(crate) fn seal(&self) {
        self.producer
            .state
            .accepting
            .store(false, Ordering::Release);
        self.producer.state.sealed.store(true, Ordering::Release);
        let _ = self
            .producer
            .sender
            .lock()
            .expect("settlement sender poisoned")
            .take();
    }

    #[cfg(any(test, feature = "test-seam"))]
    pub(crate) async fn flush_for_test(&self) {
        let (done, received) = oneshot::channel();
        let sender = self
            .producer
            .sender
            .lock()
            .expect("settlement sender poisoned")
            .as_ref()
            .cloned()
            .expect("custody settlement worker must remain available");
        sender
            .send(SettlementMessage::Flush(done))
            .await
            .expect("custody settlement worker must remain available");
        received
            .await
            .expect("custody settlement flush must complete");
    }

    #[cfg(any(test, feature = "test-seam"))]
    pub(crate) async fn failed_count(&self) -> usize {
        self.producer.state.failed.lock().await.len()
    }

    #[cfg(any(test, feature = "test-seam"))]
    pub(crate) fn degrade_for_test(&self) {
        self.producer
            .state
            .accepting
            .store(false, Ordering::Release);
    }
}

impl CustodySettlementWorker {
    pub(crate) async fn retry(&self) -> Result<usize> {
        let (done, received) = oneshot::channel();
        self.control
            .send(SettlementControl::Retry(done))
            .await
            .map_err(|_| DaemonError::Store("custody settlement worker is not running".into()))?;
        received
            .await
            .map_err(|_| DaemonError::Store("custody settlement retry cancelled".into()))?
            .map_err(DaemonError::Store)
    }

    pub async fn shutdown(&self, producer: &CustodySettlementService) -> Result<()> {
        producer.seal();
        let (done, received) = oneshot::channel();
        self.control
            .send(SettlementControl::Drain(done))
            .await
            .map_err(|_| DaemonError::Store("custody settlement worker is not running".into()))?;
        received
            .await
            .map_err(|_| DaemonError::Store("custody settlement drain cancelled".into()))?
            .map_err(DaemonError::Store)?;
        let task = self.task.lock().await.take();
        if let Some(task) = task {
            task.await.map_err(|error| {
                DaemonError::Store(format!("custody settlement worker join failed: {error}"))
            })?;
        }
        Ok(())
    }
}

async fn retain_or_settle_effect(state: &CustodySettlementState, settlement: EffectSettlement) {
    if let Err((error, settlement)) = settle_effect_async(&state.store, settlement).await {
        tracing::error!(custody_id = %settlement.reservation.custody_id, generation = settlement.reservation.generation, error = %error, "custody effect release retained for same-boot recovery");
        state.accepting.store(false, Ordering::Release);
        state.failed.lock().await.push_back(settlement);
    }
}

async fn retry_failed_effects(state: &CustodySettlementState) -> Result<usize> {
    let mut pending = {
        let mut failed = state.failed.lock().await;
        std::mem::take(&mut *failed)
    };
    let mut recovered = 0;
    let mut first_error = None;
    while let Some(settlement) = pending.pop_front() {
        match settle_effect_async(&state.store, settlement).await {
            Ok(()) => recovered += 1,
            Err((error, settlement)) => {
                first_error.get_or_insert(error);
                state.failed.lock().await.push_back(settlement);
            }
        }
    }
    if state.failed.lock().await.is_empty() && !state.sealed.load(Ordering::Acquire) {
        state.accepting.store(true, Ordering::Release);
    }
    first_error.map_or(Ok(recovered), Err)
}

async fn settle_effect_async(
    store: &CustodyStore,
    settlement: EffectSettlement,
) -> std::result::Result<(), (DaemonError, EffectSettlement)> {
    let reservation = settlement.reservation;
    // Store first, then the reservation's stripe without ever waiting for it
    // under the Store: a long maintenance proof on a colliding stripe delays
    // only this release, which retries asynchronously, never an unrelated Store
    // RPC (#1172).
    let (mut guard, root) = lock_store_then_root(store, reservation.custody_id).await;
    let released = guard.release_effect_locked(reservation);
    drop(root);
    drop(guard);
    released.map_err(|error| (error, settlement))
}

impl CustodyEffectPermit {
    pub fn effective_cwd(&self) -> &Path {
        &self.effective_cwd
    }

    pub fn cargo_target_dir(&self) -> Option<&Path> {
        self.cargo_target_dir.as_deref()
    }

    /// Scratch construction is authorized only by the ContextRead effect
    /// whose authenticated cwd it consumes.  Keep the effect kind private;
    /// callers receive only the exact predicate needed by that boundary.
    pub(crate) const fn is_context_read(&self) -> bool {
        matches!(self.kind, EffectKind::ContextRead)
    }

    #[cfg(any(test, feature = "test-seam"))]
    pub(crate) fn for_execution_scratch_test(
        effective_cwd: PathBuf,
        cargo_target_dir: Option<PathBuf>,
        kind: EffectKind,
    ) -> Self {
        Self {
            effective_cwd,
            cargo_target_dir,
            kind,
            settlement: None,
        }
    }
}

impl Drop for CustodyEffectPermit {
    fn drop(&mut self) {
        let Some(pending) = self.settlement.take() else {
            return;
        };
        // An OwnedPermit reserves a real queue slot before durable admission.
        // Its send path is infallible and cannot strand the active counter.
        let _sender = pending
            .handoff
            .send(SettlementMessage::Release(pending.settlement));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyClassification {
    OrdinaryUnsandboxed,
    RequiresPersistedAuthentication,
}

/// The only domain classifier allowed to treat an all-null tuple as ordinary.
/// Any partial or historical tuple is a refusal, never a canonical fallback.
#[derive(Debug, Default)]
pub struct CustodyService;

impl CustodyService {
    /// Classify every persisted session before restore rebuilds any runtime
    /// maps. This is deliberately a startup-only authentication pass: it
    /// performs no provider/context/tool effect and never invents a legacy
    /// owner from lineage or a canonical fallback from a malformed tuple.
    pub fn reconcile_startup(store: &mut Store, sandbox_base: &Path) -> Result<()> {
        Self::reconcile_startup_with_pool(store, sandbox_base, startup_proof_pool_size())
    }

    /// `reconcile_startup` with an explicit proof pool; `pool <= 1` is the
    /// fully serial pass.
    pub(crate) fn reconcile_startup_with_pool(
        store: &mut Store,
        sandbox_base: &Path,
        pool: usize,
    ) -> Result<()> {
        const STARTUP_GROUP_BATCH: usize = 64;
        // Every aggregate reader below requires an allocation identity; a
        // legacy row without one used to abort the whole restore (and with it
        // restart-intent recovery) on every boot.
        let repaired = store.repair_missing_custody_allocation_ids()?;
        if repaired > 0 {
            tracing::warn!(
                repaired,
                "Backfilled missing sandbox custody allocation identities from allocation events"
            );
        }
        // No provider has been restored yet. A crash after reservation but
        // before bind leaves a Starting child without custody; settle it
        // before aggregate classification sees its cached root as a rival.
        let mut successor_after = None;
        loop {
            let page =
                store.startup_unbound_successor_page(successor_after, STARTUP_GROUP_BATCH)?;
            if page.is_empty() {
                break;
            }
            for successor_id in &page {
                store.settle_reserved_rotation_custody_failure(
                    *successor_id,
                    SandboxCustodyErrorCodeV1::OwnershipMissing,
                )?;
            }
            successor_after = page.last().copied();
        }
        // One worktree listing per repository for the whole classification
        // pass, and every live-root proof computed concurrently up front
        // (#961); see `WorktreeListings`.
        let mut listings = Self::prefetch_startup_live_proofs(store, sandbox_base, pool)?;
        after_startup_prefetch_for_test(sandbox_base);
        let mut after = None;
        loop {
            let groups = store.startup_custody_group_page(after.as_deref(), STARTUP_GROUP_BATCH)?;
            let Some(last) = groups.last().map(|group| group.key.clone()) else {
                break;
            };
            for group in groups {
                if group.custody_id.is_some() {
                    Self::reconcile_startup_existing_aggregate(
                        store,
                        sandbox_base,
                        &group,
                        &mut listings,
                    )?;
                } else {
                    let sessions = Self::bounded_legacy_group_sessions(store, &group)?;
                    match group.sandbox_root.as_deref() {
                        Some(root) => Self::reconcile_startup_root_group(
                            store,
                            sandbox_base,
                            root,
                            sessions,
                            &mut listings,
                        )?,
                        None => Self::reconcile_startup_rootless_session(
                            store,
                            sessions.into_iter().next().ok_or_else(|| {
                                DaemonError::Store("empty rootless startup custody group".into())
                            })?,
                        )?,
                    }
                }
            }
            after = Some(last);
        }
        // Diagnostics only: a failure here must not turn into a failed
        // restore after every authoritative group reconciled.
        if let Err(error) = Self::log_retained_unowned_diagnostics(store, sandbox_base) {
            tracing::warn!(%error, "retained_unowned sandbox custody diagnostics skipped");
        }
        Ok(())
    }

    /// Legacy reconstruction is intentionally bounded: its lineage is held
    /// in memory for graph authentication, so refuse a root with more than
    /// this many candidates instead of allocating proportional to a corrupt
    /// historical lineage. Durable aggregates use participant pages below.
    fn bounded_legacy_group_sessions(
        store: &Store,
        group: &StartupCustodyGroup,
    ) -> Result<Vec<Session>> {
        const LEGACY_PARTICIPANT_CAP: usize = 128;
        let sessions =
            store.startup_custody_group_sessions_page(group, None, LEGACY_PARTICIPANT_CAP + 1)?;
        if sessions.len() > LEGACY_PARTICIPANT_CAP {
            return Err(DaemonError::Store(
                "legacy sandbox custody participant cap exceeded".into(),
            ));
        }
        Ok(sessions)
    }

    /// Existing aggregates are authoritative only after every durable link
    /// and every persisted same-root claimant has been inspected in bounded
    /// deterministic pages. This prevents an authenticated owner from
    /// masking a duplicate executable contender.
    fn reconcile_startup_existing_aggregate(
        store: &mut Store,
        sandbox_base: &Path,
        group: &StartupCustodyGroup,
        listings: &mut WorktreeListings,
    ) -> Result<()> {
        const PARTICIPANT_PAGE: usize = 64;
        let custody_id = group.custody_id.expect("aggregate group checked");
        if store.startup_custody_has_settlement_fence(custody_id)? {
            return Ok(());
        }
        let root_path = group
            .sandbox_root
            .as_deref()
            .ok_or_else(|| DaemonError::Store("aggregate startup group lacks root".into()))?;
        let root = store
            .startup_custody_root_for_sandbox_root(root_path)?
            .ok_or_else(|| DaemonError::Store("aggregate disappeared during startup".into()))?;
        if root.custody_id != custody_id {
            return Err(DaemonError::Store(
                "aggregate startup root key drifted".into(),
            ));
        }
        if root.sandbox_root != root_path {
            return Err(DaemonError::Store(
                "aggregate startup root identity drifted".into(),
            ));
        }

        let mut after = None;
        let mut owner_seen = false;
        let mut conflict = false;
        loop {
            let page = store.startup_custody_group_sessions_page(group, after, PARTICIPANT_PAGE)?;
            let Some(last) = page.last().map(|session| session.id) else {
                break;
            };
            for session in &page {
                if root.owner_session_id == Some(session.id) {
                    owner_seen = true;
                }
                let linked = store.startup_session_has_custody_link(session.id, custody_id)?;
                if !linked
                    && root
                        .owner_session_id
                        .is_some_and(|owner| session.continued_from == Some(owner))
                    && store.startup_settled_unbound_successor(
                        session.id,
                        root.owner_session_id.expect("checked predecessor"),
                        custody_id,
                    )?
                {
                    continue;
                }
                let ownership_generation =
                    store.startup_session_ownership_generation(session.id, custody_id)?;
                let claims_root = session.sandbox_root.as_deref() == Some(Path::new(root_path));
                let matches_static_identity = session.working_dir.to_string_lossy()
                    == root.canonical_repo_dir
                    && matches!(session.sandbox_kind, Some(SandboxKind::GitWorktree));
                // Terminal tombstones are allowed to be rootless, but every
                // claimant must still carry the immutable SQL custody link and
                // immutable ownership event.  Event discovery is bidirectional
                // so nulling both cached Session facts cannot hide a claimant.
                let live_tuple = matches!(session.sandbox_kind, Some(SandboxKind::GitWorktree))
                    && claims_root
                    && session.sandbox_branch.as_deref() == Some(root.sandbox_branch.as_str())
                    && matches!(
                        session.sandbox_cleanup_state,
                        Some(SandboxCleanupState::Live)
                    );
                let executable = matches!(
                    session.status,
                    SessionStatus::Starting
                        | SessionStatus::Running
                        | SessionStatus::WaitingApproval
                );
                let state_compatible = if root.state == "live" {
                    let is_owner = root.owner_session_id == Some(session.id);
                    let expected_generation = if is_owner {
                        Some(root.generation)
                    } else {
                        ownership_generation
                    };
                    let expected_projection = if is_owner {
                        "live_sandboxed"
                    } else {
                        "historical_transferred"
                    };
                    let projection_matches = expected_generation
                        .map(|generation| {
                            store.startup_session_projection_matches(
                                session.id,
                                custody_id,
                                generation,
                                expected_projection,
                            )
                        })
                        .transpose()?
                        .unwrap_or(false);
                    live_tuple
                        && matches_static_identity
                        && (is_owner || !executable)
                        && expected_generation == ownership_generation
                        && projection_matches
                } else {
                    let cleanup_compatible = match root.state.as_str() {
                        "purged" => {
                            session.sandbox_cleanup_state == Some(SandboxCleanupState::Purged)
                        }
                        "failed" => {
                            session.sandbox_cleanup_state == Some(SandboxCleanupState::Failed)
                        }
                        // Quarantine preserves its preexisting terminal
                        // history. A previously Purged tombstone remains
                        // rootless Purged; a cleanup failure remains Failed.
                        "quarantined" => matches!(
                            session.sandbox_cleanup_state,
                            Some(SandboxCleanupState::Purged | SandboxCleanupState::Failed)
                        ),
                        _ => false,
                    };
                    !executable
                        && matches_static_identity
                        && cleanup_compatible
                        && ((session.sandbox_root.is_none() && session.sandbox_branch.is_none())
                            || (claims_root
                                && session.sandbox_branch.as_deref()
                                    == Some(root.sandbox_branch.as_str())))
                };
                if !linked || ownership_generation.is_none() || !state_compatible {
                    conflict = true;
                }
            }
            after = Some(last);
        }
        if root.state == "live" && (!owner_seen || root.owner_session_id.is_none()) {
            conflict = true;
        }
        if conflict {
            // Re-page rather than retaining an unbounded participant list.
            let mut after = None;
            loop {
                let page =
                    store.startup_custody_group_sessions_page(group, after, PARTICIPANT_PAGE)?;
                let Some(last) = page.last().map(|session| session.id) else {
                    break;
                };
                for session in page {
                    store.invalidate_startup_session(
                        session.id,
                        SandboxCustodyErrorCodeV1::OwnershipConflict,
                    )?;
                }
                after = Some(last);
            }
            if root.state == "live" && root.owner_session_id.is_some() {
                store.record_failed_revalidation(
                    custody_id,
                    root.generation,
                    SandboxCustodyErrorCodeV1::OwnershipConflict,
                    SandboxCustodyTransitionV1::StartupReconciliation,
                )?;
            } else {
                store.quarantine_startup_terminal_root(
                    custody_id,
                    SandboxCustodyErrorCodeV1::OwnershipConflict,
                )?;
            }
            return Ok(());
        }

        if root.state != "live" {
            return Self::authenticate_startup_terminal_root(store, sandbox_base, &root, listings);
        }
        let owner_id = root.owner_session_id.expect("checked live owner");
        let owner = store.get_session(owner_id)?.ok_or_else(|| {
            DaemonError::Store("live startup custody root owner is missing".into())
        })?;
        let authorization = match startup_settlement_failure_for_test() {
            Some(error) => Err(error),
            None => Self::authorize_live_with_listings(
                &owner,
                store,
                sandbox_base,
                SandboxCustodyTransitionV1::StartupReconciliation,
                listings,
                None,
            ),
        };
        match authorization {
            Ok(CustodyHandle::Sandboxed(sandboxed)) => store.publish_startup_live_verification(
                owner.id,
                sandboxed.custody_id,
                sandboxed.owner_generation,
            ),
            Ok(CustodyHandle::Ordinary(_)) => Err(DaemonError::Store(
                "startup custody classifier returned an ordinary live handle".into(),
            )),
            Err(error) => {
                let Some(code) = startup_error_code(&error) else {
                    return Err(error);
                };
                store.invalidate_startup_session(owner.id, code)
            }
        }
    }

    fn reconcile_startup_rootless_session(store: &mut Store, session: Session) -> Result<()> {
        if matches!(
            session.sandbox_cleanup_state,
            Some(SandboxCleanupState::Purged)
        ) && matches!(session.sandbox_kind, Some(SandboxKind::GitWorktree))
            && session.sandbox_branch.is_none()
        {
            return store.publish_startup_rootless_purged(session.id);
        }
        if matches!(
            session.sandbox_cleanup_state,
            Some(SandboxCleanupState::Failed)
        ) && matches!(session.sandbox_kind, Some(SandboxKind::GitWorktree))
            && session.sandbox_root.is_none()
            && session.sandbox_branch.is_none()
        {
            return store.publish_startup_rootless_failed(session.id);
        }
        match Self::classify(&session) {
            Ok(CustodyClassification::OrdinaryUnsandboxed) => {
                store.publish_startup_ordinary(session.id)
            }
            Ok(CustodyClassification::RequiresPersistedAuthentication) => Err(DaemonError::Store(
                "rootless startup group cannot require live authentication".into(),
            )),
            Err(error) => {
                let Some(code) = startup_error_code(&error) else {
                    return Err(error);
                };
                store.invalidate_startup_session(session.id, code)
            }
        }
    }

    fn reconcile_startup_root_group(
        store: &mut Store,
        sandbox_base: &Path,
        sandbox_root: &str,
        sessions: Vec<Session>,
        listings: &mut WorktreeListings,
    ) -> Result<()> {
        if let Some(root) = store.startup_custody_root_for_sandbox_root(sandbox_root)? {
            if root.state != "live" {
                return Self::authenticate_startup_terminal_root(
                    store,
                    sandbox_base,
                    &root,
                    listings,
                );
            }
            let Some(owner_id) = root.owner_session_id else {
                // A malformed live root with no owner has no capability to
                // revalidate. Every attached executable session is fenced.
                for session in sessions {
                    store.invalidate_startup_session(
                        session.id,
                        SandboxCustodyErrorCodeV1::OwnershipMissing,
                    )?;
                }
                return Ok(());
            };
            let owner = store.get_session(owner_id)?.ok_or_else(|| {
                DaemonError::Store("live startup custody root owner is missing".into())
            })?;
            let authorization = match startup_settlement_failure_for_test() {
                Some(error) => Err(error),
                None => Self::authorize_live_with_listings(
                    &owner,
                    store,
                    sandbox_base,
                    SandboxCustodyTransitionV1::StartupReconciliation,
                    listings,
                    None,
                ),
            };
            match authorization {
                Ok(CustodyHandle::Sandboxed(sandboxed)) => {
                    store.publish_startup_live_verification(
                        owner.id,
                        sandboxed.custody_id,
                        sandboxed.owner_generation,
                    )?;
                }
                Ok(CustodyHandle::Ordinary(_)) => {
                    return Err(DaemonError::Store(
                        "startup custody classifier returned an ordinary live handle".into(),
                    ));
                }
                Err(error) => {
                    // `authorize_live_locked` has atomically invalidated or
                    // quarantined the aggregate with StartupReconciliation
                    // provenance. This status settlement is compatible and
                    // cannot turn a terminal historical row into Failed.
                    let Some(code) = startup_error_code(&error) else {
                        return Err(error);
                    };
                    store.invalidate_startup_session(owner.id, code)?;
                }
            }
            return Ok(());
        }

        if Self::try_reconstruct_legacy_live_root(store, sandbox_base, sandbox_root, &sessions)? {
            return Ok(());
        }
        if Self::try_reconstruct_legacy_terminal_root(store, sandbox_base, sandbox_root, &sessions)?
        {
            return Ok(());
        }
        for session in sessions {
            // Terminal rows retain their status while gaining an invalid
            // ownerless projection; active rows are failed by this helper.
            store.invalidate_startup_session(
                session.id,
                SandboxCustodyErrorCodeV1::TupleIncomplete,
            )?;
        }
        Ok(())
    }

    /// A terminal aggregate is historical, but retained on-disk evidence is
    /// still security-sensitive. Authenticate it before publishing history.
    /// A genuinely missing root is compatible with completed terminal cleanup;
    /// a symlink (including dangling) is evidence substitution and quarantines.
    fn authenticate_startup_terminal_root(
        store: &mut Store,
        sandbox_base: &Path,
        root: &crate::store::sandbox_custody::StartupCustodyRoot,
        listings: &mut WorktreeListings,
    ) -> Result<()> {
        let root_path = Path::new(&root.sandbox_root);
        // Static identity is authenticated before any root probe. An ENOENT
        // can publish terminal history only for the immutable generation-one
        // allocation path, never for a fabricated missing path.
        let canonical_base = match std::fs::canonicalize(sandbox_base) {
            Ok(path) if path == sandbox_base => path,
            _ => {
                return store.quarantine_startup_terminal_root(
                    root.custody_id,
                    SandboxCustodyErrorCodeV1::RootOutsideBase,
                );
            }
        };
        let allocation: Option<String> = store
            .conn
            .query_row(
                "SELECT to_owner_session_id FROM sandbox_custody_events WHERE custody_id=?1 AND sequence=1 AND event_kind='allocated'",
                [root.custody_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(allocation) = allocation
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|error| DaemonError::Store(error.to_string()))?
        else {
            return store.quarantine_startup_terminal_root(
                root.custody_id,
                SandboxCustodyErrorCodeV1::OwnershipConflict,
            );
        };
        let expected_root = canonical_base.join(root.allocation_id.to_string());
        if root_path.parent() != Some(canonical_base.as_path()) {
            return store.quarantine_startup_terminal_root(
                root.custody_id,
                SandboxCustodyErrorCodeV1::RootOutsideBase,
            );
        }
        if root_path != expected_root {
            return store.quarantine_startup_terminal_root(
                root.custody_id,
                SandboxCustodyErrorCodeV1::RootIdentityMismatch,
            );
        }
        let session = match store.get_session(allocation)? {
            Some(session) => session,
            None => {
                return store.quarantine_startup_terminal_root(
                    root.custody_id,
                    SandboxCustodyErrorCodeV1::OwnershipMissing,
                );
            }
        };
        let canonical_repo = match std::fs::canonicalize(&session.working_dir) {
            Ok(path)
                if path == session.working_dir && path == Path::new(&root.canonical_repo_dir) =>
            {
                path
            }
            _ => {
                return store.quarantine_startup_terminal_root(
                    root.custody_id,
                    SandboxCustodyErrorCodeV1::WorktreeMismatch,
                );
            }
        };
        // A logical Purged tombstone intentionally nulls the mutable Session
        // root and branch. It may use that rootless tuple, but never to skip
        // authentication of immutable retained filesystem/Git evidence.
        let rootless_purged = root.state == "purged"
            && session.sandbox_root.is_none()
            && session.sandbox_branch.is_none()
            && session.sandbox_cleanup_state == Some(SandboxCleanupState::Purged);
        let tuple_matches = rootless_purged
            || (session.working_dir.to_string_lossy() == root.canonical_repo_dir
                && matches!(session.sandbox_kind, Some(SandboxKind::GitWorktree))
                && session.sandbox_root.as_deref().map(Path::new) == Some(root_path)
                && session.sandbox_branch.as_deref() == Some(root.sandbox_branch.as_str()));
        let inputs = TerminalProofInputs {
            root_path: root_path.to_path_buf(),
            canonical_repo,
            sandbox_branch: root.sandbox_branch.clone(),
            repository_identity: root.repository_identity.clone(),
            source_commit: root.source_commit.clone(),
        };
        let proof = match listings.take_prefetched_terminal_proof(root.custody_id, &inputs) {
            Some(proof) => proof,
            None => Self::probe_startup_terminal_root(&inputs),
        };
        match proof {
            TerminalRootProof::Absent => store.publish_startup_terminal_root(root.custody_id),
            TerminalRootProof::Refused(code) => {
                store.quarantine_startup_terminal_root(root.custody_id, code)
            }
            // The evidence probe is side-effect free, so evaluating it
            // regardless of `tuple_matches` yields the same decision.
            TerminalRootProof::Evidence { valid } if tuple_matches && valid => {
                store.publish_startup_terminal_root(root.custody_id)
            }
            TerminalRootProof::Evidence { .. } => store.quarantine_startup_terminal_root(
                root.custody_id,
                SandboxCustodyErrorCodeV1::WorktreeMismatch,
            ),
        }
    }

    /// The filesystem/Git half of terminal-root authentication: no Store
    /// access and no writes, so startup can run it concurrently (#961).
    fn probe_startup_terminal_root(inputs: &TerminalProofInputs) -> TerminalRootProof {
        let root_path = inputs.root_path.as_path();
        match std::fs::symlink_metadata(root_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return TerminalRootProof::Absent;
            }
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            _ => {
                return TerminalRootProof::Refused(SandboxCustodyErrorCodeV1::RootIdentityMismatch);
            }
        }
        let canonical_root = match std::fs::canonicalize(root_path) {
            Ok(path) if path == root_path => path,
            _ => {
                return TerminalRootProof::Refused(SandboxCustodyErrorCodeV1::RootIdentityMismatch);
            }
        };
        let canonical_repo = inputs.canonical_repo.as_path();
        let registered = git_output(canonical_repo, ["worktree", "list", "--porcelain"]);
        let common = git_output(
            &canonical_root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .and_then(|path| std::fs::canonicalize(path).ok());
        let root_head = git_output(&canonical_root, ["rev-parse", "HEAD"]);
        let valid = git_output(&canonical_root, ["rev-parse", "--show-toplevel"])
            .is_some_and(|top| PathBuf::from(top) == canonical_root)
            && git_output(&canonical_root, ["branch", "--show-current"]).as_deref()
                == Some(inputs.sandbox_branch.as_str())
            && registered.is_some_and(|listed| {
                root_head.as_deref().is_some_and(|head| {
                    has_matching_worktree_stanza_with_head(
                        &listed,
                        &canonical_root,
                        &inputs.sandbox_branch,
                        head,
                    )
                })
            })
            && common.is_some_and(|identity| {
                identity.to_string_lossy() == inputs.repository_identity
                    && std::fs::canonicalize(canonical_repo.join(".git"))
                        .is_ok_and(|repo_identity| repo_identity == identity)
            })
            && git_success(
                &canonical_root,
                ["merge-base", "--is-ancestor", &inputs.source_commit, "HEAD"],
            );
        TerminalRootProof::Evidence { valid }
    }

    fn try_reconstruct_legacy_live_root(
        store: &mut Store,
        sandbox_base: &Path,
        sandbox_root: &str,
        sessions: &[Session],
    ) -> Result<bool> {
        let cleanup_shape = sessions
            .first()
            .and_then(|session| session.sandbox_cleanup_state);
        if sessions.is_empty()
            || !matches!(cleanup_shape, None | Some(SandboxCleanupState::Live))
            || !sessions.iter().all(|session| {
                matches!(session.sandbox_kind, Some(SandboxKind::GitWorktree))
                    && session.sandbox_root.as_deref() == Some(Path::new(sandbox_root))
                    && session.sandbox_branch.is_some()
                    && matches!(
                        session.sandbox_cleanup_state,
                        None | Some(SandboxCleanupState::Live)
                    )
            })
            || !sessions
                .iter()
                .all(|session| session.sandbox_cleanup_state == cleanup_shape)
        {
            return Ok(false);
        }
        let root = PathBuf::from(sandbox_root);
        let allocation_session_id = root
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| Uuid::parse_str(name).ok());
        let Some(allocation_session_id) = allocation_session_id else {
            return Ok(false);
        };
        let Some(allocation) = sessions
            .iter()
            .find(|session| session.id == allocation_session_id)
        else {
            return Ok(false);
        };
        let canonical_root = match std::fs::canonicalize(&root) {
            Ok(path) => path,
            Err(_) => return Ok(false),
        };
        let canonical_base = match std::fs::canonicalize(sandbox_base) {
            Ok(path) => path,
            Err(_) => return Ok(false),
        };
        if root != canonical_root
            || sandbox_base != canonical_base
            || !canonical_root.starts_with(&canonical_base)
            || canonical_root.file_name().and_then(|name| name.to_str())
                != Some(allocation_session_id.to_string().as_str())
        {
            return Ok(false);
        }
        let canonical_repo = match std::fs::canonicalize(&allocation.working_dir) {
            Ok(path) => path,
            Err(_) => return Ok(false),
        };
        if allocation.working_dir != canonical_repo {
            return Ok(false);
        }
        let branch = allocation.sandbox_branch.as_deref().expect("checked above");
        if !sessions.iter().all(|session| {
            session.working_dir == allocation.working_dir
                && session.sandbox_branch.as_deref() == Some(branch)
                && session.sandbox_root.as_deref() == Some(Path::new(sandbox_root))
        }) || !git_output(&canonical_root, ["rev-parse", "--show-toplevel"])
            .is_some_and(|top| PathBuf::from(top) == canonical_root)
            || git_output(&canonical_root, ["branch", "--show-current"]).as_deref() != Some(branch)
        {
            return Ok(false);
        }
        let Some(common_dir) = git_output(
            &canonical_root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ) else {
            return Ok(false);
        };
        let Some(repository_identity) = std::fs::canonicalize(common_dir)
            .ok()
            .map(|path| path.to_string_lossy().to_string())
        else {
            return Ok(false);
        };
        let Some(registered) = git_output(&canonical_repo, ["worktree", "list", "--porcelain"])
        else {
            return Ok(false);
        };
        if !has_matching_worktree_stanza(&registered, &canonical_root, branch) {
            return Ok(false);
        }
        let Some(source_commit) = git_output(&canonical_root, ["rev-parse", "HEAD"]) else {
            return Ok(false);
        };
        let Some(lineage) = legacy_lineage(sessions, allocation_session_id) else {
            return Ok(false);
        };
        store.reconstruct_legacy_startup_root(LegacyStartupRoot {
            custody_id: Uuid::new_v4(),
            allocation_session_id,
            canonical_repo_dir: allocation.working_dir.to_string_lossy().to_string(),
            sandbox_root: sandbox_root.to_owned(),
            sandbox_branch: branch.to_owned(),
            repository_identity,
            source_commit,
            lineage,
        })?;
        Ok(true)
    }

    fn try_reconstruct_legacy_terminal_root(
        store: &mut Store,
        sandbox_base: &Path,
        sandbox_root: &str,
        sessions: &[Session],
    ) -> Result<bool> {
        let cleanup_shape = sessions
            .first()
            .and_then(|session| session.sandbox_cleanup_state);
        if sessions.is_empty()
            || !matches!(
                cleanup_shape,
                Some(SandboxCleanupState::Purged | SandboxCleanupState::Failed)
            )
            || !sessions.iter().all(|session| {
                matches!(session.sandbox_kind, Some(SandboxKind::GitWorktree))
                    && session.sandbox_root.as_deref() == Some(Path::new(sandbox_root))
                    && session.sandbox_branch.is_some()
                    && matches!(
                        session.sandbox_cleanup_state,
                        Some(SandboxCleanupState::Purged | SandboxCleanupState::Failed)
                    )
            })
            || !sessions
                .iter()
                .all(|session| session.sandbox_cleanup_state == cleanup_shape)
        {
            return Ok(false);
        }
        let root = Path::new(sandbox_root);
        let Some(allocation_session_id) = root
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| Uuid::parse_str(name).ok())
        else {
            return Ok(false);
        };
        if !root.is_absolute()
            || root.parent() != Some(sandbox_base)
            || root.file_name().and_then(|name| name.to_str())
                != Some(allocation_session_id.to_string().as_str())
        {
            return Ok(false);
        }
        // `Path::exists` follows links and reports a dangling symlink as
        // absent.  A dangling custody root is malformed retained evidence,
        // never safe historical absence.
        match std::fs::symlink_metadata(root) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            _ => return Ok(false),
        }
        let canonical_root = match std::fs::canonicalize(root) {
            Ok(path) if path == root => path,
            _ => return Ok(false),
        };
        let Some(allocation) = sessions
            .iter()
            .find(|session| session.id == allocation_session_id)
        else {
            return Ok(false);
        };
        let cleanup_state = allocation.sandbox_cleanup_state.expect("checked above");
        if !sessions.iter().all(|session| {
            session.working_dir == allocation.working_dir
                && session.sandbox_branch == allocation.sandbox_branch
                && session.sandbox_cleanup_state == Some(cleanup_state)
        }) {
            return Ok(false);
        }
        let canonical_repo = match std::fs::canonicalize(&allocation.working_dir) {
            Ok(path) => path,
            Err(_) => return Ok(false),
        };
        if allocation.working_dir != canonical_repo {
            return Ok(false);
        }
        let canonical_base = match std::fs::canonicalize(sandbox_base) {
            Ok(path) => path,
            Err(_) => return Ok(false),
        };
        let branch = allocation.sandbox_branch.as_deref().expect("checked above");
        if canonical_base != sandbox_base
            || !canonical_root.starts_with(&canonical_base)
            || !git_output(&canonical_root, ["rev-parse", "--show-toplevel"])
                .is_some_and(|top| PathBuf::from(top) == canonical_root)
            || git_output(&canonical_root, ["branch", "--show-current"]).as_deref() != Some(branch)
            || !git_output(&canonical_repo, ["worktree", "list", "--porcelain"]).is_some_and(
                |listed| has_matching_worktree_stanza(&listed, &canonical_root, branch),
            )
        {
            return Ok(false);
        }
        let Some(common_dir) = git_output(
            &canonical_root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ) else {
            return Ok(false);
        };
        if std::fs::canonicalize(common_dir).ok()
            != std::fs::canonicalize(canonical_repo.join(".git")).ok()
        {
            return Ok(false);
        }
        let Some(repository_identity) = std::fs::canonicalize(canonical_repo.join(".git"))
            .ok()
            .map(|path| path.to_string_lossy().to_string())
        else {
            return Ok(false);
        };
        // The retained worktree itself is the only source-authentic proof of
        // its allocation revision.  Do not substitute today's canonical HEAD
        // for missing historical evidence.
        let Some(source_commit) = git_output(&canonical_root, ["rev-parse", "HEAD"]) else {
            return Ok(false);
        };
        store.reconstruct_legacy_terminal_root(LegacyTerminalRoot {
            custody_id: Uuid::new_v4(),
            allocation_session_id,
            canonical_repo_dir: allocation.working_dir.to_string_lossy().to_string(),
            sandbox_root: sandbox_root.to_owned(),
            sandbox_branch: allocation.sandbox_branch.clone().expect("checked above"),
            repository_identity,
            source_commit,
            cleanup_state,
        })?;
        Ok(true)
    }

    fn log_retained_unowned_diagnostics(store: &Store, sandbox_base: &Path) -> Result<()> {
        let mut after = None;
        loop {
            let page = store.startup_custody_group_page(after.as_deref(), 64)?;
            let Some(last) = page.last().map(|group| group.key.clone()) else {
                break;
            };
            for group in page {
                if let Some(root) = group.sandbox_root
                    && store
                        .startup_custody_root_for_sandbox_root(&root)?
                        .is_none()
                {
                    retained_unowned_diagnostic(
                        Some(&root),
                        "sql_root_without_authoritative_aggregate",
                    );
                }
            }
            after = Some(last);
        }
        let Ok(entries) = std::fs::read_dir(sandbox_base) else {
            return Ok(());
        };
        // Filesystem iterators have no portable keyset cursor. Bound the
        // direct-child diagnostic fan-out and emit one stable overflow reason;
        // never recurse, canonicalize, follow, or mutate an entry.
        const ON_DISK_DIAGNOSTIC_CAP: usize = 256;
        let mut inspected = 0;
        for entry in entries {
            if inspected == ON_DISK_DIAGNOSTIC_CAP {
                retained_unowned_diagnostic(None, "base_directory_diagnostic_overflow");
                break;
            }
            // Count every direct entry before inspecting its type.  This is
            // deliberately the only bounded read_dir work; non-UUID names and
            // symlinks cannot create an unbounded pre-filter scan.
            inspected += 1;
            let Ok(entry) = entry else {
                retained_unowned_diagnostic(None, "base_directory_entry_error");
                continue;
            };
            let Ok(metadata) = entry.file_type() else {
                continue;
            };
            if metadata.is_symlink() || !metadata.is_dir() {
                continue;
            }
            let name = entry.file_name();
            if Uuid::parse_str(&name.to_string_lossy()).is_ok() {
                let root = entry.path().to_string_lossy().to_string();
                if store
                    .startup_custody_root_for_sandbox_root(&root)?
                    .is_none()
                {
                    retained_unowned_diagnostic(
                        Some(&root),
                        "base_directory_without_authoritative_aggregate",
                    );
                }
            }
        }
        Ok(())
    }

    /// Compatibility classifier for the already-converted continue path.
    pub fn classify(session: &Session) -> Result<CustodyClassification> {
        Self::classify_for_transition(session, SandboxCustodyTransitionV1::Continue)
    }

    /// Keep the transition chosen by an establishment caller through every
    /// tuple classification and refusal; never silently relabel handoff work
    /// as Continue merely because it reuses the compatibility classifier.
    pub(crate) fn classify_for_transition(
        session: &Session,
        transition: SandboxCustodyTransitionV1,
    ) -> Result<CustodyClassification> {
        let tuple_absent = session.sandbox_kind.is_none()
            && session.sandbox_root.is_none()
            && session.sandbox_branch.is_none()
            && session.sandbox_cleanup_state.is_none();
        if tuple_absent {
            return Ok(CustodyClassification::OrdinaryUnsandboxed);
        }

        let tuple_complete_live = matches!(session.sandbox_kind, Some(SandboxKind::GitWorktree))
            && session.sandbox_root.is_some()
            && session.sandbox_branch.is_some()
            && matches!(
                session.sandbox_cleanup_state,
                Some(SandboxCleanupState::Live)
            );
        if tuple_complete_live {
            return Ok(CustodyClassification::RequiresPersistedAuthentication);
        }

        let code = match session.sandbox_cleanup_state {
            Some(SandboxCleanupState::Purged) => SandboxCustodyErrorCodeV1::HistoricalPurged,
            Some(SandboxCleanupState::Failed) => SandboxCustodyErrorCodeV1::CleanupFailed,
            _ => SandboxCustodyErrorCodeV1::TupleIncomplete,
        };
        Err(Self::refusal(code, Some(session.id), transition))
    }

    pub fn authorize_ordinary(session: &Session) -> Result<CustodyHandle> {
        Self::authorize_ordinary_for_transition(session, SandboxCustodyTransitionV1::Continue)
    }

    pub(crate) fn authorize_ordinary_for_transition(
        session: &Session,
        transition: SandboxCustodyTransitionV1,
    ) -> Result<CustodyHandle> {
        match Self::classify_for_transition(session, transition)? {
            CustodyClassification::OrdinaryUnsandboxed => {
                Ok(CustodyHandle::Ordinary(OrdinaryCustody {
                    canonical_working_dir: session.working_dir.clone(),
                }))
            }
            CustodyClassification::RequiresPersistedAuthentication => Err(Self::refusal(
                SandboxCustodyErrorCodeV1::OwnershipMissing,
                Some(session.id),
                transition,
            )),
        }
    }

    pub(crate) fn begin_ordinary_effect(
        handle: &CustodyHandle,
        kind: EffectKind,
    ) -> Result<CustodyEffectPermit> {
        match handle {
            CustodyHandle::Ordinary(ordinary) => Ok(CustodyEffectPermit {
                effective_cwd: ordinary.canonical_working_dir.clone(),
                cargo_target_dir: None,
                kind,
                settlement: None,
            }),
            CustodyHandle::Sandboxed(_) => Err(Self::refusal(
                SandboxCustodyErrorCodeV1::OwnershipMissing,
                None,
                SandboxCustodyTransitionV1::EffectRevalidation,
            )),
        }
    }

    /// Authenticate the persisted aggregate and then independently prove the
    /// live filesystem/git identity. This is useful for preflight, but callers
    /// must still use [`begin_effect`] immediately before an effect: its
    /// revalidation is intentionally not reusable as a location capability.
    pub fn authorize_live(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
    ) -> Result<CustodyHandle> {
        Self::authorize_live_holding(session, store, sandbox_base, transition, None)
    }

    /// [`Self::authorize_live`] that never waits for the stripe: a busy stripe
    /// returns the typed retryable `root_busy` at once. For callers that hold
    /// the Store on the async path and cannot take the stripe Store-first (they
    /// await more state while holding the Store): they drop everything, back
    /// off asynchronously and retry, so a maintenance proof on a colliding
    /// stripe delays only them and never the Store (#1166).
    pub fn authorize_live_try(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
    ) -> Result<CustodyHandle> {
        // An unknown custody refuses inside `authorize_live_holding` before any
        // stripe is taken, exactly as `authorize_live` does.
        let Ok(custody) = store.live_custody_for_session(session.id) else {
            return Self::authorize_live_holding(session, store, sandbox_base, transition, None);
        };
        let root = crate::store::sandbox_custody::try_lock_custody_root(custody.custody_id)
            .ok_or_else(crate::store::custody_lock_order::lock_order_busy_error)?;
        Self::authorize_live_holding(session, store, sandbox_base, transition, Some(root))
    }

    /// [`authorize_live`] for a caller that already holds this session's
    /// custody stripe from `lock_store_then_session_root` (Store first, stripe
    /// acquired without waiting). `None` takes the stripe here, which may wait
    /// for it while the caller holds the Store: only use it where no
    /// maintenance proof can hold the same stripe (tests, startup).
    pub fn authorize_live_holding(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
        held: Option<CustodyRootGuard>,
    ) -> Result<CustodyHandle> {
        Self::authorize_live_with_listings(
            session,
            store,
            sandbox_base,
            transition,
            &mut WorktreeListings::Fresh,
            held,
        )
    }

    fn authorize_live_with_listings(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
        listings: &mut WorktreeListings,
        held: Option<CustodyRootGuard>,
    ) -> Result<CustodyHandle> {
        Self::classify_for_transition(session, transition)?;
        let custody_id = match store.live_custody_for_session(session.id) {
            Ok(custody) => custody.custody_id,
            Err(_) => {
                // Startup may have already proven that a legacy cached tuple
                // is non-executable. Preserve that durable diagnosis instead
                // of relabeling it as missing ownership, while remaining
                // fail-closed and never deriving custody from the tuple.
                let code = match store.unlinked_invalid_custody_error(session.id) {
                    Ok(Some(code))
                        if code == SandboxCustodyErrorCodeV1::TupleIncomplete.as_str() =>
                    {
                        SandboxCustodyErrorCodeV1::TupleIncomplete
                    }
                    _ => SandboxCustodyErrorCodeV1::OwnershipMissing,
                };
                return Err(Self::refusal(code, Some(session.id), transition));
            }
        };
        let _root_guard = held.unwrap_or_else(|| lock_custody_root(custody_id));
        Self::authorize_live_locked_with_listings(
            session,
            store,
            sandbox_base,
            transition,
            true,
            listings,
        )
    }

    /// Retry preauthentication/bind validation must be observational: a
    /// malformed or raced source is refused without rewriting its owner,
    /// generation, immutable events, or projection.
    fn authorize_live_nonmutating(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
    ) -> Result<CustodyHandle> {
        Self::authorize_live_nonmutating_holding(session, store, sandbox_base, transition, None)
    }

    /// [`authorize_live_nonmutating`] with the stripe already held (see
    /// [`authorize_live_holding`]).
    fn authorize_live_nonmutating_holding(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
        held: Option<CustodyRootGuard>,
    ) -> Result<CustodyHandle> {
        Self::classify_for_transition(session, transition)?;
        let custody_id = store
            .live_custody_for_session(session.id)
            .map_err(|_| {
                Self::refusal(
                    SandboxCustodyErrorCodeV1::OwnershipMissing,
                    Some(session.id),
                    transition,
                )
            })?
            .custody_id;
        let _root_guard = held.unwrap_or_else(|| lock_custody_root(custody_id));
        Self::authorize_live_locked_with_policy(session, store, sandbox_base, transition, false)
    }

    fn authorize_live_locked_with_policy(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
        record_failure: bool,
    ) -> Result<CustodyHandle> {
        Self::authorize_live_locked_with_listings(
            session,
            store,
            sandbox_base,
            transition,
            record_failure,
            &mut WorktreeListings::Fresh,
        )
    }

    fn authorize_live_locked_with_listings(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
        record_failure: bool,
        listings: &mut WorktreeListings,
    ) -> Result<CustodyHandle> {
        Self::classify_for_transition(session, transition)?;
        let persisted = store.live_custody_for_session(session.id).map_err(|_| {
            Self::refusal(
                SandboxCustodyErrorCodeV1::CustodyChanged,
                Some(session.id),
                transition,
            )
        })?;
        if store.startup_custody_has_settlement_fence(persisted.custody_id)? {
            return Err(DaemonError::PolicyDenied(
                "source-worktree settlement journal fences this sandbox execution".into(),
            ));
        }
        let failure = |code| {
            if record_failure {
                match store.record_failed_revalidation_locked(
                    persisted.custody_id,
                    persisted.generation,
                    code,
                    transition,
                ) {
                    Ok(()) => Self::refusal(code, Some(session.id), transition),
                    Err(error) => error,
                }
            } else {
                Self::refusal(code, Some(session.id), transition)
            }
        };
        Self::authenticate_live_filesystem(session, &persisted, sandbox_base, listings, failure)
    }

    /// Compute every live root's filesystem/Git proof, and every retained
    /// terminal root's evidence probe, concurrently before the serial startup
    /// pass (#961). Each probe is a pure function of its inputs, the sandbox
    /// base, the filesystem and the pass-scoped listing snapshot; it reads no
    /// Store and writes nothing. Startup runs before any provider or sandbox
    /// mutation is restored, so an early probe is the result the serial pass
    /// would compute. The serial pass is unchanged: it applies each result
    /// in its own order through the existing publication, revalidation
    /// failure, quarantine and invalidation paths, and re-probes any root
    /// whose inputs differ from the prefetched ones.
    fn prefetch_startup_live_proofs(
        store: &Store,
        sandbox_base: &Path,
        pool: usize,
    ) -> Result<WorktreeListings> {
        enum Job {
            Terminal(Uuid, TerminalProofInputs),
            Live(
                Box<Session>,
                crate::store::sandbox_custody::PersistedCustody,
            ),
        }
        enum Proof {
            Terminal(TerminalRootProof, Option<WorktreeFingerprint>),
            Live(
                std::result::Result<(), SandboxCustodyErrorCodeV1>,
                Option<WorktreeFingerprint>,
            ),
        }
        if pool <= 1 {
            return Ok(WorktreeListings::startup_snapshot());
        }
        let started = std::time::Instant::now();
        // Terminal probes are the longest (a fresh listing and five Git
        // calls each), so they are claimed first.
        let mut jobs: Vec<Job> = store
            .startup_terminal_root_probe_inputs()?
            .into_iter()
            .filter(|(_, inputs)| {
                std::fs::symlink_metadata(&inputs.root_path).is_ok_and(|metadata| metadata.is_dir())
            })
            .map(|(custody_id, inputs)| Job::Terminal(custody_id, inputs))
            .collect();
        for owner_id in store.startup_live_root_owner_ids()? {
            let Some(owner) = store.get_session(owner_id)? else {
                continue;
            };
            if Self::classify_for_transition(
                &owner,
                SandboxCustodyTransitionV1::StartupReconciliation,
            )
            .is_err()
            {
                continue;
            }
            let Ok(persisted) = store.live_custody_for_session(owner_id) else {
                continue;
            };
            jobs.push(Job::Live(Box::new(owner), persisted));
        }
        // One listing per repository, keyed exactly as the live probe keys it.
        let repositories: Vec<PathBuf> = jobs
            .iter()
            .filter_map(|job| match job {
                Job::Live(owner, _) => std::fs::canonicalize(&owner.working_dir).ok(),
                Job::Terminal(..) => None,
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let listed = scoped_pool_map(
            &repositories,
            pool,
            || (),
            |(), _, repo| git_output(repo, ["worktree", "list", "--porcelain"]),
        );
        let listings: HashMap<PathBuf, Option<String>> =
            repositories.into_iter().zip(listed).collect();
        let results = scoped_pool_map(
            &jobs,
            pool,
            || WorktreeListings::StartupSnapshot(listings.clone()),
            |local, index, job| {
                // The worktree fingerprint is read before and after the
                // probe; a change during the probe discards the result.
                let (custody_id, proof) = match job {
                    Job::Terminal(custody_id, inputs) => {
                        let before = WorktreeFingerprint::observe(
                            &inputs.root_path,
                            &inputs.canonical_repo,
                            None,
                        );
                        let proof = Self::probe_startup_terminal_root(inputs);
                        let after = WorktreeFingerprint::observe(
                            &inputs.root_path,
                            &inputs.canonical_repo,
                            None,
                        );
                        (
                            *custody_id,
                            Proof::Terminal(proof, (before == after).then_some(after)),
                        )
                    }
                    Job::Live(owner, persisted) => {
                        let observe = || {
                            owner.sandbox_root.as_deref().map(|root| {
                                WorktreeFingerprint::observe(
                                    root,
                                    &owner.working_dir,
                                    Some(sandbox_base),
                                )
                            })
                        };
                        let before = observe();
                        let proof =
                            Self::probe_live_filesystem(owner, persisted, sandbox_base, local);
                        let after = observe();
                        (
                            persisted.custody_id,
                            Proof::Live(proof, if before == after { after } else { None }),
                        )
                    }
                };
                startup_probe_completed_for_test(sandbox_base, index, jobs.len(), custody_id);
                proof
            },
        );
        // Only admissions are kept, and only with a stable, fully readable
        // fingerprint: every refusal is re-probed by the serial pass itself.
        let mut proofs = HashMap::new();
        let mut terminal_proofs = HashMap::new();
        for (job, result) in jobs.into_iter().zip(results) {
            match (job, result) {
                (Job::Live(owner, persisted), Proof::Live(Ok(()), Some(fingerprint)))
                    if fingerprint.is_readable() =>
                {
                    proofs.insert(
                        persisted.custody_id,
                        PrefetchedLiveProof {
                            inputs: LiveProofInputs::of(&owner, &persisted),
                            fingerprint,
                        },
                    );
                }
                (Job::Terminal(custody_id, inputs), Proof::Terminal(proof, Some(fingerprint)))
                    if proof.admits() && fingerprint.is_readable() =>
                {
                    terminal_proofs.insert(
                        custody_id,
                        PrefetchedTerminalProof {
                            inputs,
                            fingerprint,
                            proof,
                        },
                    );
                }
                (Job::Live(..), Proof::Live(..)) | (Job::Terminal(..), Proof::Terminal(..)) => {}
                _ => unreachable!("each job yields its own proof kind"),
            }
        }
        tracing::info!(
            live_roots = proofs.len(),
            terminal_roots = terminal_proofs.len(),
            pool,
            duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            "Prefetched startup custody proofs"
        );
        Ok(WorktreeListings::StartupPrefetched(Box::new(
            StartupPrefetch {
                listings,
                proofs,
                terminal_proofs,
            },
        )))
    }

    fn authenticate_live_filesystem(
        session: &Session,
        persisted: &crate::store::sandbox_custody::PersistedCustody,
        sandbox_base: &Path,
        listings: &mut WorktreeListings,
        mut failure: impl FnMut(SandboxCustodyErrorCodeV1) -> DaemonError,
    ) -> Result<CustodyHandle> {
        // A startup proof computed concurrently for exactly these inputs is
        // the same deterministic result this probe would return (#961).
        let proof = match listings.take_prefetched_proof(session, persisted, sandbox_base) {
            Some(proof) => proof,
            None => Self::probe_live_filesystem(session, persisted, sandbox_base, listings),
        };
        match proof {
            Ok(()) => Ok(CustodyHandle::Sandboxed(SandboxedCustody {
                session_id: session.id,
                custody_id: persisted.custody_id,
                owner_generation: persisted.generation,
            })),
            Err(code) => Err(failure(code)),
        }
    }

    /// The filesystem/Git half of live authentication: no Store access and no
    /// writes, so startup can run it concurrently. Returns the first refusal.
    fn probe_live_filesystem(
        session: &Session,
        persisted: &crate::store::sandbox_custody::PersistedCustody,
        sandbox_base: &Path,
        listings: &mut WorktreeListings,
    ) -> std::result::Result<(), SandboxCustodyErrorCodeV1> {
        let Some(session_root) = session.sandbox_root.as_ref() else {
            return Err(SandboxCustodyErrorCodeV1::TupleIncomplete);
        };
        let Some(session_branch) = session.sandbox_branch.as_deref() else {
            return Err(SandboxCustodyErrorCodeV1::TupleIncomplete);
        };
        if persisted.owner_session_id != session.id
            || persisted.canonical_repo_dir != session.working_dir.to_string_lossy()
            || persisted.sandbox_root != session_root.to_string_lossy()
            || persisted.sandbox_branch != session_branch
        {
            return Err(SandboxCustodyErrorCodeV1::RootIdentityMismatch);
        }
        let base = std::fs::canonicalize(sandbox_base)
            .map_err(|_| SandboxCustodyErrorCodeV1::RootOutsideBase)?;
        let root = std::fs::canonicalize(session_root)
            .map_err(|_| SandboxCustodyErrorCodeV1::RootMissing)?;
        let allocation_root_name = persisted.allocation_id.to_string();
        if !root.starts_with(&base)
            || root.file_name().and_then(|name| name.to_str())
                != Some(allocation_root_name.as_str())
            || root != *session_root
            || persisted.sandbox_root != root.to_string_lossy()
        {
            return Err(SandboxCustodyErrorCodeV1::RootOutsideBase);
        }
        let canonical = std::fs::canonicalize(&session.working_dir)
            .map_err(|_| SandboxCustodyErrorCodeV1::RootMissing)?;
        if canonical != session.working_dir
            || persisted.canonical_repo_dir != canonical.to_string_lossy()
        {
            return Err(SandboxCustodyErrorCodeV1::RootIdentityMismatch);
        }
        // One combined rev-parse only ever admits: its values are the ones
        // the three separate calls print, so a match here is a match there.
        // Anything else re-runs the separate calls, which decide every
        // refusal exactly as before (#961).
        let common_dir = match git_worktree_identity(&root).filter(|identity| {
            PathBuf::from(&identity.toplevel) == root
                && identity.head.strip_prefix("refs/heads/") == Some(session_branch)
        }) {
            Some(identity) => identity.common_dir,
            None => {
                if !git_output(&root, ["rev-parse", "--show-toplevel"])
                    .is_some_and(|top| PathBuf::from(top) == root)
                    || git_output(&root, ["branch", "--show-current"]).as_deref()
                        != Some(session_branch)
                {
                    return Err(SandboxCustodyErrorCodeV1::WorktreeMismatch);
                }
                let Some(common_dir) = git_output(
                    &root,
                    ["rev-parse", "--path-format=absolute", "--git-common-dir"],
                ) else {
                    return Err(SandboxCustodyErrorCodeV1::WorktreeMismatch);
                };
                common_dir
            }
        };
        if std::fs::canonicalize(common_dir)
            .ok()
            .map(|path| path.to_string_lossy().to_string())
            != Some(persisted.repository_identity.clone())
        {
            return Err(SandboxCustodyErrorCodeV1::WorktreeMismatch);
        }
        if !git_is_ancestor(&root, &persisted.source_commit) {
            return Err(SandboxCustodyErrorCodeV1::SourceRevisionUnavailable);
        }
        if listings.registers(&canonical, &root, session_branch) != Some(true) {
            return Err(SandboxCustodyErrorCodeV1::WorktreeMismatch);
        }
        Ok(())
    }

    /// Reserve then activate a persisted permit immediately before one effect.
    /// A preflight handle is deliberately insufficient: this repeats every
    /// persisted and filesystem/git check under the root-local admission lock.
    pub async fn begin_effect(
        store_handle: &CustodyStore,
        settlement_service: &CustodySettlementService,
        handle: &CustodyHandle,
        session: &Session,
        sandbox_base: &Path,
        boot_id: Uuid,
        kind: EffectKind,
    ) -> Result<CustodyEffectPermit> {
        Self::begin_effect_for_transition(
            store_handle,
            settlement_service,
            handle,
            session,
            sandbox_base,
            boot_id,
            kind,
            SandboxCustodyTransitionV1::EffectRevalidation,
        )
        .await
    }

    pub(crate) async fn begin_effect_for_transition(
        store_handle: &CustodyStore,
        settlement_service: &CustodySettlementService,
        handle: &CustodyHandle,
        session: &Session,
        sandbox_base: &Path,
        boot_id: Uuid,
        kind: EffectKind,
        transition: SandboxCustodyTransitionV1,
    ) -> Result<CustodyEffectPermit> {
        Self::begin_effect_for_transition_with_policy(
            store_handle,
            settlement_service,
            handle,
            session,
            sandbox_base,
            boot_id,
            kind,
            transition,
            true,
        )
        .await
    }

    async fn begin_effect_for_transition_nonmutating(
        store_handle: &CustodyStore,
        settlement_service: &CustodySettlementService,
        handle: &CustodyHandle,
        session: &Session,
        sandbox_base: &Path,
        boot_id: Uuid,
        kind: EffectKind,
        transition: SandboxCustodyTransitionV1,
    ) -> Result<CustodyEffectPermit> {
        Self::begin_effect_for_transition_with_policy(
            store_handle,
            settlement_service,
            handle,
            session,
            sandbox_base,
            boot_id,
            kind,
            transition,
            false,
        )
        .await
    }

    async fn begin_effect_for_transition_with_policy(
        store_handle: &CustodyStore,
        settlement_service: &CustodySettlementService,
        handle: &CustodyHandle,
        session: &Session,
        sandbox_base: &Path,
        boot_id: Uuid,
        kind: EffectKind,
        transition: SandboxCustodyTransitionV1,
        record_failure: bool,
    ) -> Result<CustodyEffectPermit> {
        match handle {
            CustodyHandle::Ordinary(_) => Self::begin_ordinary_effect(handle, kind),
            CustodyHandle::Sandboxed(sandboxed) => {
                // Reserve a bounded release slot before touching durable
                // counters. This makes a later Drop queue offer infallible
                // under normal service operation.
                let (capacity, handoff) = settlement_service.reserve()?;
                if session.id != sandboxed.session_id {
                    return Err(Self::refusal(
                        SandboxCustodyErrorCodeV1::CustodyChanged,
                        Some(sandboxed.session_id),
                        transition,
                    ));
                }
                let observed = {
                    let store = store_handle.lock().await;
                    let observed = store.live_custody_for_session(session.id).map_err(|_| {
                        Self::refusal(
                            SandboxCustodyErrorCodeV1::CustodyChanged,
                            Some(session.id),
                            transition,
                        )
                    })?;
                    if store.startup_custody_has_settlement_fence(observed.custody_id)? {
                        return Err(DaemonError::PolicyDenied(
                            "source-worktree settlement journal fences this sandbox execution"
                                .into(),
                        ));
                    }
                    observed
                };
                let mut failure_code = None;
                let revalidated = Self::authenticate_live_filesystem(
                    session,
                    &observed,
                    sandbox_base,
                    &mut WorktreeListings::Fresh,
                    |code| {
                        failure_code = Some(code);
                        Self::refusal(code, Some(session.id), transition)
                    },
                );
                let revalidated = match revalidated {
                    Ok(revalidated) => revalidated,
                    Err(error) => {
                        if record_failure {
                            if let Some(code) = failure_code {
                                let mut store = store_handle.lock().await;
                                if let Some(_root_guard) =
                                    crate::store::sandbox_custody::try_lock_custody_root(
                                        observed.custody_id,
                                    )
                                {
                                    store.record_failed_revalidation_locked(
                                        observed.custody_id,
                                        observed.generation,
                                        code,
                                        transition,
                                    )?;
                                }
                            }
                        }
                        return Err(error);
                    }
                };
                let CustodyHandle::Sandboxed(current) = revalidated else {
                    unreachable!("sandbox handle cannot revalidate as ordinary")
                };
                if current.custody_id != sandboxed.custody_id
                    || current.owner_generation != sandboxed.owner_generation
                {
                    return Err(Self::refusal(
                        SandboxCustodyErrorCodeV1::CustodyChanged,
                        Some(sandboxed.session_id),
                        transition,
                    ));
                }
                // The stripe is one of 64 shared by every custody, so a busy
                // stripe is usually an unrelated root's proof. Wait for it to
                // quiesce without holding the Store (the lock-order contract)
                // for a bounded budget; a longer hold surrenders with the typed
                // retryable `root_busy` (#1157) so a scheduled resume backs
                // off instead of stalling the scheduler. `latest != observed`
                // below rechecks every persisted fact after the wait.
                let Some((mut store, _root_guard)) = lock_store_then_root_within(
                    store_handle,
                    sandboxed.custody_id,
                    admission_wait(),
                )
                .await
                else {
                    return Err(Self::refusal(
                        SandboxCustodyErrorCodeV1::RootBusy,
                        Some(current.session_id),
                        transition,
                    ));
                };
                let latest = store.live_custody_for_session(session.id).map_err(|_| {
                    Self::refusal(
                        SandboxCustodyErrorCodeV1::CustodyChanged,
                        Some(current.session_id),
                        transition,
                    )
                })?;
                if latest != observed
                    || store.startup_custody_has_settlement_fence(latest.custody_id)?
                {
                    return Err(Self::refusal(
                        SandboxCustodyErrorCodeV1::CustodyChanged,
                        Some(current.session_id),
                        transition,
                    ));
                }
                let reservation = store
                    .reserve_effect_locked(current.custody_id, current.owner_generation, boot_id)
                    .map_err(|error| {
                        if matches!(&error, DaemonError::StructuredRpc { message, .. } if message == "sandbox_custody:reclaim_prepared") {
                            Self::refusal(
                                SandboxCustodyErrorCodeV1::ReclaimPrepared,
                                Some(current.session_id),
                                transition,
                            )
                        } else {
                            Self::refusal(
                                SandboxCustodyErrorCodeV1::CustodyChanged,
                                Some(current.session_id),
                                transition,
                            )
                        }
                    })?;
                let effective_cwd = session
                    .sandbox_root
                    .as_ref()
                    .expect("validated root")
                    .clone();
                if let Err(error) = store.settle_effect_locked(reservation, true) {
                    let _ = store.settle_effect_locked(reservation, false);
                    return Err(error);
                }
                Ok(CustodyEffectPermit {
                    effective_cwd,
                    cargo_target_dir: Some(
                        session
                            .sandbox_root
                            .as_ref()
                            .expect("validated root")
                            .join("target"),
                    ),
                    kind,
                    settlement: Some(PendingSettlement {
                        handoff,
                        settlement: EffectSettlement {
                            reservation,
                            _capacity: capacity,
                        },
                    }),
                })
            }
        }
    }

    /// Reclaim only `sandbox_root/target` while holding the same custody-root
    /// stripe used by transfer and effect admission.  A stale generation,
    /// active permit, altered owner/status, missing target, or symlink target
    /// refuses removal; the worktree, branch, and root are never removed.
    pub fn reclaim_terminal_target(
        store: &Store,
        custody_id: Uuid,
        expected_generation: u64,
        sandbox_base: &Path,
    ) -> Result<bool> {
        Ok(Self::reclaim_terminal_target_bytes(
            store,
            custody_id,
            expected_generation,
            sandbox_base,
        )?
        .is_some())
    }

    /// Same fenced reclaim operation, preserving the existing byte-count
    /// accounting without exposing a path outside the authorization fence.
    pub(crate) fn reclaim_terminal_target_bytes(
        store: &Store,
        custody_id: Uuid,
        expected_generation: u64,
        sandbox_base: &Path,
    ) -> Result<Option<u64>> {
        let _root_guard = lock_custody_root(custody_id);
        Self::reclaim_terminal_target_bytes_locked(
            store,
            custody_id,
            expected_generation,
            sandbox_base,
        )
    }

    /// Caller holds the custody root stripe. This permits the production
    /// sweep to fence its active-map reauthentication through the same lock
    /// as SQL selection and filesystem removal.
    pub fn reclaim_terminal_target_bytes_locked(
        store: &Store,
        custody_id: Uuid,
        expected_generation: u64,
        sandbox_base: &Path,
    ) -> Result<Option<u64>> {
        let outcome = Self::terminal_target_outcome_locked(
            store,
            custody_id,
            expected_generation,
            sandbox_base,
            true,
        )?;
        Self::compatibility_target_bytes(outcome)
    }

    /// Authenticate and size a terminal target without deleting it. Dry-run
    /// reporting deliberately shares every SQL, custody, Git, containment,
    /// symlink, and generation check with the mutating operation.
    pub(crate) fn inspect_terminal_target_bytes_locked(
        store: &Store,
        custody_id: Uuid,
        expected_generation: u64,
        sandbox_base: &Path,
    ) -> Result<Option<u64>> {
        let outcome = Self::terminal_target_outcome_locked(
            store,
            custody_id,
            expected_generation,
            sandbox_base,
            false,
        )?;
        Self::compatibility_target_bytes(outcome)
    }

    pub fn reclaim_terminal_target_outcome_phased(
        store: CustodyStore,
        pass: Arc<crate::sandbox::target_reclaim::ReclaimPassState>,
        active_owner: &dyn Fn() -> std::result::Result<bool, ReclaimSkipReason>,
        custody_id: Uuid,
        expected_generation: u64,
        sandbox_base: &Path,
        remove: bool,
    ) -> Result<TargetReclaimOutcome> {
        Self::terminal_target_outcome_with_store(
            &mut PhasedReclaimStore { store, pass },
            Some(active_owner),
            custody_id,
            expected_generation,
            sandbox_base,
            remove,
        )
    }

    fn terminal_target_outcome_locked(
        mut store: &Store,
        custody_id: Uuid,
        expected_generation: u64,
        sandbox_base: &Path,
        remove: bool,
    ) -> Result<TargetReclaimOutcome> {
        Self::terminal_target_outcome_with_store(
            &mut store,
            None,
            custody_id,
            expected_generation,
            sandbox_base,
            remove,
        )
    }

    fn terminal_target_outcome_with_store(
        access: &mut impl ReclaimStoreAccess,
        active_owner: Option<&dyn Fn() -> std::result::Result<bool, ReclaimSkipReason>>,
        custody_id: Uuid,
        expected_generation: u64,
        sandbox_base: &Path,
        remove: bool,
    ) -> Result<TargetReclaimOutcome> {
        let target = match access.with_store(|store| {
            store.reclaim_terminal_target_locked(custody_id, expected_generation)
        }) {
            Some(Ok(Some(target))) => target,
            Some(Ok(None)) => {
                return Ok(TargetReclaimOutcome::refused(
                    ReclaimSkipReason::CustodyOrGenerationDrift,
                ));
            }
            Some(Err(error)) => return Err(error),
            None => {
                return Ok(TargetReclaimOutcome::refused(
                    ReclaimSkipReason::DurationBudget,
                ));
            }
        };
        // The database selection only establishes a generation/idle fence. It
        // is not filesystem authority: every identity fact is authenticated
        // again while the same root stripe remains held through deletion.
        if target.owner_session_id != target.session_id
            || target.validated_generation != expected_generation
        {
            return Ok(TargetReclaimOutcome::refused(
                ReclaimSkipReason::CustodyOrGenerationDrift,
            ));
        }
        let pinned =
            match PinnedSandboxRoot::open(sandbox_base, &target.sandbox_root, target.allocation_id)
            {
                Ok(pinned) => pinned,
                Err(reason) => return Ok(TargetReclaimOutcome::refused(reason)),
            };
        let root = pinned.root_path();
        let canonical = match std::fs::canonicalize(&target.canonical_repo_dir) {
            Ok(canonical) => canonical,
            Err(_) => {
                return Ok(TargetReclaimOutcome::refused(
                    ReclaimSkipReason::GitOrRootIdentityRefusal,
                ));
            }
        };
        if !git_output(&root, ["rev-parse", "--show-toplevel"])
            .is_some_and(|top| PathBuf::from(top) == root)
            || git_output(&root, ["branch", "--show-current"]).as_deref()
                != Some(target.sandbox_branch.as_str())
            || !git_is_ancestor(&root, &target.source_commit)
        {
            return Ok(TargetReclaimOutcome::refused(
                ReclaimSkipReason::GitOrRootIdentityRefusal,
            ));
        }
        let Some(common_dir) = git_output(
            &root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ) else {
            return Ok(TargetReclaimOutcome::refused(
                ReclaimSkipReason::GitOrRootIdentityRefusal,
            ));
        };
        if std::fs::canonicalize(common_dir)
            .ok()
            .map(|path| path.to_string_lossy().to_string())
            != Some(target.repository_identity)
        {
            return Ok(TargetReclaimOutcome::refused(
                ReclaimSkipReason::GitOrRootIdentityRefusal,
            ));
        }
        let Some(registered) = git_output(&canonical, ["worktree", "list", "--porcelain"]) else {
            return Ok(TargetReclaimOutcome::refused(
                ReclaimSkipReason::GitOrRootIdentityRefusal,
            ));
        };
        if !has_matching_worktree_stanza(&registered, &root, &target.sandbox_branch) {
            return Ok(TargetReclaimOutcome::refused(
                ReclaimSkipReason::GitOrRootIdentityRefusal,
            ));
        }
        if !remove {
            return Ok(pinned.inspect_target());
        }
        let identity = match pinned.registered_target_identity() {
            Ok(identity) => identity,
            Err(reason) => return Ok(TargetReclaimOutcome::refused(reason)),
        };
        let root_already_locked = access.root_already_locked();
        let mut epoch = 0u32;
        let mut epoch_attempts = 0u32;
        let mut store_device = identity.device;
        let prepared = loop {
            store_device = match crate::store::target_reclaim_sweep::store_target_device(
                identity.device,
                epoch,
            ) {
                Some(store_device) => store_device,
                None => break Some(Err(ReclaimSkipReason::TargetIdentityChanged)),
            };
            let prepared = access.with_store(|store| {
                let _root_guard = if root_already_locked {
                    None
                } else {
                    Some(
                        crate::store::sandbox_custody::try_lock_custody_root(custody_id)
                            .ok_or(ReclaimSkipReason::RootBusy)?,
                    )
                };
                if let Some(active_owner) = active_owner {
                    if active_owner()? {
                        return Err(ReclaimSkipReason::ActiveOwner);
                    }
                }
                store
                    .prepare_target_reclaim_intent(
                        custody_id,
                        expected_generation,
                        store_device,
                        identity.inode,
                    )
                    .map_err(|_| ReclaimSkipReason::CustodyOrGenerationDrift)
            });
            match prepared {
                Some(Ok(PrepareTargetReclaimIntentResult::Terminal(event)))
                    if crate::store::target_reclaim_sweep::terminal_evidence_is_for_earlier_target(
                        &event,
                        identity.birth,
                    ) =>
                {
                    // The same device and inode now name a directory born after
                    // the evidence was recorded: a rebuilt `target/` that reused
                    // the reclaimed inode. It is a new target with its own
                    // identity epoch (#1040).
                    epoch = crate::store::target_reclaim_sweep::next_target_epoch(epoch, identity.birth);
                    if epoch > crate::store::target_reclaim_sweep::TARGET_EPOCH_MAX
                        || epoch_attempts >= MAX_TARGET_EPOCH_PROBES
                    {
                        break Some(Err(ReclaimSkipReason::TargetIdentityChanged));
                    }
                    epoch_attempts += 1;
                }
                other => break other,
            }
        };
        let intent = match prepared {
            Some(Ok(PrepareTargetReclaimIntentResult::Active(intent))) => intent,
            Some(Ok(PrepareTargetReclaimIntentResult::Terminal(event))) => {
                tracing::debug!(
                    schedule_id = event.schedule_id,
                    custody_id = %event.custody_id,
                    generation = event.generation,
                    allocation_id = %event.allocation_id,
                    bucket = event.bucket,
                    slot_name = %event.slot_name,
                    expected_device = event.expected_device,
                    expected_inode = event.expected_inode,
                    terminal_state = ?event.terminal_state,
                    "Target reclaim candidate matched retained terminal intent evidence"
                );
                if event.custody_id != custody_id
                    || event.generation != expected_generation
                    || event.allocation_id != target.allocation_id
                    || event.expected_device != store_device
                    || event.expected_inode != identity.inode
                {
                    return Ok(TargetReclaimOutcome::refused(
                        ReclaimSkipReason::TargetIdentityChanged,
                    ));
                }
                return Ok(match event.terminal_state {
                    TargetReclaimIntentTerminalState::Completed => TargetReclaimOutcome {
                        kind: TargetReclaimKind::StagedRemoved,
                        bytes: 0,
                        reason: None,
                        terminal_rejection: false,
                    },
                    TargetReclaimIntentTerminalState::Abandoned => {
                        TargetReclaimOutcome::refused(ReclaimSkipReason::CustodyOrGenerationDrift)
                    }
                });
            }
            Some(Ok(PrepareTargetReclaimIntentResult::SuccessorReservationPending)) => {
                return Ok(TargetReclaimOutcome::refused(
                    ReclaimSkipReason::CustodyOrGenerationDrift,
                ));
            }
            Some(Err(reason)) => {
                tracing::warn!(
                    custody_id = %custody_id,
                    generation = expected_generation,
                    ?reason,
                    "Failed to durably prepare target reclaim intent"
                );
                return Ok(TargetReclaimOutcome::refused(reason));
            }
            None => {
                return Ok(TargetReclaimOutcome::refused(
                    ReclaimSkipReason::DurationBudget,
                ));
            }
        };
        let filesystem_intent = RegisteredTargetIntent {
            custody_id: intent.custody_id,
            generation: intent.generation,
            allocation_id: intent.allocation_id,
            bucket: intent.bucket,
            slot_name: intent.slot_name.clone(),
            expected_device: crate::store::target_reclaim_sweep::split_store_target_device(
                intent.expected_device,
            )
            .0,
            expected_inode: intent.expected_inode,
        };
        let staged_outcome = pinned.stage_registered_target(&filesystem_intent);
        if staged_outcome.kind == TargetReclaimKind::Refused || staged_outcome.reason.is_some() {
            return Ok(staged_outcome);
        }
        let staged = match access.with_store(|store| store.mark_target_reclaim_staged(&intent)) {
            Some(Ok(intent)) => intent,
            result => {
                return Ok(TargetReclaimOutcome {
                    kind: TargetReclaimKind::StagedPending,
                    bytes: staged_outcome.bytes,
                    reason: Some(if result.is_none() {
                        ReclaimSkipReason::DurationBudget
                    } else {
                        ReclaimSkipReason::StagedDeletionIncomplete
                    }),
                    terminal_rejection: false,
                });
            }
        };
        // The old target is now detached and the Prepared gate has ended.
        // Traverse its pinned private slot only after that durable transition.
        let measured = crate::sandbox::target_reclaim::delete_registered_target(
            sandbox_base,
            &filesystem_intent,
            true,
        );
        if measured.kind != TargetReclaimKind::Inspected {
            return Ok(TargetReclaimOutcome {
                kind: TargetReclaimKind::StagedPending,
                bytes: 0,
                reason: measured
                    .reason
                    .or(Some(ReclaimSkipReason::StagedDeletionIncomplete)),
                terminal_rejection: false,
            });
        }
        let staged_bytes = measured.bytes;
        let deleting = match access.with_store(|store| store.mark_target_reclaim_deleting(&staged))
        {
            Some(Ok(intent)) => intent,
            result => {
                return Ok(TargetReclaimOutcome {
                    kind: TargetReclaimKind::StagedPending,
                    bytes: staged_bytes,
                    reason: Some(if result.is_none() {
                        ReclaimSkipReason::DurationBudget
                    } else {
                        ReclaimSkipReason::StagedDeletionIncomplete
                    }),
                    terminal_rejection: false,
                });
            }
        };
        pause_reclaim_before_delete_for_test(sandbox_base);
        let deleted = crate::sandbox::target_reclaim::delete_registered_target(
            sandbox_base,
            &filesystem_intent,
            false,
        );
        let completion = (deleted.kind == TargetReclaimKind::RecoveredRemoved)
            .then(|| access.with_store(|store| store.complete_target_reclaim_intent(&deleting)));
        let completed = matches!(completion, Some(Some(Ok(()))));
        if !completed {
            return Ok(TargetReclaimOutcome {
                kind: TargetReclaimKind::StagedPending,
                bytes: staged_bytes,
                reason: deleted.reason.or(Some(if matches!(completion, Some(None)) {
                    ReclaimSkipReason::DurationBudget
                } else {
                    ReclaimSkipReason::StagedDeletionIncomplete
                })),
                terminal_rejection: false,
            });
        }
        Ok(TargetReclaimOutcome {
            kind: TargetReclaimKind::StagedRemoved,
            bytes: staged_bytes,
            reason: None,
            terminal_rejection: false,
        })
    }

    fn compatibility_target_bytes(outcome: TargetReclaimOutcome) -> Result<Option<u64>> {
        match outcome.kind {
            TargetReclaimKind::Inspected | TargetReclaimKind::StagedRemoved => {
                Ok(Some(outcome.bytes))
            }
            TargetReclaimKind::Refused
                if outcome.reason == Some(ReclaimSkipReason::TargetAbsent) =>
            {
                Ok(Some(0))
            }
            TargetReclaimKind::Refused => Ok(None),
            TargetReclaimKind::StagedPending
            | TargetReclaimKind::RecoveredRemoved
            | TargetReclaimKind::RecoveredPending => Err(Self::refusal(
                SandboxCustodyErrorCodeV1::PersistenceTransitionFailed,
                None,
                SandboxCustodyTransitionV1::Purge,
            )),
        }
    }

    pub fn refusal(
        code: SandboxCustodyErrorCodeV1,
        session_id: Option<Uuid>,
        transition: SandboxCustodyTransitionV1,
    ) -> DaemonError {
        let (retryable, recovery) = match code {
            SandboxCustodyErrorCodeV1::SourceWorktreeDirty => {
                (false, SandboxCustodyRecoveryV1::CommitSource)
            }
            SandboxCustodyErrorCodeV1::CustodyChanged
            | SandboxCustodyErrorCodeV1::ReclaimPrepared
            | SandboxCustodyErrorCodeV1::RootBusy
            | SandboxCustodyErrorCodeV1::PersistenceTransitionFailed => {
                (true, SandboxCustodyRecoveryV1::RetryAfterReconcile)
            }
            SandboxCustodyErrorCodeV1::HistoricalPurged
            | SandboxCustodyErrorCodeV1::HistoricalTransferred => {
                (false, SandboxCustodyRecoveryV1::CreateNewSession)
            }
            _ => (false, SandboxCustodyRecoveryV1::InspectStatus),
        };
        sandbox_custody_error(SandboxCustodyErrorV1 {
            version: 1,
            code,
            session_id,
            transition,
            retryable,
            recovery,
        })
    }
}

fn startup_error_code(error: &DaemonError) -> Option<SandboxCustodyErrorCodeV1> {
    let DaemonError::StructuredRpc { message, .. } = error else {
        return None;
    };
    Some(match message.strip_prefix("sandbox_custody:") {
        Some("tuple_incomplete") => SandboxCustodyErrorCodeV1::TupleIncomplete,
        Some("historical_purged") => SandboxCustodyErrorCodeV1::HistoricalPurged,
        Some("historical_transferred") => SandboxCustodyErrorCodeV1::HistoricalTransferred,
        Some("cleanup_failed") => SandboxCustodyErrorCodeV1::CleanupFailed,
        Some("root_missing") => SandboxCustodyErrorCodeV1::RootMissing,
        Some("root_outside_base") => SandboxCustodyErrorCodeV1::RootOutsideBase,
        Some("root_identity_mismatch") => SandboxCustodyErrorCodeV1::RootIdentityMismatch,
        Some("worktree_mismatch") => SandboxCustodyErrorCodeV1::WorktreeMismatch,
        Some("ownership_missing") => SandboxCustodyErrorCodeV1::OwnershipMissing,
        Some("ownership_conflict") => SandboxCustodyErrorCodeV1::OwnershipConflict,
        Some("source_revision_unavailable") => SandboxCustodyErrorCodeV1::SourceRevisionUnavailable,
        Some("source_worktree_dirty") => SandboxCustodyErrorCodeV1::SourceWorktreeDirty,
        Some("allocation_failed") => SandboxCustodyErrorCodeV1::AllocationFailed,
        // A failed CAS/settlement, or an unknown structured failure, leaves
        // readiness unproven and must propagate rather than fabricate an
        // invalidation classification.
        Some("custody_changed" | "reclaim_prepared" | "persistence_transition_failed") | None => {
            return None;
        }
        Some(_) => return None,
    })
}

/// Returns allocation-first lineage only when every persisted participant is
/// one unbranched rotation/retry edge. `continued_from` alone is insufficient:
/// the durable retry/rotation counters must prove the transition class.
fn legacy_lineage(sessions: &[Session], allocation: Uuid) -> Option<Vec<(Uuid, CustodyCause)>> {
    let by_id: BTreeMap<Uuid, &Session> = sessions
        .iter()
        .map(|session| (session.id, session))
        .collect();
    if by_id.len() != sessions.len() || !by_id.contains_key(&allocation) {
        return None;
    }
    let mut children: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    let mut sources = Vec::new();
    for session in sessions {
        match session.continued_from {
            Some(parent) if by_id.contains_key(&parent) => {
                children.entry(parent).or_default().push(session.id)
            }
            Some(_) => return None,
            None => sources.push(session.id),
        }
    }
    if sources != vec![allocation] || children.values().any(|value| value.len() > 1) {
        return None;
    }
    let mut result = vec![(allocation, CustodyCause::StartupReconciliation)];
    let mut current = allocation;
    while let Some(nexts) = children.get(&current) {
        let next = *nexts.first()?;
        let previous = by_id.get(&current)?;
        let successor = by_id.get(&next)?;
        let retry_proven = previous.status == SessionStatus::Failed
            && previous.retry_attempt == previous.max_retries
            && previous.max_retries.is_some_and(|max| max > 0)
            && successor.max_retries == previous.max_retries
            && successor
                .retry_attempt
                .is_some_and(|attempt| attempt > 0 && Some(attempt) <= successor.max_retries)
            && successor.rotation_depth == previous.rotation_depth;
        // Production rotation builds a fresh child with no retry budget even
        // when the rotated parent carried one; retry and rotation are mutually
        // exclusive lineage causes.
        let rotation_proven = successor.rotation_depth == previous.rotation_depth + 1
            && successor.retry_attempt.is_none()
            && successor.max_retries.is_none();
        let cause = if retry_proven {
            CustodyCause::Retry
        } else if rotation_proven {
            CustodyCause::Rotation
        } else {
            return None;
        };
        result.push((next, cause));
        current = next;
        if result.len() > sessions.len() {
            return None;
        }
    }
    (result.len() == sessions.len()).then_some(result)
}

/// Source of `git worktree list --porcelain` for live-root authentication.
///
/// Runtime effects always read a fresh listing. Startup custody
/// classification runs before any provider or sandbox mutation is restored,
/// and the daemon is the only writer of sandbox worktrees, so it reads each
/// repository's listing once and reuses it for every live root (#961): with
/// ~1,400 registered worktrees one listing costs ~31 ms, and the per-root
/// call made startup classification quadratic (32.8 s on the hub). Every
/// other per-root check (paths, branch, common dir, source ancestry) still
/// runs per root.
///
/// `StartupPrefetched` is the same snapshot plus live-root proofs computed
/// concurrently before the serial pass (see
/// [`CustodyService::prefetch_startup_live_proofs`]).
enum WorktreeListings {
    Fresh,
    StartupSnapshot(HashMap<PathBuf, Option<String>>),
    StartupPrefetched(Box<StartupPrefetch>),
}

/// The startup listing snapshot and the proofs computed from it.
struct StartupPrefetch {
    listings: HashMap<PathBuf, Option<String>>,
    proofs: HashMap<Uuid, PrefetchedLiveProof>,
    terminal_proofs: HashMap<Uuid, PrefetchedTerminalProof>,
}

/// Everything `probe_startup_terminal_root` reads besides the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalProofInputs {
    pub(crate) root_path: PathBuf,
    pub(crate) canonical_repo: PathBuf,
    pub(crate) sandbox_branch: String,
    pub(crate) repository_identity: String,
    pub(crate) source_commit: String,
}

/// The filesystem/Git evidence for a terminal root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalRootProof {
    /// The root is genuinely absent (compatible with completed cleanup).
    Absent,
    /// Evidence substitution (symlink, non-directory, non-canonical path).
    Refused(SandboxCustodyErrorCodeV1),
    /// The retained worktree's Git evidence, before the tuple check.
    Evidence { valid: bool },
}

impl TerminalRootProof {
    /// Outcomes that publish history rather than quarantine.
    const fn admits(self) -> bool {
        matches!(self, Self::Absent | Self::Evidence { valid: true })
    }
}

/// A concurrently computed terminal-root admission, its exact inputs and
/// the worktree fingerprint it was computed against.
struct PrefetchedTerminalProof {
    inputs: TerminalProofInputs,
    fingerprint: WorktreeFingerprint,
    proof: TerminalRootProof,
}

/// A concurrently computed live-root admission (the probe passed), its
/// exact inputs and the worktree fingerprint it was computed against. It is
/// used only when the serial pass reaches the same root with identical
/// inputs and an identical fingerprint; anything else re-probes serially.
struct PrefetchedLiveProof {
    inputs: LiveProofInputs,
    fingerprint: WorktreeFingerprint,
}

/// One observed filesystem fact: present with a value, absent, or not
/// readable (any other I/O error, which disqualifies a cached proof).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Observed<T> {
    Missing,
    Present(T),
    Unreadable,
}

impl<T> Observed<T> {
    fn from_io(result: std::io::Result<T>) -> Self {
        match result {
            Ok(value) => Self::Present(value),
            // A path under a regular file cannot exist either (the reftable
            // backend leaves `refs/heads` as a stub file).
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Self::Missing
            }
            Err(_) => Self::Unreadable,
        }
    }

    const fn is_readable(&self) -> bool {
        !matches!(self, Self::Unreadable)
    }

    fn present(&self) -> Option<&T> {
        match self {
            Self::Present(value) => Some(value),
            Self::Missing | Self::Unreadable => None,
        }
    }
}

/// Everything a startup worktree probe's verdict depends on besides its
/// Store inputs, read with plain file reads (no Git process) so the serial
/// pass can cheaply confirm a prefetched proof is still current (#961):
/// the path resolutions the probes perform, the root's inode, the linked
/// worktree's `.git` pointer, its administrative files (`HEAD`,
/// `commondir`, the `gitdir` back-link that `git worktree list` reports,
/// `config.worktree`), the common directory's `config`, the storage of the
/// branch `HEAD` names (loose ref and `packed-refs` line for the files
/// backend; the common and per-worktree reftable table lists, which change
/// on every ref update, for the reftable backend) and the files that can
/// change ancestry (`shallow`, `info/grafts`, `objects/info/alternates`,
/// replace refs).
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeFingerprint {
    sandbox_base: Option<Observed<PathBuf>>,
    root_inode: Observed<(u64, u64, bool, bool)>,
    root: Observed<PathBuf>,
    repository: Observed<PathBuf>,
    repository_git: Observed<PathBuf>,
    dot_git: Observed<Vec<u8>>,
    head: Observed<Vec<u8>>,
    commondir: Observed<Vec<u8>>,
    backlink: Observed<Vec<u8>>,
    worktree_config: Observed<Vec<u8>>,
    common: Observed<PathBuf>,
    config: Observed<Vec<u8>>,
    branch_ref: Observed<Vec<u8>>,
    packed_branch_ref: Observed<Vec<Vec<u8>>>,
    reftable: Observed<Vec<u8>>,
    worktree_reftable: Observed<Vec<u8>>,
    shallow: Observed<Vec<u8>>,
    grafts: Observed<Vec<u8>>,
    alternates: Observed<Vec<u8>>,
    replace_refs: Observed<Vec<(std::ffi::OsString, Vec<u8>)>>,
}

impl WorktreeFingerprint {
    fn observe(root: &Path, repository: &Path, sandbox_base: Option<&Path>) -> Self {
        let read = |path: &Path| Observed::from_io(std::fs::read(path));
        let dot_git = read(&root.join(".git"));
        // A linked worktree's `.git` file names its administrative dir.
        let gitdir = dot_git.present().and_then(|contents| {
            let text = std::str::from_utf8(contents).ok()?;
            let target = text.trim_end().strip_prefix("gitdir: ")?;
            Some(root.join(target))
        });
        let at_gitdir = |name: &str| match &gitdir {
            Some(gitdir) => read(&gitdir.join(name)),
            None => Observed::Missing,
        };
        let head = at_gitdir("HEAD");
        let commondir = at_gitdir("commondir");
        let common_path = gitdir.as_ref().and_then(|gitdir| {
            let text = std::str::from_utf8(commondir.present()?).ok()?;
            Some(gitdir.join(text.trim_end()))
        });
        let at_common = |name: &str| match &common_path {
            Some(common) => read(&common.join(name)),
            None => Observed::Missing,
        };
        let branch = head.present().and_then(|contents| {
            let text = std::str::from_utf8(contents).ok()?;
            Some(text.trim_end().strip_prefix("ref: ")?.to_owned())
        });
        let branch_ref = match &branch {
            Some(branch) => at_common(branch),
            None => Observed::Missing,
        };
        let packed_branch_ref = match (&branch, at_common("packed-refs")) {
            (Some(branch), Observed::Present(packed)) => {
                let suffix = format!(" {branch}");
                Observed::Present(
                    packed
                        .split(|byte| *byte == b'\n')
                        .filter(|line| line.ends_with(suffix.as_bytes()))
                        .map(<[u8]>::to_vec)
                        .collect(),
                )
            }
            (_, Observed::Unreadable) => Observed::Unreadable,
            _ => Observed::Missing,
        };
        let replace_refs = match &common_path {
            Some(common) => match std::fs::read_dir(common.join("refs/replace")) {
                Ok(entries) => {
                    let mut refs = Vec::new();
                    let mut readable = true;
                    for entry in entries {
                        match entry.and_then(|entry| {
                            std::fs::read(entry.path())
                                .map(|contents| (entry.file_name(), contents))
                        }) {
                            Ok(reference) => refs.push(reference),
                            Err(_) => readable = false,
                        }
                    }
                    refs.sort();
                    if readable {
                        Observed::Present(refs)
                    } else {
                        Observed::Unreadable
                    }
                }
                Err(error) => Observed::from_io(Err(error)),
            },
            None => Observed::Missing,
        };
        Self {
            sandbox_base: sandbox_base.map(|base| Observed::from_io(std::fs::canonicalize(base))),
            root_inode: Observed::from_io(root_inode(root)),
            root: Observed::from_io(std::fs::canonicalize(root)),
            repository: Observed::from_io(std::fs::canonicalize(repository)),
            repository_git: Observed::from_io(std::fs::canonicalize(repository.join(".git"))),
            dot_git,
            backlink: at_gitdir("gitdir"),
            worktree_config: at_gitdir("config.worktree"),
            head,
            commondir,
            common: match &common_path {
                Some(common) => Observed::from_io(std::fs::canonicalize(common)),
                None => Observed::Missing,
            },
            config: at_common("config"),
            branch_ref,
            packed_branch_ref,
            reftable: at_common("reftable/tables.list"),
            worktree_reftable: at_gitdir("reftable/tables.list"),
            shallow: at_common("shallow"),
            grafts: at_common("info/grafts"),
            alternates: at_common("objects/info/alternates"),
            replace_refs,
        }
    }

    /// Whether every fact was read or proven absent.
    fn is_readable(&self) -> bool {
        self.sandbox_base.as_ref().is_none_or(Observed::is_readable)
            && self.root_inode.is_readable()
            && self.root.is_readable()
            && self.repository.is_readable()
            && self.repository_git.is_readable()
            && self.dot_git.is_readable()
            && self.head.is_readable()
            && self.commondir.is_readable()
            && self.backlink.is_readable()
            && self.worktree_config.is_readable()
            && self.common.is_readable()
            && self.config.is_readable()
            && self.branch_ref.is_readable()
            && self.packed_branch_ref.is_readable()
            && self.reftable.is_readable()
            && self.worktree_reftable.is_readable()
            && self.shallow.is_readable()
            && self.grafts.is_readable()
            && self.alternates.is_readable()
            && self.replace_refs.is_readable()
    }
}

#[cfg(unix)]
fn root_inode(root: &Path) -> std::io::Result<(u64, u64, bool, bool)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(root)?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.is_dir(),
        metadata.file_type().is_symlink(),
    ))
}

#[cfg(not(unix))]
fn root_inode(_root: &Path) -> std::io::Result<(u64, u64, bool, bool)> {
    Err(std::io::Error::other("root inode is unavailable"))
}

/// Everything `probe_live_filesystem` reads besides the (pass-constant)
/// sandbox base, the filesystem and the listing snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveProofInputs {
    session_id: Uuid,
    working_dir: PathBuf,
    sandbox_root: Option<PathBuf>,
    sandbox_branch: Option<String>,
    persisted: crate::store::sandbox_custody::PersistedCustody,
}

impl LiveProofInputs {
    fn of(session: &Session, persisted: &crate::store::sandbox_custody::PersistedCustody) -> Self {
        Self {
            session_id: session.id,
            working_dir: session.working_dir.clone(),
            sandbox_root: session.sandbox_root.clone(),
            sandbox_branch: session.sandbox_branch.clone(),
            persisted: persisted.clone(),
        }
    }
}

impl WorktreeListings {
    fn startup_snapshot() -> Self {
        Self::StartupSnapshot(HashMap::new())
    }

    /// Whether `repo` registers `root` on `branch`; `None` when the listing
    /// cannot be read.
    fn registers(&mut self, repo: &Path, root: &Path, branch: &str) -> Option<bool> {
        let cache = match self {
            Self::Fresh => {
                return git_output(repo, ["worktree", "list", "--porcelain"])
                    .map(|listing| has_matching_worktree_stanza(&listing, root, branch));
            }
            Self::StartupSnapshot(cache) => cache,
            Self::StartupPrefetched(prefetch) => &mut prefetch.listings,
        };
        cache
            .entry(repo.to_path_buf())
            .or_insert_with(|| git_output(repo, ["worktree", "list", "--porcelain"]))
            .as_deref()
            .map(|listing| has_matching_worktree_stanza(listing, root, branch))
    }

    /// The prefetched admission for this root, if it was computed from
    /// exactly these inputs and the worktree fingerprint is unchanged now.
    /// Each proof is used at most once; a second authorization of the same
    /// root in one pass, any drift, and every refusal re-probe.
    fn take_prefetched_proof(
        &mut self,
        session: &Session,
        persisted: &crate::store::sandbox_custody::PersistedCustody,
        sandbox_base: &Path,
    ) -> Option<std::result::Result<(), SandboxCustodyErrorCodeV1>> {
        let Self::StartupPrefetched(prefetch) = self else {
            return None;
        };
        let prefetched = prefetch.proofs.remove(&persisted.custody_id)?;
        if prefetched.inputs != LiveProofInputs::of(session, persisted) {
            return None;
        }
        let root = session.sandbox_root.as_deref()?;
        let now = WorktreeFingerprint::observe(root, &session.working_dir, Some(sandbox_base));
        (now.is_readable() && now == prefetched.fingerprint).then_some(Ok(()))
    }

    /// The prefetched terminal-root admission, under the same once-only,
    /// identical-inputs, unchanged-fingerprint rule.
    fn take_prefetched_terminal_proof(
        &mut self,
        custody_id: Uuid,
        inputs: &TerminalProofInputs,
    ) -> Option<TerminalRootProof> {
        let Self::StartupPrefetched(prefetch) = self else {
            return None;
        };
        let prefetched = prefetch.terminal_proofs.remove(&custody_id)?;
        if prefetched.inputs != *inputs {
            return None;
        }
        let now = WorktreeFingerprint::observe(&inputs.root_path, &inputs.canonical_repo, None);
        (now.is_readable() && now == prefetched.fingerprint).then_some(prefetched.proof)
    }
}

/// Bounded pool for startup live-root proofs (#961).
const STARTUP_PROOF_POOL_MAX: usize = 8;

pub(crate) fn startup_proof_pool_size() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(STARTUP_PROOF_POOL_MAX)
}

/// Run `work` for every item on up to `pool` scoped threads, each with its
/// own `state`, and return the results in item order.
fn scoped_pool_map<T: Sync, S, R: Send>(
    items: &[T],
    pool: usize,
    state: impl Fn() -> S + Sync,
    work: impl Fn(&mut S, usize, &T) -> R + Sync,
) -> Vec<R> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<R>>> =
        items.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..pool.clamp(1, items.len().max(1)) {
            scope.spawn(|| {
                let mut local = state();
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result = work(&mut local, index, item);
                    *results[index]
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
                }
            });
        }
    });
    results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .expect("every item is claimed exactly once")
        })
        .collect()
}

fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A worktree's top level, absolute common directory and symbolic HEAD
/// from one `git rev-parse`, each line trimmed exactly as `git_output`
/// trims one call's output.
#[derive(Debug, PartialEq, Eq)]
struct GitWorktreeIdentity {
    toplevel: String,
    common_dir: String,
    head: String,
}

fn git_worktree_identity(root: &Path) -> Option<GitWorktreeIdentity> {
    let output = std::process::Command::new("git")
        .args([
            "rev-parse",
            "--show-toplevel",
            "--path-format=absolute",
            "--git-common-dir",
            "--symbolic-full-name",
            "HEAD",
        ])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let identity = GitWorktreeIdentity {
        toplevel: lines.next()?.trim().to_owned(),
        common_dir: lines.next()?.trim().to_owned(),
        head: lines.next()?.trim().to_owned(),
    };
    lines.next().is_none().then_some(identity)
}

fn git_success<const N: usize>(cwd: &Path, args: [&str; N]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .is_ok_and(|status| status.success())
}

fn git_is_ancestor(root: &Path, source_commit: &str) -> bool {
    std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", source_commit, "HEAD"])
        .current_dir(root)
        .status()
        .is_ok_and(|status| status.success())
}

/// Epoch probes one reclaim attempt makes before it refuses. Each probe is one
/// retained terminal event for a target that reused the same device and inode.
const MAX_TARGET_EPOCH_PROBES: u32 = 8;

fn has_matching_worktree_stanza(porcelain: &str, root: &Path, branch: &str) -> bool {
    let expected_root = format!("worktree {}", root.display());
    let expected_branch = format!("branch refs/heads/{branch}");
    porcelain.split("\n\n").any(|stanza| {
        let mut lines = stanza.lines();
        lines.next() == Some(expected_root.as_str()) && lines.any(|line| line == expected_branch)
    })
}

fn has_matching_worktree_stanza_with_head(
    porcelain: &str,
    root: &Path,
    branch: &str,
    head: &str,
) -> bool {
    let expected_root = format!("worktree {}", root.display());
    let expected_branch = format!("branch refs/heads/{branch}");
    let expected_head = format!("HEAD {head}");
    porcelain.split("\n\n").any(|stanza| {
        let lines = stanza.lines().collect::<Vec<_>>();
        lines.first() == Some(&expected_root.as_str())
            && lines.contains(&expected_branch.as_str())
            && lines.contains(&expected_head.as_str())
    })
}

#[cfg(test)]
mod worktree_listing_tests {
    use super::WorktreeListings;
    use std::path::Path;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn startup_snapshot_reads_each_repository_listing_once_for_every_root() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let roots = (0..3)
            .map(|index| {
                let root = temp.path().join(format!("root-{index}"));
                let branch = format!("rsi/root-{index}");
                git(
                    &repo,
                    &[
                        "worktree",
                        "add",
                        "-q",
                        "-b",
                        &branch,
                        root.to_str().unwrap(),
                    ],
                );
                (std::fs::canonicalize(&root).unwrap(), branch)
            })
            .collect::<Vec<_>>();
        let repo = std::fs::canonicalize(&repo).unwrap();

        let mut snapshot = WorktreeListings::startup_snapshot();
        for (root, branch) in &roots {
            assert_eq!(snapshot.registers(&repo, root, branch), Some(true));
        }
        // A root on the wrong branch is still refused from the snapshot.
        assert_eq!(
            snapshot.registers(&repo, &roots[0].0, "rsi/other"),
            Some(false)
        );
        let WorktreeListings::StartupSnapshot(cache) = &snapshot else {
            panic!("startup snapshot mode");
        };
        assert_eq!(cache.len(), 1, "one listing read for one repository");

        // Runtime authentication reads a fresh listing and sees a removal.
        git(
            &repo,
            &[
                "worktree",
                "remove",
                "--force",
                roots[2].0.to_str().unwrap(),
            ],
        );
        assert_eq!(
            WorktreeListings::Fresh.registers(&repo, &roots[2].0, &roots[2].1),
            Some(false)
        );
        assert_eq!(
            WorktreeListings::Fresh.registers(&repo, &roots[1].0, &roots[1].1),
            Some(true)
        );
    }
    /// A prefetched startup admission is used once, only for the exact
    /// inputs it was computed from, and only while the worktree fingerprint
    /// is unchanged (#961).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn prefetched_proof_is_used_once_and_only_for_identical_inputs() {
        use super::{
            LiveProofInputs, PrefetchedLiveProof, PrefetchedTerminalProof, StartupPrefetch,
            TerminalProofInputs, TerminalRootProof, WorktreeFingerprint,
        };
        use crate::store::sandbox_custody::PersistedCustody;
        use std::collections::HashMap;
        use uuid::Uuid;

        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base");
        let root = base.join("root");
        let repo = temp.path().join("repo");
        let mut session = crate::store::tests::make_test_session();
        session.working_dir = repo.clone();
        session.sandbox_root = Some(root.clone());
        session.sandbox_branch = Some("rsi/root".into());
        let persisted = PersistedCustody {
            custody_id: Uuid::new_v4(),
            allocation_session_id: session.id,
            allocation_id: session.id,
            owner_session_id: session.id,
            generation: 1,
            canonical_repo_dir: repo.display().to_string(),
            sandbox_root: root.display().to_string(),
            sandbox_branch: "rsi/root".into(),
            repository_identity: "/repo/.git".into(),
            source_commit: "0".repeat(40),
        };
        let prefetched = || {
            WorktreeListings::StartupPrefetched(Box::new(StartupPrefetch {
                listings: HashMap::new(),
                terminal_proofs: HashMap::new(),
                proofs: HashMap::from([(
                    persisted.custody_id,
                    PrefetchedLiveProof {
                        inputs: LiveProofInputs::of(&session, &persisted),
                        fingerprint: WorktreeFingerprint::observe(&root, &repo, Some(&base)),
                    },
                )]),
            }))
        };

        let mut listings = prefetched();
        assert_eq!(
            listings.take_prefetched_proof(&session, &persisted, &base),
            Some(Ok(()))
        );
        assert_eq!(
            listings.take_prefetched_proof(&session, &persisted, &base),
            None,
            "a second authorization in the pass re-probes"
        );

        let mut listings = prefetched();
        let mut drifted = session.clone();
        drifted.sandbox_branch = Some("rsi/other".into());
        assert_eq!(
            listings.take_prefetched_proof(&drifted, &persisted, &base),
            None
        );

        let mut listings = prefetched();
        let mut advanced = persisted.clone();
        advanced.generation = 2;
        assert_eq!(
            listings.take_prefetched_proof(&session, &advanced, &base),
            None
        );

        // The worktree changed after the proof: re-probe.
        let mut listings = prefetched();
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(
            listings.take_prefetched_proof(&session, &persisted, &base),
            None
        );

        assert_eq!(
            WorktreeListings::Fresh.take_prefetched_proof(&session, &persisted, &base),
            None
        );

        // Terminal-root admissions follow the same rule.
        let inputs = TerminalProofInputs {
            root_path: root.clone(),
            canonical_repo: repo.clone(),
            sandbox_branch: "rsi/root".into(),
            repository_identity: "/repo/.git".into(),
            source_commit: "0".repeat(40),
        };
        let custody_id = persisted.custody_id;
        let terminal = || {
            WorktreeListings::StartupPrefetched(Box::new(StartupPrefetch {
                listings: HashMap::new(),
                proofs: HashMap::new(),
                terminal_proofs: HashMap::from([(
                    custody_id,
                    PrefetchedTerminalProof {
                        inputs: inputs.clone(),
                        fingerprint: WorktreeFingerprint::observe(&root, &repo, None),
                        proof: TerminalRootProof::Evidence { valid: true },
                    },
                )]),
            }))
        };
        let mut listings = terminal();
        assert_eq!(
            listings.take_prefetched_terminal_proof(custody_id, &inputs),
            Some(TerminalRootProof::Evidence { valid: true })
        );
        assert_eq!(
            listings.take_prefetched_terminal_proof(custody_id, &inputs),
            None
        );
        let mut listings = terminal();
        let mut moved = inputs.clone();
        moved.canonical_repo = temp.path().join("elsewhere");
        assert_eq!(
            listings.take_prefetched_terminal_proof(custody_id, &moved),
            None
        );
        let mut listings = terminal();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            listings.take_prefetched_terminal_proof(custody_id, &inputs),
            None
        );
    }

    /// The fingerprint sees a branch switch, a moved branch tip and a removed
    /// root without running Git.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn worktree_fingerprint_sees_head_branch_tip_and_root_changes() {
        use super::WorktreeFingerprint;

        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let root = temp.path().join("root");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "rsi/root",
                root.to_str().unwrap(),
            ],
        );
        let root = std::fs::canonicalize(&root).unwrap();
        let repo = std::fs::canonicalize(&repo).unwrap();
        let observe = || WorktreeFingerprint::observe(&root, &repo, Some(temp.path()));
        let original = observe();
        assert!(original.is_readable());
        assert_eq!(observe(), original, "stable while nothing changes");

        git(&root, &["commit", "-q", "--allow-empty", "-m", "moved tip"]);
        let moved = observe();
        assert_ne!(moved, original, "a new branch tip is seen");

        git(&repo, &["pack-refs", "--all"]);
        let packed = observe();
        assert_ne!(packed, moved, "packing the branch ref is seen");
        assert_eq!(observe(), packed);

        git(&root, &["switch", "-q", "-c", "rsi/elsewhere"]);
        assert_ne!(observe(), packed, "a switched HEAD is seen");

        std::fs::remove_dir_all(&root).unwrap();
        assert_ne!(observe(), packed, "a removed root is seen");
    }

    /// With the reftable backend the per-worktree table list carries HEAD
    /// and the common one carries branch tips; both are fingerprinted.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn worktree_fingerprint_sees_reftable_head_and_branch_changes() {
        use super::WorktreeFingerprint;

        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(
            &repo,
            &["init", "-q", "-b", "main", "--ref-format=reftable"],
        );
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let root = temp.path().join("root");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "rsi/root",
                root.to_str().unwrap(),
            ],
        );
        let root = std::fs::canonicalize(&root).unwrap();
        let repo = std::fs::canonicalize(&repo).unwrap();
        let observe = || WorktreeFingerprint::observe(&root, &repo, Some(temp.path()));
        let original = observe();
        assert!(original.is_readable(), "{original:?}");
        assert_eq!(observe(), original);

        git(&root, &["commit", "-q", "--allow-empty", "-m", "moved tip"]);
        let moved = observe();
        assert_ne!(moved, original, "a new reftable branch tip is seen");

        git(&root, &["switch", "-q", "-c", "rsi/elsewhere"]);
        assert_ne!(observe(), moved, "a switched reftable HEAD is seen");
    }

    /// The combined rev-parse prints exactly what the separate calls print,
    /// and a detached HEAD is never taken for a branch (#961).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn combined_rev_parse_matches_the_separate_calls() {
        use super::{GitWorktreeIdentity, git_output, git_worktree_identity};

        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let root = temp.path().join("root");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "rsi/root",
                root.to_str().unwrap(),
            ],
        );
        let root = std::fs::canonicalize(&root).unwrap();
        let separate = || GitWorktreeIdentity {
            toplevel: git_output(&root, ["rev-parse", "--show-toplevel"]).unwrap(),
            common_dir: git_output(
                &root,
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .unwrap(),
            head: format!(
                "refs/heads/{}",
                git_output(&root, ["branch", "--show-current"]).unwrap()
            ),
        };
        assert_eq!(git_worktree_identity(&root), Some(separate()));

        git(&root, &["checkout", "-q", "--detach"]);
        let identity = git_worktree_identity(&root).unwrap();
        assert_eq!(identity.head, "HEAD");
        assert_eq!(
            git_output(&root, ["branch", "--show-current"]).as_deref(),
            Some("")
        );
        assert!(git_worktree_identity(&temp.path().join("missing")).is_none());
    }
}

/// Operator-run timing of startup custody classification against a copy of a
/// real store (#961). The source database is copied before it is opened, and
/// every Git probe is read-only, so the live store and sandboxes are never
/// mutated:
///
/// ```text
/// sqlite3 ~/.rsi/rsi.db ".backup /tmp/rsi-bench.db"
/// RSI_CUSTODY_BENCH_DB=/tmp/rsi-bench.db \
///   cargo test --release -p rsid --lib startup_custody_bench -- --ignored --nocapture
/// ```
///
/// `RSI_CUSTODY_BENCH_POOL` overrides the proof pool (1 is fully serial);
/// `RSI_CUSTODY_BENCH_DUMP=<file>` writes the final custody roots, event
/// write order, projections and Session statuses (no timestamps).
#[cfg(test)]
mod startup_custody_bench {
    use super::CustodyService;
    use crate::store::Store;
    use std::path::PathBuf;
    use std::time::Instant;

    fn outcome_dump(store: &Store) -> String {
        const QUERIES: [&str; 4] = [
            "SELECT 'root|'||custody_id||'|'||state||'|'||COALESCE(owner_session_id,'')||'|'||
                    generation||'|'||event_sequence||'|'||validation_state||'|'||
                    COALESCE(validated_generation,'')||'|'||COALESCE(validation_error_code,'')
               FROM sandbox_custody_roots ORDER BY custody_id",
            "SELECT 'event|'||custody_id||'|'||sequence||'|'||event_kind||'|'||cause||'|'||
                    COALESCE(error_code,'')
               FROM sandbox_custody_events ORDER BY rowid",
            "SELECT 'projection|'||session_id||'|'||execution_state||'|'||freshness||'|'||
                    COALESCE(effective_cwd,'')||'|'||COALESCE(custody_id,'')||'|'||
                    COALESCE(custody_generation,'')||'|'||COALESCE(error_code,'')
               FROM session_execution_projections ORDER BY session_id",
            "SELECT 'session|'||id||'|'||status||'|'||COALESCE(stop_reason,'')
               FROM sessions ORDER BY id",
        ];
        let mut dump = String::new();
        for sql in QUERIES {
            let mut statement = store.conn.prepare(sql).expect("dump query");
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("dump rows");
            for row in rows {
                dump.push_str(&row.expect("dump row"));
                dump.push('\n');
            }
        }
        dump
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    #[ignore = "needs RSI_CUSTODY_BENCH_DB and reads real sandboxes"]
    fn reconcile_startup_against_store_copy() {
        let Ok(source) = std::env::var("RSI_CUSTODY_BENCH_DB") else {
            eprintln!("RSI_CUSTODY_BENCH_DB unset; skipping");
            return;
        };
        // Print the prefetch summary line (roots, pool, duration_ms).
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_max_level(tracing::Level::INFO)
            .try_init();
        let sandbox_base = std::env::var("RSI_CUSTODY_BENCH_SANDBOX_BASE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").expect("HOME")).join(".rsi/sandboxes")
            });
        let sandbox_base = std::fs::canonicalize(&sandbox_base).expect("sandbox base");
        let pool = std::env::var("RSI_CUSTODY_BENCH_POOL")
            .ok()
            .and_then(|pool| pool.parse().ok())
            .unwrap_or_else(super::startup_proof_pool_size);
        let temp = tempfile::tempdir().expect("tempdir");
        let copy = temp.path().join("rsi.db");
        std::fs::copy(&source, &copy).expect("copy bench store");
        let mut store = Store::open(&copy).expect("open bench store copy");

        let started = Instant::now();
        CustodyService::reconcile_startup_with_pool(&mut store, &sandbox_base, pool)
            .expect("reconcile");
        eprintln!(
            "startup_custody_bench pool={pool} reconcile_ms={}",
            started.elapsed().as_millis()
        );
        if let Ok(path) = std::env::var("RSI_CUSTODY_BENCH_DUMP") {
            std::fs::write(path, outcome_dump(&store)).expect("write dump");
        }
    }
}
