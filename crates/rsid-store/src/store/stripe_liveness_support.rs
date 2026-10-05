//! Test support for #1172: a custody path must never hold the Store while it
//! waits for a custody stripe. A maintenance proof (purge, archive cleanup,
//! settlement) can hold a stripe for seconds; every other Store user, the
//! daemon's RPC handlers and the watchdog's Store probe included, must keep
//! being served meanwhile, and the path itself must finish once the proof ends.
//!
//! [`run_behind_held_stripes`] starts a Store probe on a plain OS thread (so a
//! starved runtime cannot hide an unanswered Store, and the probe is ready
//! before the path starts), holds the stripes on another plain thread (a
//! readiness handshake returns only once they are all held), then runs the path
//! under test. The holder keeps the stripes until the path *acknowledges
//! contention* (the `custody_lock_order` seam: it found a held stripe, dropped
//! the Store and began to wait) and then for the full `hold` interval after
//! that acknowledgement. The probe only counts samples taken inside that
//! interval, so a path that spends the hold elsewhere (a slow sandbox
//! allocation), or never needs a stripe at all, or blocks the runtime thread
//! under the Store instead of acknowledging, fails instead of passing
//! vacuously (#1174, #1179).

use super::Store;
use super::custody_lock_order::admission_contention_signal_matching;
use super::sandbox_custody::{custody_root_lock_shard, lock_custody_root};
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as StoreMutex;
use uuid::Uuid;

/// Worst acceptable wait for an unrelated Store acquisition while the path is
/// blocked on a stripe. Far below the watchdog's 5 s probe deadline and the
/// 3 s hold, and far above scheduling noise on a loaded runner.
pub const PROBE_BOUND: Duration = Duration::from_millis(1500);

/// Fewest probe samples taken inside the post-acknowledgement hold that make
/// the run meaningful (a 3 s hold at one probe per 5 ms is ~600).
pub const MIN_PROBE_SAMPLES: usize = 20;

/// How long the holder waits for the path to acknowledge contention before it
/// gives up and releases (the run then reports no acknowledgement). Long
/// enough for a slow sandbox allocation ahead of the contended transition.
pub const ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// What a held-stripe run observed.
pub struct StripeRun<T> {
    /// The path under test's own result.
    pub output: T,
    /// Whether the path found a held stripe, dropped the Store and waited (the
    /// contention handshake). Without it the run proves nothing.
    pub contention_acknowledged: bool,
    /// The worst Store wait an unrelated probe saw inside the hold that
    /// follows the acknowledgement.
    pub worst_wait: Duration,
    /// How many probe samples were taken inside that hold.
    pub samples_while_held: usize,
    /// Whether the path finished only after the stripes were released, i.e. it
    /// really waited behind them.
    pub finished_after_release: bool,
    /// What each extra caller-supplied probe (an outer registry the path might
    /// pin) saw inside the same hold.
    pub extra_probes: Vec<ProbeReport>,
}

/// One probe's observation inside the post-acknowledgement hold.
pub struct ProbeReport {
    pub name: &'static str,
    pub worst_wait: Duration,
    pub samples: usize,
}

/// An extra probe: `touch` acquires and releases the resource under test (an
/// active-session registry, say) on a plain thread; the wait is what is timed.
pub type ProbeFn = Box<dyn Fn() + Send + 'static>;

impl<T> StripeRun<T> {
    /// Assert the path really contended for a held stripe, the Store stayed
    /// answerable meanwhile and the path waited behind the stripes, then hand
    /// back the path's result.
    #[track_caller]
    pub fn assert_store_stayed_free(self, what: &str) -> T {
        assert!(
            self.contention_acknowledged,
            "{what}: the path never acknowledged contention on a held stripe, so the run \
             proves nothing about it"
        );
        assert!(
            self.samples_while_held >= MIN_PROBE_SAMPLES,
            "{what}: the Store probe took only {} samples while the stripes were held",
            self.samples_while_held
        );
        assert!(
            self.worst_wait < PROBE_BOUND,
            "{what}: an unrelated Store user waited {:?} behind a path blocked on a custody stripe",
            self.worst_wait
        );
        assert!(
            self.finished_after_release,
            "{what}: the path finished before the stripes were released, so it never waited on one"
        );
        for probe in &self.extra_probes {
            assert!(
                probe.samples >= MIN_PROBE_SAMPLES,
                "{what}: the {} probe took only {} samples while the stripes were held",
                probe.name,
                probe.samples
            );
            assert!(
                probe.worst_wait < PROBE_BOUND,
                "{what}: an unrelated {} user waited {:?} behind a path blocked on a custody stripe",
                probe.name,
                probe.worst_wait
            );
        }
        self.output
    }
}

/// `n` custody ids that cover every stripe, one per shard.
fn one_id_per_stripe() -> Vec<Uuid> {
    let mut ids: Vec<Option<Uuid>> = vec![None; 64];
    while ids.iter().any(Option::is_none) {
        let id = Uuid::new_v4();
        ids[custody_root_lock_shard(id)].get_or_insert(id);
    }
    ids.into_iter().flatten().collect()
}

/// Flags shared by the holder, the probe and the runner.
#[derive(Default)]
struct Shared {
    /// The path acknowledged contention; the hold interval is running.
    acknowledged: AtomicBool,
    /// Set before the stripes drop: a path that finishes while this is still
    /// false never waited on a held stripe.
    released: AtomicBool,
    /// The path returned; a holder still waiting for an acknowledgement stops.
    path_done: AtomicBool,
    /// The probe should stop sampling.
    stop: AtomicBool,
}

struct Holder {
    thread: std::thread::JoinHandle<()>,
}

/// Hold the stripes of `ids` on a plain thread: from before the path starts
/// until `hold` after it acknowledges contention on one of them (or until the
/// path ends / `ack_timeout` passes without an acknowledgement). Returns once
/// all are held (the readiness handshake).
fn hold_stripes(
    ids: Vec<Uuid>,
    hold: Duration,
    ack_timeout: Duration,
    contended: std::sync::mpsc::Receiver<()>,
    shared: &Arc<Shared>,
) -> Holder {
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let shared = Arc::clone(shared);
    let thread = std::thread::spawn(move || {
        // Distinct ids can share a stripe; take each stripe once.
        let mut seen = BTreeSet::new();
        let guards: Vec<_> = ids
            .into_iter()
            .filter(|id| seen.insert(custody_root_lock_shard(*id)))
            .map(lock_custody_root)
            .collect();
        held_tx.send(()).expect("the test is waiting for readiness");
        let deadline = Instant::now() + ack_timeout;
        let acknowledged = loop {
            match contended.recv_timeout(Duration::from_millis(5)) {
                Ok(()) => break true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if shared.path_done.load(Ordering::SeqCst) || Instant::now() >= deadline {
                        break false;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break false,
            }
        };
        if acknowledged {
            shared.acknowledged.store(true, Ordering::SeqCst);
            std::thread::sleep(hold);
        }
        shared.released.store(true, Ordering::SeqCst);
        drop(guards);
    });
    held_rx.recv().expect("the holder thread reached readiness");
    Holder { thread }
}

/// A probe on a plain thread, so it needs no runtime worker and a path that
/// starves the runtime cannot hide an unanswered resource. `touch` acquires and
/// releases the resource; the wait is timed. The report holds the worst wait and
/// the sample count, both taken only while the post-acknowledgement hold ran;
/// the receiver fires after the first sample (probe readiness).
fn start_probe(
    name: &'static str,
    touch: ProbeFn,
    shared: &Arc<Shared>,
) -> (
    std::thread::JoinHandle<ProbeReport>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let shared = Arc::clone(shared);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let thread = std::thread::spawn(move || {
        let mut ready = Some(ready_tx);
        let mut worst_wait = Duration::ZERO;
        let mut samples = 0;
        while !shared.stop.load(Ordering::Acquire) {
            let held_at_start = shared.acknowledged.load(Ordering::SeqCst)
                && !shared.released.load(Ordering::SeqCst);
            let started = Instant::now();
            touch();
            let waited = started.elapsed();
            if held_at_start {
                worst_wait = worst_wait.max(waited);
                samples += 1;
            }
            if let Some(ready) = ready.take() {
                let _ = ready.send(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        ProbeReport {
            name,
            worst_wait,
            samples,
        }
    });
    (thread, ready_rx)
}

/// Run `path` while `ids`' stripes (every stripe when `None`) are held until
/// `hold` after the path acknowledges contention, with an unrelated Store probe
/// running meanwhile. Call from a multi-thread runtime with at least two
/// workers.
pub async fn run_behind_held_stripes<T, F>(
    store: &Arc<StoreMutex<Store>>,
    ids: Option<Vec<Uuid>>,
    hold: Duration,
    path: F,
) -> StripeRun<T>
where
    F: Future<Output = T>,
{
    run_behind_held_stripes_within(store, ids, hold, ACKNOWLEDGEMENT_TIMEOUT, Vec::new(), path)
        .await
}

/// [`run_behind_held_stripes`] with an explicit acknowledgement budget and
/// extra probes (named resources an outer guard in the path might pin).
pub async fn run_behind_held_stripes_within<T, F>(
    store: &Arc<StoreMutex<Store>>,
    ids: Option<Vec<Uuid>>,
    hold: Duration,
    ack_timeout: Duration,
    extra_probes: Vec<(&'static str, ProbeFn)>,
    path: F,
) -> StripeRun<T>
where
    F: Future<Output = T>,
{
    let ids = ids.unwrap_or_else(one_id_per_stripe);
    // Registered before the stripes are held or the path starts, so the
    // acknowledgement cannot be missed. A busy stripe is contention for every
    // custody id that hashes to it.
    let shards: BTreeSet<usize> = ids.iter().copied().map(custody_root_lock_shard).collect();
    let contended = admission_contention_signal_matching(move |custody_id| {
        shards.contains(&custody_root_lock_shard(custody_id))
    });
    let shared = Arc::new(Shared::default());
    // Probe readiness first: the path never starts before every probe sampled.
    let store_probe = Arc::clone(store);
    let mut probes = vec![start_probe(
        "Store",
        Box::new(move || drop(store_probe.blocking_lock())),
        &shared,
    )];
    probes.extend(
        extra_probes
            .into_iter()
            .map(|(name, touch)| start_probe(name, touch, &shared)),
    );
    let mut threads = Vec::new();
    for (thread, ready) in probes {
        ready.await.expect("the probe took a sample");
        threads.push(thread);
    }
    let holder = hold_stripes(ids, hold, ack_timeout, contended, &shared);
    let output = tokio::time::timeout(Duration::from_secs(60), path)
        .await
        .expect("the path under test finished instead of hanging");
    shared.path_done.store(true, Ordering::SeqCst);
    let finished_after_release = shared.released.load(Ordering::SeqCst);
    let contention_acknowledged = shared.acknowledged.load(Ordering::SeqCst);
    holder.thread.join().expect("the stripe holder joined");
    shared.stop.store(true, Ordering::Release);
    let mut reports = threads
        .into_iter()
        .map(|thread| thread.join().expect("a probe joined"));
    let store_report = reports.next().expect("the Store probe report");
    StripeRun {
        output,
        contention_acknowledged,
        worst_wait: store_report.worst_wait,
        samples_while_held: store_report.samples,
        finished_after_release,
        extra_probes: reports.collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::custody_lock_order::{lock_store_then_root, signal_admission_contention};

    fn store() -> (tempfile::TempDir, Arc<StoreMutex<Store>>) {
        let dir = tempfile::tempdir().expect("store dir");
        let store = Store::open(&dir.path().join("liveness-support.db")).expect("store");
        (dir, Arc::new(StoreMutex::new(store)))
    }

    /// A path that never needs a stripe cannot pass: no acknowledgement, so the
    /// assertion refuses it, however fast it finishes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_path_that_never_contends_is_refused() {
        let (_dir, store) = store();
        let run = run_behind_held_stripes_within(
            &store,
            Some(vec![Uuid::new_v4()]),
            Duration::from_millis(200),
            Duration::from_secs(1),
            Vec::new(),
            async {},
        )
        .await;
        assert!(!run.contention_acknowledged);
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run.assert_store_stayed_free("no contention");
        }))
        .expect_err("a run without contention must fail");
        let message = refused
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            message.contains("never acknowledged contention"),
            "{message}"
        );
    }

    /// The old blocking transition: the path blocks a runtime thread on the
    /// stripe under the Store and never acknowledges. It is refused (after the
    /// acknowledgement budget releases the holder), not passed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_path_that_blocks_on_the_stripe_without_acknowledging_is_refused() {
        let (_dir, store) = store();
        let custody_id = Uuid::new_v4();
        let blocked_store = Arc::clone(&store);
        let run = run_behind_held_stripes_within(
            &store,
            Some(vec![custody_id]),
            Duration::from_millis(200),
            Duration::from_millis(500),
            Vec::new(),
            async move {
                let _store = blocked_store.lock().await;
                tokio::task::block_in_place(|| drop(lock_custody_root(custody_id)));
            },
        )
        .await;
        assert!(!run.contention_acknowledged);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run.assert_store_stayed_free("blocking transition");
            }))
            .is_err(),
            "a path that blocks under the Store without acknowledging must fail"
        );
    }

    /// A path whose contention starts long after the run began (a slow sandbox
    /// allocation ahead of the transition) still gets the full hold and probe
    /// samples measured from the acknowledgement. The old timed hold started
    /// before the path and would already have released.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn late_contention_is_measured_from_the_acknowledgement() {
        let (_dir, store) = store();
        let custody_id = Uuid::new_v4();
        let waiting_store = Arc::clone(&store);
        let started = Instant::now();
        let run = run_behind_held_stripes(
            &store,
            Some(vec![custody_id]),
            Duration::from_millis(600),
            async move {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let (_store, _root) = lock_store_then_root(&waiting_store, custody_id).await;
            },
        )
        .await;
        let elapsed = started.elapsed();
        assert!(run.contention_acknowledged);
        run.assert_store_stayed_free("late contention");
        assert!(
            elapsed >= Duration::from_millis(2000),
            "the stripe was held {:?} after the acknowledgement, not from run start",
            elapsed
        );
    }

    /// The probe is not vacuous: a path that acknowledges contention and then
    /// pins the Store is caught by the probe bound.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_path_that_pins_the_store_after_acknowledging_fails_the_probe_bound() {
        let (_dir, store) = store();
        let custody_id = Uuid::new_v4();
        let pinning_store = Arc::clone(&store);
        let run = run_behind_held_stripes(
            &store,
            Some(vec![custody_id]),
            Duration::from_millis(2500),
            async move {
                let _store = pinning_store.lock().await;
                signal_admission_contention(custody_id);
                tokio::task::block_in_place(|| std::thread::sleep(Duration::from_millis(1800)));
            },
        )
        .await;
        assert!(run.contention_acknowledged);
        assert!(
            run.worst_wait >= PROBE_BOUND,
            "the probe saw only {:?} while the Store was pinned",
            run.worst_wait
        );
    }

    /// An extra probe (an outer registry the path might pin across its stripe
    /// wait) is held to the same bound as the Store probe.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_extra_probe_pinned_across_the_wait_fails_the_probe_bound() {
        let (_dir, store) = store();
        let registry = Arc::new(tokio::sync::RwLock::new(()));
        let probed = Arc::clone(&registry);
        let pinning = Arc::clone(&registry);
        let custody_id = Uuid::new_v4();
        let waiting_store = Arc::clone(&store);
        let run = run_behind_held_stripes_within(
            &store,
            Some(vec![custody_id]),
            Duration::from_millis(2500),
            ACKNOWLEDGEMENT_TIMEOUT,
            vec![("registry", Box::new(move || drop(probed.blocking_read())))],
            async move {
                // The registry guard is held across the stripe wait, the Store
                // is not: only the extra probe can see it.
                let _registry = pinning.write().await;
                let (_store, _root) = lock_store_then_root(&waiting_store, custody_id).await;
            },
        )
        .await;
        assert!(run.contention_acknowledged);
        assert!(run.worst_wait < PROBE_BOUND, "the Store stayed free");
        let probe = run.extra_probes.first().expect("the registry probe ran");
        assert!(
            probe.worst_wait >= PROBE_BOUND,
            "the registry probe saw only {:?} while the registry was pinned",
            probe.worst_wait
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run.assert_store_stayed_free("pinned registry");
            }))
            .is_err()
        );
    }
}
