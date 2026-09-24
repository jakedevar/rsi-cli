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
use crate::store::sandbox_custody::{
    ArchivedRotationRestoration, CustodyCause, EffectReservation, LegacyStartupRoot,
    LegacyTerminalRoot, RetryAuthorityCapture, RetryAuthorityFence, RotationAuthorityCapture,
    RotationAuthorityFence, SessionCustodyBinding, StartupCustodyGroup, lock_custody_root,
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
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
#[cfg(test)]
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

#[cfg(test)]
type ReclaimBeforeDeleteHook = (PathBuf, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>);

#[cfg(test)]
fn reclaim_before_delete_hook() -> &'static std::sync::Mutex<Option<ReclaimBeforeDeleteHook>> {
    static HOOK: std::sync::OnceLock<std::sync::Mutex<Option<ReclaimBeforeDeleteHook>>> =
        std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
pub fn set_reclaim_before_delete_hook(hook: Option<ReclaimBeforeDeleteHook>) {
    *reclaim_before_delete_hook()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

#[cfg(test)]
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

#[cfg(not(test))]
const fn pause_reclaim_before_delete_for_test(_base: &Path) {}

/// Cloneable, opaque execution runtime carried only through the manager-owned
/// monitor/rotation recursion. It retains the existing Store, settlement
/// producer, allocator base, and daemon boot identity without reconstructing
/// any of them from raw session metadata.
#[derive(Clone)]
pub(crate) struct CustodyExecutionRuntime {
    store: CustodyStore,
    settlements: CustodySettlementService,
    sandbox_base: PathBuf,
    boot_id: Uuid,
}

impl CustodyExecutionRuntime {
    pub(crate) fn new(
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
    pub(crate) async fn prepare_handoff_resume(&self, session: &Session) -> Result<PreparedLaunch> {
        let transition = SandboxCustodyTransitionV1::HandoffResume;
        let custody = match CustodyService::classify_for_transition(session, transition) {
            Ok(CustodyClassification::OrdinaryUnsandboxed) => {
                CustodyService::authorize_ordinary_for_transition(session, transition)
            }
            Ok(CustodyClassification::RequiresPersistedAuthentication) => {
                let mut store = self.store.lock().await;
                CustodyService::authorize_live(session, &mut store, &self.sandbox_base, transition)
            }
            Err(error) => Err(error),
        }?;
        Ok(PreparedLaunch::new(custody))
    }

    /// The transitional raw-config seam is ContextRead, not ProviderLaunch.
    /// Permit drop queues settlement; provider/process lifetime custody remains
    /// deliberately later work.
    pub(crate) async fn begin_handoff_context_read(
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
    pub(crate) async fn prepare_rotation_successor(
        &self,
        predecessor: &Session,
    ) -> Result<RotationCustodyCandidate> {
        self.prepare_rotation_successor_from(predecessor, RotationPredecessorSource::Completed)
            .await
    }

    /// `prepare_rotation_successor` with an explicit predecessor source.
    /// Only restart recovery passes `RecoveredOpenIntent`.
    pub(crate) async fn prepare_rotation_successor_from(
        &self,
        predecessor: &Session,
        source: RotationPredecessorSource,
    ) -> Result<RotationCustodyCandidate> {
        let transition = SandboxCustodyTransitionV1::Rotation;
        let mut store = self.store.lock().await;
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
                CustodyService::authorize_live(
                    predecessor,
                    &mut store,
                    &self.sandbox_base,
                    transition,
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
    pub(crate) fn apply_rotation_successor_tuple(
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
    pub(crate) async fn bind_rotation_successor(
        &self,
        candidate: RotationCustodyCandidate,
        predecessor: &Session,
        successor: &Session,
    ) -> std::result::Result<BoundRotationCustody, RotationBindFailure> {
        if candidate.predecessor_id != predecessor.id
            || successor.continued_from != Some(predecessor.id)
        {
            let mut store = self.store.lock().await;
            return Err(self.settle_and_classify_rotation_refusal(
                &mut store,
                &candidate,
                predecessor,
                successor.id,
            ));
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
                let mut store = self.store.lock().await;
                return Err(self.settle_and_classify_rotation_refusal(
                    &mut store,
                    &candidate,
                    predecessor,
                    successor.id,
                ));
            }
        };
        let disposition = match binding {
            SessionCustodyBinding::Ordinary => RotationCustodyDisposition::Ordinary,
            SessionCustodyBinding::Transfer { .. } => RotationCustodyDisposition::Transferred,
            _ => unreachable!("rotation only binds ordinary or transfer custody"),
        };
        let mut store = self.store.lock().await;
        if !store
            .completed_rotation_authority_matches(&candidate.durable_predecessor, predecessor.id)
            .unwrap_or(false)
        {
            return Err(self.settle_and_classify_rotation_refusal(
                &mut store,
                &candidate,
                predecessor,
                successor.id,
            ));
        }
        if store
            .bind_reserved_rotation_session_custody(
                successor.id,
                binding,
                &candidate.durable_predecessor,
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
            Err(self.settle_and_classify_rotation_refusal(
                &mut store,
                &candidate,
                predecessor,
                successor.id,
            ))
        }
    }

    fn settle_and_classify_rotation_refusal(
        &self,
        store: &mut Store,
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
                CustodyService::authorize_live(
                    predecessor,
                    store,
                    &self.sandbox_base,
                    SandboxCustodyTransitionV1::Rotation,
                )
                .is_ok_and(|current| current == candidate.custody)
            }
            CustodyHandle::Sandboxed(_) => false,
        };
        if restorable {
            RotationBindFailure::Restorable
        } else {
            RotationBindFailure::Superseded
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_rotation_candidate_predecessor_for_test(
        candidate: &mut RotationCustodyCandidate,
        predecessor_id: Uuid,
    ) {
        candidate.predecessor_id = predecessor_id;
    }

    #[cfg(test)]
    pub(crate) fn replace_rotation_candidate_handle_session_for_test(
        candidate: &mut RotationCustodyCandidate,
        session_id: Uuid,
    ) {
        if let CustodyHandle::Sandboxed(sandboxed) = &mut candidate.custody {
            sandboxed.session_id = session_id;
        }
    }

    pub(crate) async fn begin_rotation_context_read(
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
                let mut store = self.store.lock().await;
                let custody = CustodyService::authorize_live(
                    successor,
                    &mut store,
                    &self.sandbox_base,
                    transition,
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

    pub(crate) async fn settle_bound_rotation_failure(
        &self,
        successor_id: Uuid,
        bound: &BoundRotationCustody,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        let mut store = self.store.lock().await;
        store.fail_bound_rotation_successor(
            successor_id,
            match &bound.expected {
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
            },
            code,
        )
    }

    /// Preserve the exact refusal written by ContextRead revalidation.  A
    /// typed custody refusal carries the bounded code already persisted on an
    /// invalid/quarantined projection; unrelated errors use the transition's
    /// persistence failure class without altering authority state.
    pub(crate) async fn settle_rotation_context_failure(
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
    pub(crate) async fn restore_archived_rotation_predecessor(
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
    pub(crate) async fn finalize_rotation_predecessor(
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
        self.store.lock().await.finalize_rotation_predecessor(
            durable_predecessor,
            successor_id,
            expected,
        )
    }

    /// Authenticate the complete durable Failed retry source before a child
    /// UUID, controller reservation, token, model admission, context read, or
    /// provider effect can exist.
    pub(crate) async fn prepare_retry_successor(
        &self,
        source: &Session,
    ) -> Result<RetryCustodyCandidate> {
        let transition = SandboxCustodyTransitionV1::Retry;
        let mut store = self.store.lock().await;
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
                CustodyService::authorize_live_nonmutating(
                    source,
                    &mut store,
                    &self.sandbox_base,
                    transition,
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

    pub(crate) fn apply_retry_successor_tuple(
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
    pub(crate) async fn bind_retry_successor(
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
            let mut store = self.store.lock().await;
            // Hold the same root stripe from persisted/filesystem/Git
            // reauthentication through the SQL Transfer CAS. Ordinary retry
            // has no exclusive root and therefore needs no stripe.
            let _root_guard = match &candidate.custody {
                CustodyHandle::Ordinary(_) => None,
                CustodyHandle::Sandboxed(expected) => {
                    let guard = lock_custody_root(expected.custody_id);
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

    pub(crate) async fn begin_retry_context_read(
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
                let mut store = self.store.lock().await;
                let custody = CustodyService::authorize_live_nonmutating(
                    successor,
                    &mut store,
                    &self.sandbox_base,
                    transition,
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

    pub(crate) async fn settle_bound_retry_failure(
        &self,
        successor_id: Uuid,
        bound: &BoundRetryCustody,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        self.store.lock().await.fail_bound_rotation_successor(
            successor_id,
            match bound.expected {
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
            },
            code,
        )
    }

    pub(crate) async fn settle_retry_context_failure(
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
    pub(crate) async fn prepare_manager_action_fork(
        &self,
        source: &Session,
    ) -> Result<SpawnForkCandidate> {
        self.prepare_spawn_fork(source).await
    }

    /// Authenticate the same source custody as a normal manager action while
    /// selecting an earlier exact commit already observed and stored by the
    /// daemon. DB-native review uses this after reservation so the author may
    /// advance independently without changing the reviewer's source.
    pub(crate) async fn prepare_manager_action_fork_at(
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
    pub(crate) async fn prepare_manager_handoff(
        &self,
        predecessor: &Session,
        handoff: &rsi_common::harness_manager_v2::ManagerCommittedHandoffV2,
    ) -> Result<(
        crate::store::manager_successions::VerifiedManagerHandoff,
        String,
    )> {
        let transition = SandboxCustodyTransitionV1::AgentSpawnChild;
        let authenticate = |store: &mut Store| -> Result<_> {
            match CustodyService::classify_for_transition(predecessor, transition)? {
                CustodyClassification::OrdinaryUnsandboxed => {
                    CustodyService::authorize_ordinary_for_transition(predecessor, transition)?;
                    Ok(None)
                }
                CustodyClassification::RequiresPersistedAuthentication => {
                    CustodyService::authorize_live_nonmutating(
                        predecessor,
                        store,
                        &self.sandbox_base,
                        transition,
                    )?;
                    Ok(Some(store.live_custody_for_session(predecessor.id)?))
                }
            }
        };
        let source_error = |error: crate::error::DaemonError| {
            tracing::warn!(session_id=%predecessor.id, %error, "manager handoff custody authentication refused");
            crate::store::harness_manager_v2::refused("manager_succession_source_custody_changed")
        };
        let before = authenticate(&mut *self.store.lock().await).map_err(source_error)?;
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
        let after = authenticate(&mut *self.store.lock().await).map_err(source_error)?;
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
    pub(crate) async fn authenticate_manager_candidate(
        &self,
        candidate: &Session,
        expected: &crate::store::manager_successions::ManagerRootCustody,
    ) -> Result<()> {
        let mut store = self.store.lock().await;
        CustodyService::authorize_live_nonmutating(
            candidate,
            &mut store,
            &self.sandbox_base,
            SandboxCustodyTransitionV1::AgentSpawnChild,
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

    pub(crate) async fn prepare_spawn_fork(&self, emitter: &Session) -> Result<SpawnForkCandidate> {
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
                let mut store = self.store.lock().await;
                CustodyService::authorize_live_nonmutating(
                    emitter,
                    &mut store,
                    &self.sandbox_base,
                    transition,
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
    pub(crate) async fn settle_agent_spawn_custody_failure(
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
pub(crate) struct SpawnForkCandidate {
    fork_origin: PathBuf,
    fork_commit: String,
}

impl SpawnForkCandidate {
    /// Repository handle the allocator forks from. This is the emitter's
    /// canonical repository directory used strictly as an object-store
    /// handle; revision authority is exclusively [`Self::fork_commit`].
    pub(crate) fn fork_origin(&self) -> &Path {
        &self.fork_origin
    }

    /// Exact clean emitter HEAD OID captured at authentication time.
    pub(crate) fn fork_commit(&self) -> &str {
        &self.fork_commit
    }
}

pub(crate) struct RotationCustodyCandidate {
    predecessor_id: Uuid,
    durable_predecessor: RotationAuthorityFence,
    custody: CustodyHandle,
}

pub(crate) struct BoundRotationCustody {
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
    pub(crate) fn disposition(&self) -> RotationCustodyDisposition {
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
pub(crate) enum RotationCustodyDisposition {
    Ordinary,
    Transferred,
}

#[derive(Debug)]
pub(crate) enum RotationBindFailure {
    Restorable,
    Superseded,
    SettlementFailed(DaemonError),
}

pub(crate) struct RetryCustodyCandidate {
    source_id: Uuid,
    durable_source: RetryAuthorityFence,
    custody: CustodyHandle,
}

#[derive(Clone)]
pub(crate) struct BoundRetryCustody {
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
    pub(crate) fn disposition(&self) -> RetryCustodyDisposition {
        self.disposition
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryCustodyDisposition {
    Ordinary,
    Transferred,
}

#[derive(Debug)]
pub(crate) enum RetryBindFailure {
    Superseded,
    SettlementFailed(DaemonError),
}

pub(crate) fn custody_error_code(error: &DaemonError) -> Option<SandboxCustodyErrorCodeV1> {
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

#[cfg(test)]
static STARTUP_DIAGNOSTICS: std::sync::Mutex<Vec<(String, String)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn take_startup_diagnostics_for_test() -> Vec<(String, String)> {
    std::mem::take(
        &mut *STARTUP_DIAGNOSTICS
            .lock()
            .expect("startup diagnostics lock"),
    )
}

/// Deterministic startup settlement seam. The injected error is consumed at
/// the real classification-to-invalidation boundary, never at startup entry.
#[cfg(test)]
static STARTUP_SETTLEMENT_FAILURE: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum StartupSettlementFailureForTest {
    CustodyChanged,
    PersistenceTransitionFailed,
    UnknownStructured,
}

#[cfg(test)]
pub(crate) fn fail_next_startup_settlement_for_test(failure: StartupSettlementFailureForTest) {
    let encoded = match failure {
        StartupSettlementFailureForTest::CustodyChanged => 1,
        StartupSettlementFailureForTest::PersistenceTransitionFailed => 2,
        StartupSettlementFailureForTest::UnknownStructured => 3,
    };
    STARTUP_SETTLEMENT_FAILURE.store(encoded, Ordering::Release);
}

#[cfg(test)]
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

#[cfg(not(test))]
fn startup_settlement_failure_for_test() -> Option<DaemonError> {
    None
}

fn retained_unowned_diagnostic(sandbox_root: Option<&str>, reason: &'static str) {
    tracing::warn!(
        sandbox_root = sandbox_root.unwrap_or(""),
        reason,
        retained_unowned = true,
        "retained_unowned sandbox custody diagnostic"
    );
    #[cfg(test)]
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
pub(crate) enum CustodyHandle {
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
pub(crate) struct PreparedLaunch {
    custody: CustodyHandle,
}

impl PreparedLaunch {
    pub(crate) fn new(custody: CustodyHandle) -> Self {
        Self { custody }
    }

    pub(crate) fn custody(&self) -> &CustodyHandle {
        &self.custody
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectKind {
    ContextRead,
    ProviderLaunch,
    ProviderTurn,
    ToolExecution,
    BuildCacheReclaim,
}

/// Non-cloneable authorization for exactly one daemon-controlled effect.
pub(crate) struct CustodyEffectPermit {
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
pub(crate) struct CustodySettlementService {
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
pub(crate) struct CustodySettlementWorker {
    state: Arc<CustodySettlementState>,
    control: mpsc::Sender<SettlementControl>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CustodySettlementService {
    pub(crate) fn new(store: CustodyStore) -> Result<(Self, CustodySettlementWorker)> {
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

    #[cfg(test)]
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

    #[cfg(test)]
    pub(crate) async fn failed_count(&self) -> usize {
        self.producer.state.failed.lock().await.len()
    }

    #[cfg(test)]
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

    pub(crate) async fn shutdown(&self, producer: &CustodySettlementService) -> Result<()> {
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
    let store = Arc::clone(store);
    match tokio::task::spawn_blocking(move || {
        let mut store = store.blocking_lock();
        store.release_effect(reservation)
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err((error, settlement)),
        Err(error) => Err((DaemonError::Store(error.to_string()), settlement)),
    }
}

impl CustodyEffectPermit {
    pub(crate) fn effective_cwd(&self) -> &Path {
        &self.effective_cwd
    }

    pub(crate) fn cargo_target_dir(&self) -> Option<&Path> {
        self.cargo_target_dir.as_deref()
    }

    /// Scratch construction is authorized only by the ContextRead effect
    /// whose authenticated cwd it consumes.  Keep the effect kind private;
    /// callers receive only the exact predicate needed by that boundary.
    pub(crate) const fn is_context_read(&self) -> bool {
        matches!(self.kind, EffectKind::ContextRead)
    }

    #[cfg(test)]
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
pub(crate) enum CustodyClassification {
    OrdinaryUnsandboxed,
    RequiresPersistedAuthentication,
}

/// The only domain classifier allowed to treat an all-null tuple as ordinary.
/// Any partial or historical tuple is a refusal, never a canonical fallback.
#[derive(Debug, Default)]
pub(crate) struct CustodyService;

impl CustodyService {
    /// Classify every persisted session before restore rebuilds any runtime
    /// maps. This is deliberately a startup-only authentication pass: it
    /// performs no provider/context/tool effect and never invents a legacy
    /// owner from lineage or a canonical fallback from a malformed tuple.
    pub(crate) fn reconcile_startup(store: &mut Store, sandbox_base: &Path) -> Result<()> {
        const STARTUP_GROUP_BATCH: usize = 64;
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
        let mut after = None;
        loop {
            let groups = store.startup_custody_group_page(after.as_deref(), STARTUP_GROUP_BATCH)?;
            let Some(last) = groups.last().map(|group| group.key.clone()) else {
                break;
            };
            for group in groups {
                if group.custody_id.is_some() {
                    Self::reconcile_startup_existing_aggregate(store, sandbox_base, &group)?;
                } else {
                    let sessions = Self::bounded_legacy_group_sessions(store, &group)?;
                    match group.sandbox_root.as_deref() {
                        Some(root) => {
                            Self::reconcile_startup_root_group(store, sandbox_base, root, sessions)?
                        }
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
        Self::log_retained_unowned_diagnostics(store, sandbox_base)?;
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
            return Self::authenticate_startup_terminal_root(store, sandbox_base, &root);
        }
        let owner_id = root.owner_session_id.expect("checked live owner");
        let owner = store.get_session(owner_id)?.ok_or_else(|| {
            DaemonError::Store("live startup custody root owner is missing".into())
        })?;
        let authorization = match startup_settlement_failure_for_test() {
            Some(error) => Err(error),
            None => Self::authorize_live(
                &owner,
                store,
                sandbox_base,
                SandboxCustodyTransitionV1::StartupReconciliation,
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
    ) -> Result<()> {
        if let Some(root) = store.startup_custody_root_for_sandbox_root(sandbox_root)? {
            if root.state != "live" {
                return Self::authenticate_startup_terminal_root(store, sandbox_base, &root);
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
                None => Self::authorize_live(
                    &owner,
                    store,
                    sandbox_base,
                    SandboxCustodyTransitionV1::StartupReconciliation,
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
        match std::fs::symlink_metadata(root_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return store.publish_startup_terminal_root(root.custody_id);
            }
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            _ => {
                return store.quarantine_startup_terminal_root(
                    root.custody_id,
                    SandboxCustodyErrorCodeV1::RootIdentityMismatch,
                );
            }
        }
        let canonical_root = match std::fs::canonicalize(root_path) {
            Ok(path) if path == root_path => path,
            _ => {
                return store.quarantine_startup_terminal_root(
                    root.custody_id,
                    SandboxCustodyErrorCodeV1::RootIdentityMismatch,
                );
            }
        };
        let registered = git_output(&canonical_repo, ["worktree", "list", "--porcelain"]);
        let common = git_output(
            &canonical_root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .and_then(|path| std::fs::canonicalize(path).ok());
        let root_head = git_output(&canonical_root, ["rev-parse", "HEAD"]);
        let valid = tuple_matches
            && git_output(&canonical_root, ["rev-parse", "--show-toplevel"])
                .is_some_and(|top| PathBuf::from(top) == canonical_root)
            && git_output(&canonical_root, ["branch", "--show-current"]).as_deref()
                == Some(root.sandbox_branch.as_str())
            && registered.is_some_and(|listed| {
                root_head.as_deref().is_some_and(|head| {
                    has_matching_worktree_stanza_with_head(
                        &listed,
                        &canonical_root,
                        &root.sandbox_branch,
                        head,
                    )
                })
            })
            && common.is_some_and(|identity| {
                identity.to_string_lossy() == root.repository_identity
                    && std::fs::canonicalize(canonical_repo.join(".git"))
                        .is_ok_and(|repo_identity| repo_identity == identity)
            })
            && git_success(
                &canonical_root,
                ["merge-base", "--is-ancestor", &root.source_commit, "HEAD"],
            );
        if !valid {
            return store.quarantine_startup_terminal_root(
                root.custody_id,
                SandboxCustodyErrorCodeV1::WorktreeMismatch,
            );
        }
        store.publish_startup_terminal_root(root.custody_id)
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
    pub(crate) fn classify(session: &Session) -> Result<CustodyClassification> {
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

    pub(crate) fn authorize_ordinary(session: &Session) -> Result<CustodyHandle> {
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
    pub(crate) fn authorize_live(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
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
        let _root_guard = lock_custody_root(custody_id);
        Self::authorize_live_locked(session, store, sandbox_base, transition)
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
        let _root_guard = lock_custody_root(custody_id);
        Self::authorize_live_locked_with_policy(session, store, sandbox_base, transition, false)
    }

    fn authorize_live_locked(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
    ) -> Result<CustodyHandle> {
        Self::authorize_live_locked_with_policy(session, store, sandbox_base, transition, true)
    }

    fn authorize_live_locked_with_policy(
        session: &Session,
        store: &mut Store,
        sandbox_base: &Path,
        transition: SandboxCustodyTransitionV1,
        record_failure: bool,
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
        let mut failure = |code| {
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
        Self::authenticate_live_filesystem(session, &persisted, sandbox_base, failure)
    }

    fn authenticate_live_filesystem(
        session: &Session,
        persisted: &crate::store::sandbox_custody::PersistedCustody,
        sandbox_base: &Path,
        mut failure: impl FnMut(SandboxCustodyErrorCodeV1) -> DaemonError,
    ) -> Result<CustodyHandle> {
        let Some(session_root) = session.sandbox_root.as_ref() else {
            return Err(failure(SandboxCustodyErrorCodeV1::TupleIncomplete));
        };
        let Some(session_branch) = session.sandbox_branch.as_deref() else {
            return Err(failure(SandboxCustodyErrorCodeV1::TupleIncomplete));
        };
        if persisted.owner_session_id != session.id
            || persisted.canonical_repo_dir != session.working_dir.to_string_lossy()
            || persisted.sandbox_root != session_root.to_string_lossy()
            || persisted.sandbox_branch != session_branch
        {
            return Err(failure(SandboxCustodyErrorCodeV1::RootIdentityMismatch));
        }
        let base = std::fs::canonicalize(sandbox_base)
            .map_err(|_| failure(SandboxCustodyErrorCodeV1::RootOutsideBase))?;
        let root = std::fs::canonicalize(session_root)
            .map_err(|_| failure(SandboxCustodyErrorCodeV1::RootMissing))?;
        let allocation_root_name = persisted.allocation_id.to_string();
        if !root.starts_with(&base)
            || root.file_name().and_then(|name| name.to_str())
                != Some(allocation_root_name.as_str())
            || root != *session_root
            || persisted.sandbox_root != root.to_string_lossy()
        {
            return Err(failure(SandboxCustodyErrorCodeV1::RootOutsideBase));
        }
        let canonical = std::fs::canonicalize(&session.working_dir)
            .map_err(|_| failure(SandboxCustodyErrorCodeV1::RootMissing))?;
        if canonical != session.working_dir
            || persisted.canonical_repo_dir != canonical.to_string_lossy()
        {
            return Err(failure(SandboxCustodyErrorCodeV1::RootIdentityMismatch));
        }
        if !git_output(&root, ["rev-parse", "--show-toplevel"])
            .is_some_and(|top| PathBuf::from(top) == root)
            || git_output(&root, ["branch", "--show-current"]).as_deref() != Some(session_branch)
        {
            return Err(failure(SandboxCustodyErrorCodeV1::WorktreeMismatch));
        }
        let Some(common_dir) = git_output(
            &root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ) else {
            return Err(failure(SandboxCustodyErrorCodeV1::WorktreeMismatch));
        };
        if std::fs::canonicalize(common_dir)
            .ok()
            .map(|path| path.to_string_lossy().to_string())
            != Some(persisted.repository_identity.clone())
        {
            return Err(failure(SandboxCustodyErrorCodeV1::WorktreeMismatch));
        }
        if !git_is_ancestor(&root, &persisted.source_commit) {
            return Err(failure(
                SandboxCustodyErrorCodeV1::SourceRevisionUnavailable,
            ));
        }
        let Some(registered) = git_output(&canonical, ["worktree", "list", "--porcelain"]) else {
            return Err(failure(SandboxCustodyErrorCodeV1::WorktreeMismatch));
        };
        if !has_matching_worktree_stanza(&registered, &root, session_branch) {
            return Err(failure(SandboxCustodyErrorCodeV1::WorktreeMismatch));
        }
        Ok(CustodyHandle::Sandboxed(SandboxedCustody {
            session_id: session.id,
            custody_id: persisted.custody_id,
            owner_generation: persisted.generation,
        }))
    }

    /// Reserve then activate a persisted permit immediately before one effect.
    /// A preflight handle is deliberately insufficient: this repeats every
    /// persisted and filesystem/git check under the root-local admission lock.
    pub(crate) async fn begin_effect(
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
                let revalidated =
                    Self::authenticate_live_filesystem(session, &observed, sandbox_base, |code| {
                        failure_code = Some(code);
                        Self::refusal(code, Some(session.id), transition)
                    });
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
                let mut store = store_handle.lock().await;
                let Some(_root_guard) =
                    crate::store::sandbox_custody::try_lock_custody_root(sandboxed.custody_id)
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
    pub(crate) fn reclaim_terminal_target(
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
    pub(crate) fn reclaim_terminal_target_bytes_locked(
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

    pub(crate) fn reclaim_terminal_target_outcome_phased(
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
        let intent = match access.with_store(|store| {
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
                    identity.device,
                    identity.inode,
                )
                .map_err(|_| ReclaimSkipReason::CustodyOrGenerationDrift)
        }) {
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
                    || event.expected_device != identity.device
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
            expected_device: intent.expected_device,
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

    pub(crate) fn refusal(
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
