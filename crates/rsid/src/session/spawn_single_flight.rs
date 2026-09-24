//! Per-session single-flight spawn guard.
//!
//! Makes it architecturally impossible for two *tracked* provider processes to
//! exist for one session id. Every provider-spawn entry point acquires a
//! per-session guard across its `check -> client.launch() -> active.insert`
//! span. The first caller spawns; a racing caller for the same id blocks on the
//! per-session lock and, on acquiring, adopts the now-live child (no-op success)
//! instead of spawning a twin.
//!
//! The bare tracker identifier for this work is deliberately kept out of every
//! symbol, comment, and event string here because it collides with an unrelated
//! already-landed feature; this work is named descriptively instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Per-session single-flight spawn lock map. Global (single-process daemon;
/// cross-process locking is a non-goal), mirroring `SPAWN_IDEMPOTENCY`
/// (`agent_verbs.rs`). The inner `tokio::sync::Mutex<()>` is held across each
/// spawn site's check -> launch -> active-insert span so a racing caller blocks
/// and then adopts the live child — and NO further. A site that then runs
/// `monitor_session` inline on the same task (the two rotation sites in
/// `rotation.rs`) MUST `drop` the guard right after the insert, before that
/// await: the monitor can re-enter `acquire_spawn_guard` for the same id and
/// this `Mutex` is non-reentrant, so holding across it self-deadlocks.
/// `continue_session`/`launch_session` instead `tokio::spawn` their monitor, so
/// their guard releases on fn return. The outer std `Mutex` is held only to
/// get/insert the per-session lock and to clean up on drop — never across an
/// `.await`.
static SPAWN_SINGLE_FLIGHT: Mutex<Option<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>> =
    Mutex::new(None);

/// Daemon-wide provider-cwd admission barrier. Provider launches take a shared
/// guard before any cwd-dependent work and hold it through durable/active
/// publication. Source-worktree settlement takes the exclusive guard across
/// its final runtime-alias recheck and Git effects. This deliberately favors a
/// small, auditable safety boundary over per-path lock-map complexity.
static PROVIDER_CWD_ADMISSION: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

pub(super) type ProviderCwdAdmissionGuard = tokio::sync::RwLockReadGuard<'static, ()>;

pub(super) async fn acquire_provider_cwd_admission() -> ProviderCwdAdmissionGuard {
    PROVIDER_CWD_ADMISSION.read().await
}

pub(super) async fn acquire_settlement_cwd_exclusion() -> tokio::sync::RwLockWriteGuard<'static, ()>
{
    PROVIDER_CWD_ADMISSION.write().await
}

/// RAII guard held across a single provider-spawn span. Dropping it releases the
/// per-session lock (unblocking any waiter) and reaps the map entry when no
/// other waiter/holder remains.
pub(super) struct SpawnGuard {
    session_id: Uuid,
    contended: bool,
    inner: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl SpawnGuard {
    /// True iff this caller had to wait for another in-flight spawn of the same
    /// id. Only a contended caller runs the adopt check (a non-contended caller
    /// is the sole/first spawner and preserves existing single-resume behavior).
    pub(super) const fn contended(&self) -> bool {
        self.contended
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        // Release the inner lock BEFORE inspecting strong_count.
        self.inner.take();
        let mut g = SPAWN_SINGLE_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(map) = g.as_mut()
            && let Some(lock) = map.get(&self.session_id)
            // strong_count == 1 => only the map holds it => no other waiter/holder.
            // Checked under the outer lock, so no one can clone it concurrently.
            && Arc::strong_count(lock) == 1
        {
            map.remove(&self.session_id);
        }
    }
}

/// Acquire the per-session single-flight spawn guard. Held across the caller's
/// check -> launch -> insert span. `contended()` reports whether the lock was
/// already held (i.e. this caller raced an in-flight spawn).
pub(super) async fn acquire_spawn_guard(session_id: Uuid) -> SpawnGuard {
    let lock = {
        let mut g = SPAWN_SINGLE_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let map = g.get_or_insert_with(HashMap::new);
        Arc::clone(
            map.entry(session_id)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    };
    // Contended iff someone holds it right now. `try_lock_owned` failing is a
    // genuine-contention signal (no false negatives); a rare false positive is
    // harmless (the adopt check then simply finds no live child and spawns).
    if let Ok(inner) = Arc::clone(&lock).try_lock_owned() {
        SpawnGuard {
            session_id,
            contended: false,
            inner: Some(inner),
        }
    } else {
        let inner = lock.lock_owned().await;
        SpawnGuard {
            session_id,
            contended: true,
            inner: Some(inner),
        }
    }
}

impl SpawnGuard {
    /// The session this guard serializes.
    pub(crate) const fn session_id(&self) -> Uuid {
        self.session_id
    }
}

/// The one global lock order for holding more than one spawn guard: ascending
/// session id, each id once. Every multi-guard holder (rotation publication,
/// lead mutation, successor commit) uses it, so two holders can never wait on
/// each other in a cycle.
fn sorted_unique(ids: &[Uuid]) -> Vec<Uuid> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Blocking acquisition of several spawn guards in the global lock order.
pub(super) async fn acquire_spawn_guards_sorted(ids: &[Uuid]) -> Vec<SpawnGuard> {
    let mut guards = Vec::new();
    for id in sorted_unique(ids) {
        guards.push(acquire_spawn_guard(id).await);
    }
    guards
}

/// Non-blocking variant: all guards in the global order, or none.
pub(super) fn try_acquire_spawn_guards_sorted(ids: &[Uuid]) -> Option<Vec<SpawnGuard>> {
    let mut guards = Vec::new();
    for id in sorted_unique(ids) {
        guards.push(try_acquire_spawn_guard(id)?);
    }
    Some(guards)
}

/// RPC-1 C1 precondition witness: the spawn guards of the rotation
/// predecessor and its successor, held in the global lock order. The store
/// publication accepts only this witness, so no continuation that passed its
/// fence check under guard(P) or guard(S) can straddle the publication commit.
pub struct RotationPublicationGuards {
    predecessor: Uuid,
    successor: Uuid,
    _guards: Vec<SpawnGuard>,
}

impl RotationPublicationGuards {
    pub async fn acquire(predecessor: Uuid, successor: Uuid) -> Self {
        Self {
            predecessor,
            successor,
            _guards: acquire_spawn_guards_sorted(&[predecessor, successor]).await,
        }
    }

    pub const fn predecessor(&self) -> Uuid {
        self.predecessor
    }

    pub const fn successor(&self) -> Uuid {
        self.successor
    }
}

/// Non-blocking ownership for bounded cleanup reconciliation. A live deferred
/// launch keeps its guard, so cleanup must not wait on or race that owner.
pub(super) fn try_acquire_spawn_guard(session_id: Uuid) -> Option<SpawnGuard> {
    let mut map = SPAWN_SINGLE_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let lock = map
        .get_or_insert_with(HashMap::new)
        .entry(session_id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let inner = lock.try_lock_owned().ok()?;
    Some(SpawnGuard {
        session_id,
        contended: false,
        inner: Some(inner),
    })
}

/// True iff a *live* tracked child already exists for `session_id`. Gates on
/// `is_alive()` (NOT mere map membership) so a finalized zombie whose handle was
/// nulled on finalize (`process: None`) is NOT adopted — a contended caller then
/// falls through and spawns fresh.
pub(super) async fn adopt_if_live(
    active: &tokio::sync::RwLock<std::collections::HashMap<Uuid, super::types::TrackedSession>>,
    session_id: Uuid,
) -> bool {
    let mut guard = active.write().await;
    match guard.get_mut(&session_id) {
        Some(t) => t
            .process
            .as_mut()
            .is_some_and(super::types::ProviderProcess::is_alive),
        None => false,
    }
}
