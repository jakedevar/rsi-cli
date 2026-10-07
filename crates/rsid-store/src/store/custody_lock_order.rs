//! The one global lock order for custody effects (Issue #606).
//!
//! Three kinds of lock can be held together by archive cleanup, cohort
//! settlement, archived-sandbox purge, adoption, retry and rotation:
//!
//! 1. **Store** (`Arc<tokio::sync::Mutex<Store>>`, the single SQLite writer),
//! 2. a **custody root lock** (`lock_custody_root`, keyed by exact custody id), and
//! 3. the **repository mutex** (`git_worktree::with_repository_mutation`, one
//!    per repository identity, so two roots of one repository share it).
//!
//! The blocking order is `Store -> custody root lock -> repository mutex`.
//! Effect admission, retry and rotation binds, `Store` methods that take a
//! root lock internally and adoption all acquire in that order.
//!
//! Maintenance passes (archive cleanup, cohort settlement, purge) do long
//! filesystem and Git proofs while holding a root lock and/or the repository
//! mutex, and they need the Store to persist their phase between steps.
//! They cannot hold the Store for the whole proof (that would pin every
//! unrelated RPC), so they cannot follow the order. Instead the rule is:
//!
//! * a thread that holds a root lock or the repository mutex never *blocks* on
//!   the Store: it uses [`BlockingStoreLockExt::blocking_lock_checked`], a
//!   bounded `try_lock` loop that fails with the typed, retryable
//!   `root_busy` custody error instead of waiting;
//! * a thread that already holds the repository mutex never blocks on a
//!   root lock: it uses [`lock_custody_root_under_repository`], bounded the
//!   same way; and where a pass can take the root lock first it does, so its
//!   blocking order is `root lock -> repository mutex`;
//! * async effect admission never waits on a root lock while holding the Store:
//!   [`lock_store_then_root`] and [`lock_store_then_session_root`] try the
//!   root lock, and when it is busy they drop the Store, wait for the root lock to
//!   quiesce, and retry. A paused maintenance proof therefore delays only the
//!   admission that needs its root, never an unrelated Store RPC.
//!
//! No blocking wait edge ever points from a root lock/repository-mutex holder to
//! the Store, or from a repository-mutex holder to a root lock, so no cycle
//! among the three lock kinds can form. Held-state is tracked per thread by
//! [`HeldScope`]: both guards are `!Send`, so a thread-local count is exact.

use super::Store;
use super::sandbox_custody::{CustodyRootGuard, lock_custody_root, try_lock_custody_root};
use crate::error::{DaemonError, Result, sandbox_custody_error};
use rsi_common::types::{
    SandboxCustodyErrorCodeV1, SandboxCustodyErrorV1, SandboxCustodyRecoveryV1,
    SandboxCustodyTransitionV1,
};
use std::cell::Cell;
use std::marker::PhantomData;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as StoreMutex, MutexGuard as StoreGuard};
use uuid::Uuid;

/// Upper bound a maintenance thread waits for the Store (or, under the
/// repository mutex, for a root lock) before surrendering with `root_busy`.
const BOUNDED_WAIT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Budget for effect admission (`CustodyService::begin_effect`) to win its
/// custody root lock. Brief contention on that root is waited out; a longer
/// hold (a purge's git push) surrenders with
/// the typed retryable `root_busy` so the scheduler re-arms the wake and keeps
/// serving other jobs instead of stalling behind the root lock (#1157).
pub(crate) const ADMISSION_WAIT: Duration = Duration::from_secs(5);

thread_local! {
    static STRIPE_HELD: Cell<usize> = const { Cell::new(0) };
    static REPOSITORY_HELD: Cell<usize> = const { Cell::new(0) };
    #[cfg(any(test, feature = "test-seam"))]
    static WAIT_OVERRIDE: Cell<Option<Duration>> = const { Cell::new(None) };
}

#[derive(Clone, Copy)]
enum HeldKind {
    Stripe,
    Repository,
}

fn counter(kind: HeldKind) -> &'static std::thread::LocalKey<Cell<usize>> {
    match kind {
        HeldKind::Stripe => &STRIPE_HELD,
        HeldKind::Repository => &REPOSITORY_HELD,
    }
}

/// Marks the current thread as holding a root lock or the repository mutex for
/// as long as it lives. `!Send`: it must drop on the thread that created it.
pub(crate) struct HeldScope {
    kind: HeldKind,
    _not_send: PhantomData<*const ()>,
}

impl HeldScope {
    pub(crate) fn stripe() -> Self {
        Self::enter(HeldKind::Stripe)
    }

    pub(crate) fn repository() -> Self {
        Self::enter(HeldKind::Repository)
    }

    fn enter(kind: HeldKind) -> Self {
        counter(kind).with(|count| count.set(count.get() + 1));
        Self {
            kind,
            _not_send: PhantomData,
        }
    }
}

impl Drop for HeldScope {
    fn drop(&mut self) {
        counter(self.kind).with(|count| count.set(count.get().saturating_sub(1)));
    }
}

fn holds_stripe_or_repository() -> bool {
    STRIPE_HELD.with(Cell::get) > 0 || REPOSITORY_HELD.with(Cell::get) > 0
}

fn holds_repository() -> bool {
    REPOSITORY_HELD.with(Cell::get) > 0
}

fn bounded_wait() -> Duration {
    #[cfg(any(test, feature = "test-seam"))]
    if let Some(wait) = WAIT_OVERRIDE.with(Cell::get) {
        return wait;
    }
    BOUNDED_WAIT
}

/// Shorten the bounded wait on this thread only (tests).
#[cfg(any(test, feature = "test-seam"))]
pub fn set_bounded_wait_for_test(wait: Option<Duration>) {
    WAIT_OVERRIDE.with(|slot| slot.set(wait));
}

/// The typed, retryable refusal a bounded acquisition surrenders with. The
/// caller's durable state is unchanged, so the pass is safe to retry.
pub(crate) fn lock_order_busy_error() -> DaemonError {
    sandbox_custody_error(SandboxCustodyErrorV1 {
        version: 1,
        code: SandboxCustodyErrorCodeV1::RootBusy,
        session_id: None,
        transition: SandboxCustodyTransitionV1::CleanupFailure,
        retryable: true,
        recovery: SandboxCustodyRecoveryV1::RetryAfterReconcile,
    })
}

/// Whether `error` is the retryable surrender from a bounded acquisition.
pub fn is_lock_order_busy_error(error: &DaemonError) -> bool {
    matches!(
        error,
        DaemonError::StructuredRpc { data, .. }
            if data.pointer("/error/code") == Some(&serde_json::json!("root_busy"))
    )
}

/// Blocking Store acquisition that is safe under a root lock or the repository
/// mutex. With neither held it is exactly `blocking_lock`; with either held
/// it is a bounded `try_lock` loop that fails with [`lock_order_busy_error`]
/// instead of risking a deadlock against a Store holder that is waiting for
/// the same root lock or repository.
pub trait BlockingStoreLockExt {
    fn blocking_lock_checked(&self) -> Result<StoreGuard<'_, Store>>;
}

impl BlockingStoreLockExt for StoreMutex<Store> {
    fn blocking_lock_checked(&self) -> Result<StoreGuard<'_, Store>> {
        if !holds_stripe_or_repository() {
            return Ok(self.blocking_lock());
        }
        let deadline = Instant::now() + bounded_wait();
        loop {
            if let Ok(guard) = self.try_lock() {
                return Ok(guard);
            }
            if Instant::now() >= deadline {
                return Err(lock_order_busy_error());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

/// Take a root lock from a thread that holds the repository mutex. The blocking
/// order is root lock-before-repository, so waiting here could deadlock against
/// an effect path holding the root lock and waiting for the repository: bound
/// the wait and surrender with [`lock_order_busy_error`].
pub fn lock_custody_root_under_repository(custody_id: Uuid) -> Result<CustodyRootGuard> {
    if !holds_repository() {
        return Ok(lock_custody_root(custody_id));
    }
    let deadline = Instant::now() + bounded_wait();
    loop {
        if let Some(guard) = try_lock_custody_root(custody_id) {
            return Ok(guard);
        }
        if Instant::now() >= deadline {
            return Err(lock_order_busy_error());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The effect-admission budget; shortened per thread in tests. Read before the
/// first await so the override binds on the polling thread.
pub fn admission_wait() -> Duration {
    #[cfg(any(test, feature = "test-seam"))]
    if let Some(wait) = WAIT_OVERRIDE.with(Cell::get) {
        return wait;
    }
    ADMISSION_WAIT
}

/// A contention observer: a predicate over the contended custody id and the
/// channel it signals.
#[cfg(any(test, feature = "test-seam"))]
type ContentionObserver = (
    Box<dyn Fn(Uuid) -> bool + Send>,
    std::sync::mpsc::Sender<()>,
);

/// Test handshake: registered observers, signalled each time an async
/// admission found a matching root lock busy, dropped the Store, and is about to
/// wait.
#[cfg(any(test, feature = "test-seam"))]
static ADMISSION_CONTENTION: std::sync::Mutex<Vec<ContentionObserver>> =
    std::sync::Mutex::new(Vec::new());

/// Ask to be told when an admission contends on `custody_id`'s root lock.
#[cfg(any(test, feature = "test-seam"))]
pub fn admission_contention_signal(custody_id: Uuid) -> std::sync::mpsc::Receiver<()> {
    admission_contention_signal_matching(move |contended| contended == custody_id)
}

/// Ask to be told when an admission contends on any custody id `matches`
/// accepts. The observer is dropped once its receiver is.
#[cfg(any(test, feature = "test-seam"))]
pub fn admission_contention_signal_matching(
    matches: impl Fn(Uuid) -> bool + Send + 'static,
) -> std::sync::mpsc::Receiver<()> {
    let (sender, receiver) = std::sync::mpsc::channel();
    ADMISSION_CONTENTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((Box::new(matches), sender));
    receiver
}

#[cfg(any(test, feature = "test-seam"))]
pub(crate) fn signal_admission_contention(custody_id: Uuid) {
    ADMISSION_CONTENTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|(matches, sender)| !matches(custody_id) || sender.send(()).is_ok());
}

/// Wait, without holding the Store, until the root lock is momentarily free.
async fn wait_root_quiescent(custody_id: Uuid) {
    let mut delay = POLL_INTERVAL;
    loop {
        if try_lock_custody_root(custody_id).is_some() {
            return;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(20));
    }
}

/// Acquire the Store and then the root lock for `custody_id` without ever
/// waiting for the root lock while holding the Store. A busy root lock drops the
/// Store, waits for the root lock to quiesce, and retries, so a paused
/// maintenance proof on the same (or a colliding) root lock cannot pin the Store.
pub async fn lock_store_then_root(
    store: &StoreMutex<Store>,
    custody_id: Uuid,
) -> (StoreGuard<'_, Store>, CustodyRootGuard) {
    loop {
        let guard = store.lock().await;
        match try_lock_custody_root(custody_id) {
            Some(root) => return (guard, root),
            None => drop(guard),
        }
        #[cfg(any(test, feature = "test-seam"))]
        signal_admission_contention(custody_id);
        wait_root_quiescent(custody_id).await;
    }
}

/// Wait, without holding the Store, until the root lock is momentarily free or
/// `deadline` passes. `false` means the deadline passed with the root lock busy.
async fn wait_root_quiescent_until(custody_id: Uuid, deadline: tokio::time::Instant) -> bool {
    let mut delay = POLL_INTERVAL;
    loop {
        if try_lock_custody_root(custody_id).is_some() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(delay.min(deadline - now)).await;
        delay = (delay * 2).min(Duration::from_millis(20));
    }
}

/// [`lock_store_then_root`] with a finite `budget` covering both the Store and
/// the root lock wait. `None` means the budget expired with the root lock (or the
/// Store) still busy; nothing is held and no durable state changed, so the
/// caller surrenders with a typed retryable `root_busy`.
pub async fn lock_store_then_root_within(
    store: &StoreMutex<Store>,
    custody_id: Uuid,
    budget: Duration,
) -> Option<(StoreGuard<'_, Store>, CustodyRootGuard)> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let guard = tokio::time::timeout_at(deadline, store.lock()).await.ok()?;
        match try_lock_custody_root(custody_id) {
            Some(root) => return Some((guard, root)),
            None => drop(guard),
        }
        #[cfg(any(test, feature = "test-seam"))]
        signal_admission_contention(custody_id);
        if !wait_root_quiescent_until(custody_id, deadline).await {
            return None;
        }
    }
}

/// The Store plus the root lock of `binding`'s custody root (none for an
/// `Ordinary` binding), acquired in the contract order without ever blocking a
/// runtime thread on the root lock: the launch path binds its session through a
/// Store method that needs the new root's root lock, and a long maintenance proof
/// on a colliding root lock must delay only that launch, never the Store (#1166).
pub async fn lock_store_then_binding_root<'a>(
    store: &'a StoreMutex<Store>,
    binding: &super::sandbox_custody::SessionCustodyBinding,
) -> (StoreGuard<'a, Store>, Option<CustodyRootGuard>) {
    lock_store_then_optional_root(store, binding.custody_id()).await
}

/// [`lock_store_then_root`] for a custody id that may be absent (an ordinary,
/// unsandboxed bind has no exclusive root): the Store alone when `None`. The
/// rotation, retry, restore and settlement paths know their root from the bind
/// or bound identity, and must not wait for its root lock while holding the Store
/// (#1172).
pub async fn lock_store_then_optional_root(
    store: &StoreMutex<Store>,
    custody_id: Option<Uuid>,
) -> (StoreGuard<'_, Store>, Option<CustodyRootGuard>) {
    match custody_id {
        Some(custody_id) => {
            let (guard, root) = lock_store_then_root(store, custody_id).await;
            (guard, Some(root))
        }
        None => (store.lock().await, None),
    }
}

/// [`lock_store_then_root`] for callers that learn the custody id from the
/// Store (`live_custody_for_session`). A session with no live custody returns
/// the Store alone: the caller's authorization then refuses it as before.
pub async fn lock_store_then_session_root(
    store: &StoreMutex<Store>,
    session_id: Uuid,
) -> (StoreGuard<'_, Store>, Option<CustodyRootGuard>) {
    lock_store_then_resolved_root(store, |guard| {
        guard
            .live_custody_for_session(session_id)
            .ok()
            .map(|custody| custody.custody_id)
    })
    .await
}

/// [`lock_store_then_root`] for callers that learn the custody id from the
/// Store through `resolve`, which runs under the Store guard on every attempt
/// (a busy root lock drops the Store, so the answer is re-read). `None` returns the
/// Store alone: the caller's own fence then refuses as before.
pub async fn lock_store_then_resolved_root<F>(
    store: &StoreMutex<Store>,
    resolve: F,
) -> (StoreGuard<'_, Store>, Option<CustodyRootGuard>)
where
    F: Fn(&Store) -> Option<Uuid>,
{
    loop {
        let guard = store.lock().await;
        let Some(custody_id) = resolve(&guard) else {
            return (guard, None);
        };
        match try_lock_custody_root(custody_id) {
            Some(root) => return (guard, Some(root)),
            None => drop(guard),
        }
        #[cfg(any(test, feature = "test-seam"))]
        signal_admission_contention(custody_id);
        wait_root_quiescent(custody_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::git_worktree::with_repository_mutation;
    use std::sync::Arc;
    use std::sync::mpsc;

    const WATCHDOG: Duration = Duration::from_secs(30);

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Arc<StoreMutex<Store>>,
        repository: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("fixture dir");
        let repository = dir.path().join("repository");
        std::fs::create_dir(&repository).expect("repository dir");
        let status = std::process::Command::new("git")
            .current_dir(&repository)
            .args(["init", "-q", "-b", "main"])
            .status()
            .expect("git init");
        assert!(status.success());
        let store = Store::open(&dir.path().join("lock-order.db")).expect("store");
        Fixture {
            _dir: dir,
            store: Arc::new(StoreMutex::new(store)),
            repository,
        }
    }

    /// A custody id in a different registry bucket than `other`.
    fn id_on_other_stripe(other: Uuid) -> Uuid {
        let shard = crate::store::sandbox_custody::custody_root_lock_shard(other);
        loop {
            let candidate = Uuid::new_v4();
            if crate::store::sandbox_custody::custody_root_lock_shard(candidate) != shard {
                return candidate;
            }
        }
    }

    /// A distinct custody id that collides with `other` on one of the 64 registry buckets.
    fn id_colliding_with(other: Uuid) -> Uuid {
        let shard = crate::store::sandbox_custody::custody_root_lock_shard(other);
        loop {
            let candidate = Uuid::new_v4();
            if candidate != other
                && crate::store::sandbox_custody::custody_root_lock_shard(candidate) == shard
            {
                return candidate;
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn colliding_root_ids_lock_independently_and_release_on_unwind() {
        let first = Uuid::new_v4();
        let second = id_colliding_with(first);
        let first_guard = lock_custody_root(first);
        let second_guard = try_lock_custody_root(second).expect("another root is independent");
        assert!(try_lock_custody_root(first).is_none());
        assert!(try_lock_custody_root(second).is_none());
        drop(second_guard);
        std::thread::spawn(move || {
            let _guard = lock_custody_root(second);
            panic!("effect fault");
        })
        .join()
        .expect_err("injected panic");
        let _second_guard = try_lock_custody_root(second).expect("unwind releases the root");
        assert!(try_lock_custody_root(first).is_none());
        drop(first_guard);
        let _first_guard = try_lock_custody_root(first).expect("drop releases the root");
    }

    fn finish<T: Send + 'static>(
        name: &str,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> std::thread::JoinHandle<T> {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(work)
            .expect("spawn")
    }

    fn join_within<T>(handle: std::thread::JoinHandle<T>, what: &str) -> T {
        let deadline = Instant::now() + WATCHDOG;
        while !handle.is_finished() {
            assert!(Instant::now() < deadline, "{what} deadlocked");
            std::thread::sleep(Duration::from_millis(2));
        }
        handle.join().expect("thread join")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn bounded_store_wait_surrenders_under_stripe_or_repository_and_blocks_otherwise() {
        let f = fixture();
        set_bounded_wait_for_test(Some(Duration::from_millis(60)));
        let custody_id = Uuid::new_v4();

        // Another thread pins the Store.
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let pin_store = Arc::clone(&f.store);
        let pinner = finish("pin-store", move || {
            let guard = pin_store.blocking_lock();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(guard);
        });
        held_rx.recv_timeout(WATCHDOG).expect("store pinned");

        // Holding a root lock: the Store wait is bounded and typed retryable.
        {
            let _root = lock_custody_root(custody_id);
            let error = f.store.blocking_lock_checked().err().expect("surrenders");
            assert!(is_lock_order_busy_error(&error), "{error}");
        }
        // Holding the repository mutex: same.
        with_repository_mutation(&f.repository, || {
            let error = f.store.blocking_lock_checked().err().expect("surrenders");
            assert!(is_lock_order_busy_error(&error), "{error}");
            Ok(())
        })
        .expect("repository mutation");
        // A held root lock elsewhere must not be mistaken for ours after release.
        assert!(!holds_stripe_or_repository());

        // Holding neither, the wait is the ordinary blocking wait: it outlasts
        // the bound and still succeeds once the Store frees up.
        set_bounded_wait_for_test(None);
        let waiter_store = Arc::clone(&f.store);
        let waiter = finish("blocking-waiter", move || {
            set_bounded_wait_for_test(Some(Duration::from_millis(10)));
            waiter_store.blocking_lock_checked().map(|_| ()).is_ok()
        });
        std::thread::sleep(Duration::from_millis(120));
        release_tx.send(()).unwrap();
        join_within(pinner, "store pinner");
        assert!(join_within(waiter, "unheld Store waiter"));
        set_bounded_wait_for_test(None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn repository_holder_surrenders_a_busy_stripe_instead_of_waiting() {
        let f = fixture();
        set_bounded_wait_for_test(Some(Duration::from_millis(60)));
        let custody_id = Uuid::new_v4();
        let _root = lock_custody_root(custody_id);
        let repository = f.repository.clone();
        let outcome = finish("repository-stripe", move || {
            set_bounded_wait_for_test(Some(Duration::from_millis(60)));
            with_repository_mutation(&repository, || {
                Ok(lock_custody_root_under_repository(custody_id).err())
            })
            .expect("repository mutation")
        });
        let error = join_within(outcome, "repository holder").expect("busy stripe is refused");
        assert!(is_lock_order_busy_error(&error), "{error}");
        set_bounded_wait_for_test(None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn paused_stripe_holder_never_pins_store_for_contended_admission() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        for _ in 0..2 {
            let f = fixture();
            let proof_custody = Uuid::new_v4();
            let admission_custody = proof_custody;
            // The "proof": a maintenance pass paused while holding its root lock.
            let (held_tx, held_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let proof = finish("paused-proof", move || {
                let _root = lock_custody_root(proof_custody);
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            held_rx
                .recv_timeout(WATCHDOG)
                .expect("proof holds its stripe");

            // A contended effect admission on the same shard; the handshake
            // proves it reached the busy root lock (and let go of the Store).
            let contended = admission_contention_signal(admission_custody);
            let store = Arc::clone(&f.store);
            let mut admission = runtime.spawn(async move {
                let (_store, _root) = lock_store_then_root(&store, admission_custody).await;
            });
            contended
                .recv_timeout(WATCHDOG)
                .expect("admission reached the busy stripe ");
            assert!(
                !admission.is_finished(),
                "admission waits for the held stripe"
            );
            runtime.block_on(async {
                // An unrelated Store RPC completes while admission is queued.
                for _ in 0..5 {
                    let store = Arc::clone(&f.store);
                    tokio::time::timeout(Duration::from_secs(5), async move {
                        store
                            .lock()
                            .await
                            .get_session(Uuid::new_v4())
                            .expect("query")
                    })
                    .await
                    .expect("unrelated Store RPC is not pinned by the queued admission");
                }
            });

            // Release the proof: admission settles; nothing deadlocked.
            release_tx.send(()).unwrap();
            join_within(proof, "paused proof");
            runtime
                .block_on(async { tokio::time::timeout(WATCHDOG, admission).await })
                .expect("admission settles after the proof releases")
                .expect("admission task");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn two_roots_of_one_repository_settlement_vs_effect_admission_cannot_deadlock() {
        // Effect path: Store -> root lock(A) -> repository mutex. Settlement
        // shape: repository mutex -> root lock(A) / Store. Before the global
        // order each waited on the other's lock; now settlement surrenders.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        for settle_waits_on in ["stripe", "store"] {
            let f = fixture();
            let root_a = Uuid::new_v4();
            let root_b = id_on_other_stripe(root_a);
            let (effect_ready_tx, effect_ready_rx) = mpsc::channel();
            let (settle_in_repo_tx, settle_in_repo_rx) = mpsc::channel();

            let store = Arc::clone(&f.store);
            let repository = f.repository.clone();
            let handle = runtime.handle().clone();
            let effect = finish("effect-admission", move || {
                handle.block_on(async {
                    let (_store, _root) = lock_store_then_root(&store, root_a).await;
                    effect_ready_tx.send(()).unwrap();
                    settle_in_repo_rx
                        .recv_timeout(WATCHDOG)
                        .expect("settlement entered");
                    // Store+root lock held; now needs the repository mutex.
                    with_repository_mutation(&repository, || Ok(())).expect("repository");
                });
            });
            effect_ready_rx
                .recv_timeout(WATCHDOG)
                .expect("effect holds Store and stripe(A)");

            let store = Arc::clone(&f.store);
            let repository = f.repository.clone();
            let wait_on = settle_waits_on;
            let settlement = finish("settlement", move || {
                set_bounded_wait_for_test(Some(Duration::from_millis(80)));
                with_repository_mutation(&repository, || {
                    settle_in_repo_tx.send(()).unwrap();
                    Ok(match wait_on {
                        "stripe" => lock_custody_root_under_repository(root_a).err(),
                        _ => {
                            let _root_b = lock_custody_root_under_repository(root_b)?;
                            store.blocking_lock_checked().err()
                        }
                    })
                })
                .expect("repository mutation")
            });
            let surrendered = join_within(settlement, "settlement").expect("settlement surrenders");
            assert!(is_lock_order_busy_error(&surrendered), "{surrendered}");
            join_within(effect, "effect admission");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn two_roots_of_one_repository_cleanup_vs_purge_cannot_deadlock() {
        // Cleanup holds root lock(A) then the repository mutex, and needs the
        // Store; purge holds root lock(B) and then needs the same repository
        // mutex. Another thread pins the Store meanwhile. Every wait is
        // bounded or ordered, so all three finish.
        let f = fixture();
        let root_a = Uuid::new_v4();
        let root_b = id_on_other_stripe(root_a);
        let (cleanup_in_repo_tx, cleanup_in_repo_rx) = mpsc::channel();
        let (release_store_tx, release_store_rx) = mpsc::channel::<()>();
        let (pinned_tx, pinned_rx) = mpsc::channel();

        let pin_store = Arc::clone(&f.store);
        let pinner = finish("store-pinner", move || {
            let guard = pin_store.blocking_lock();
            pinned_tx.send(()).unwrap();
            release_store_rx.recv().unwrap();
            drop(guard);
        });
        pinned_rx.recv_timeout(WATCHDOG).expect("store pinned");

        let store = Arc::clone(&f.store);
        let repository = f.repository.clone();
        let cleanup = finish("cleanup", move || {
            set_bounded_wait_for_test(Some(Duration::from_secs(20)));
            let _root = lock_custody_root(root_a);
            with_repository_mutation(&repository, || {
                cleanup_in_repo_tx.send(()).unwrap();
                // Waits (bounded) for the pinned Store, then proceeds.
                let _store = store.blocking_lock_checked()?;
                Ok(())
            })
        });
        cleanup_in_repo_rx
            .recv_timeout(WATCHDOG)
            .expect("cleanup in repository");

        let repository = f.repository.clone();
        let purge = finish("purge", move || {
            let _root = lock_custody_root(root_b);
            // Blocks on the repository mutex held by cleanup: root lock(B) ->
            // repository is the ordered direction, so this waits, not cycles.
            with_repository_mutation(&repository, || Ok(()))
        });
        std::thread::sleep(Duration::from_millis(100));
        release_store_tx.send(()).unwrap();
        join_within(pinner, "store pinner");
        join_within(cleanup, "cleanup").expect("cleanup settles after the Store frees");
        join_within(purge, "purge").expect("purge settles after cleanup leaves the repository");
    }
}
