//! Holder labels for the shared `Arc<tokio::sync::Mutex<Store>>` (#327, hub #2971).
//!
//! The lock-wait warnings used to say the Store was busy but not who held it,
//! and a periodic holder that is not CPU-hot never shows in a stack sample.
//! A labelled acquire records a `&'static str` and its start time in a small
//! process-wide table keyed by the mutex address while the guard lives. A
//! contended acquirer snapshots that entry (label and held-so-far) before it
//! blocks, so the warning it prints after the wait still names who it waited
//! behind. The hot path takes one short `std` mutex per acquire and release,
//! never allocates once the table has grown, and logs nothing.
//!
//! Unlabelled acquires stay valid: they record nothing and a waiter behind one
//! reports the holder as [`UNLABELLED`]. Label only the busiest sites.

use std::ops::{Deref, DerefMut};
use std::sync::Mutex as StdMutex;
use std::time::Instant;

/// What a contended acquirer prints when the holder never recorded a label.
pub(crate) const UNLABELLED: &str = "unlabelled";

/// Lock-wait or operation time that earns a warning (matches the persistence
/// worker's slow-command threshold).
const WARN_AFTER_MS: u64 = 100;

struct Held {
    mutex: usize,
    label: &'static str,
    since: Instant,
}

/// A contended acquire: who held the lock when this acquirer started waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Holder {
    pub(crate) label: &'static str,
    pub(crate) held_ms: u64,
}

struct Contention {
    /// Read only by the test that waits for a specific mutex's contention.
    #[cfg(test)]
    mutex: usize,
    at: Instant,
    holder: Holder,
}

static HELD: StdMutex<Vec<Held>> = StdMutex::new(Vec::new());
static LAST_CONTENTION: StdMutex<Option<Contention>> = StdMutex::new(None);

fn table() -> std::sync::MutexGuard<'static, Vec<Held>> {
    HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn addr<T>(mutex: &tokio::sync::Mutex<T>) -> usize {
    mutex as *const tokio::sync::Mutex<T> as usize
}

fn holder_of(mutex: usize) -> Option<Holder> {
    let held = table();
    let entry = held.iter().find(|entry| entry.mutex == mutex)?;
    Some(Holder {
        label: entry.label,
        held_ms: entry.since.elapsed().as_millis() as u64,
    })
}

fn note_contention(mutex: usize) -> Holder {
    let holder = holder_of(mutex).unwrap_or(Holder {
        label: UNLABELLED,
        held_ms: 0,
    });
    let record = Contention {
        #[cfg(test)]
        mutex,
        at: Instant::now(),
        holder,
    };
    *LAST_CONTENTION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(record);
    holder
}

/// The most recent contended acquire since `since`, for warnings that cover a
/// span with no lock handle of their own (the persistence worker's slow
/// command).
pub(crate) fn contention_since(since: Instant) -> Option<Holder> {
    let last = LAST_CONTENTION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let record = last.as_ref()?;
    (record.at >= since).then_some(record.holder)
}

/// A held lock that records its label for the guard's lifetime.
pub(crate) struct LabeledGuard<'a, T> {
    mutex: usize,
    waited_behind: Option<Holder>,
    // Declared after the table entry is removed in `Drop`, so the label is
    // gone before the lock is released.
    guard: tokio::sync::MutexGuard<'a, T>,
}

impl<T> LabeledGuard<'_, T> {
    /// Who held the lock when this acquire started waiting; `None` when it was
    /// free.
    pub(crate) fn waited_behind(&self) -> Option<Holder> {
        self.waited_behind
    }
}

impl<'a, T> LabeledGuard<'a, T> {
    fn record(
        mutex: &'a tokio::sync::Mutex<T>,
        label: &'static str,
        waited_behind: Option<Holder>,
        guard: tokio::sync::MutexGuard<'a, T>,
    ) -> Self {
        let mutex = addr(mutex);
        table().push(Held {
            mutex,
            label,
            since: Instant::now(),
        });
        Self {
            mutex,
            waited_behind,
            guard,
        }
    }
}

impl<T> Drop for LabeledGuard<'_, T> {
    fn drop(&mut self) {
        let mut held = table();
        if let Some(index) = held.iter().position(|entry| entry.mutex == self.mutex) {
            held.swap_remove(index);
        }
    }
}

impl<T> Deref for LabeledGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for LabeledGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

/// `mutex.lock().await`, recording `label` as the holder.
pub(crate) async fn lock<'a, T>(
    mutex: &'a tokio::sync::Mutex<T>,
    label: &'static str,
) -> LabeledGuard<'a, T> {
    if let Ok(guard) = mutex.try_lock() {
        return LabeledGuard::record(mutex, label, None, guard);
    }
    let waited_behind = note_contention(addr(mutex));
    let guard = mutex.lock().await;
    LabeledGuard::record(mutex, label, Some(waited_behind), guard)
}

/// `mutex.try_lock()`, recording `label` as the holder; `None` when the lock
/// is held (nothing waits, so no contention is noted).
pub(crate) fn try_lock<'a, T>(
    mutex: &'a tokio::sync::Mutex<T>,
    label: &'static str,
) -> Option<LabeledGuard<'a, T>> {
    let guard = mutex.try_lock().ok()?;
    Some(LabeledGuard::record(mutex, label, None, guard))
}

/// `mutex.blocking_lock()`, recording `label` as the holder. Call from a
/// blocking thread, as `blocking_lock` itself requires.
pub(crate) fn blocking_lock<'a, T>(
    mutex: &'a tokio::sync::Mutex<T>,
    label: &'static str,
) -> LabeledGuard<'a, T> {
    if let Ok(guard) = mutex.try_lock() {
        return LabeledGuard::record(mutex, label, None, guard);
    }
    let waited_behind = note_contention(addr(mutex));
    let guard = mutex.blocking_lock();
    LabeledGuard::record(mutex, label, Some(waited_behind), guard)
}

/// Run `op` under the labelled lock and warn, keeping the persistence worker's
/// existing `store_lock_wait_ms` / `store_operation_ms` fields, when either
/// phase is slow. The warning adds the holder this call waited behind.
pub(crate) fn run_timed_blocking<T, R>(
    mutex: &tokio::sync::Mutex<T>,
    label: &'static str,
    op: impl FnOnce(&mut T) -> R,
) -> R {
    let lock_started = Instant::now();
    let mut guard = blocking_lock(mutex, label);
    let store_lock_wait_ms = lock_started.elapsed().as_millis() as u64;
    let operation_started = Instant::now();
    let result = op(&mut *guard);
    let store_operation_ms = operation_started.elapsed().as_millis() as u64;
    if store_lock_wait_ms >= WARN_AFTER_MS || store_operation_ms >= WARN_AFTER_MS {
        let holder = guard.waited_behind();
        tracing::warn!(
            store_lock_wait_ms,
            store_operation_ms,
            lock_holder = holder.map_or("none", |holder| holder.label),
            // The holder's hold so far when this call began waiting, plus the
            // wait: its whole hold when it was the one that released to us.
            lock_holder_held_ms = holder.map_or(0, |holder| holder.held_ms + store_lock_wait_ms),
            lock_label = label,
            "Store operation timing"
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<StdMutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn recorded_for(mutex: usize) -> Option<Holder> {
        let last = LAST_CONTENTION.lock().unwrap();
        let record = last.as_ref()?;
        (record.mutex == mutex).then_some(record.holder)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn lock_wait_warning_names_the_holder_label() {
        let mutex = Arc::new(tokio::sync::Mutex::new(0u32));
        let key = addr(&mutex);
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        let holder_mutex = Arc::clone(&mutex);
        let holder = std::thread::spawn(move || {
            let mut guard = blocking_lock(&holder_mutex, "test::manager_intent_pass");
            *guard += 1;
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.recv().unwrap();

        let logs = LogBuffer::default();
        let writer = logs.clone();
        let waiter_mutex = Arc::clone(&mutex);
        let waiter = std::thread::spawn(move || {
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                run_timed_blocking(&waiter_mutex, "test::waiter", |value| {
                    *value += 1;
                    *value
                })
            })
        });

        // The waiter records its contention before it blocks; release the
        // holder only once it has, and only after the wait outlasts the
        // warning threshold.
        let deadline = Instant::now() + Duration::from_secs(10);
        while recorded_for(key).is_none() {
            assert!(Instant::now() < deadline, "waiter never contended");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(WARN_AFTER_MS + 40));
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert_eq!(waiter.join().unwrap(), 2);

        let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(captured.contains("Store operation timing"), "{captured}");
        assert!(captured.contains("store_lock_wait_ms="), "{captured}");
        assert!(captured.contains("store_operation_ms="), "{captured}");
        assert!(
            captured.contains("lock_holder=\"test::manager_intent_pass\""),
            "{captured}"
        );
        let held_ms: u64 = captured
            .split("lock_holder_held_ms=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no lock_holder_held_ms in {captured}"));
        assert!(held_ms >= WARN_AFTER_MS, "{captured}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn released_lock_leaves_no_holder_entry() {
        let mutex = tokio::sync::Mutex::new(());
        let key = addr(&mutex);
        {
            let _guard = blocking_lock(&mutex, "test::scoped");
            assert_eq!(
                holder_of(key).map(|holder| holder.label),
                Some("test::scoped")
            );
        }
        assert_eq!(holder_of(key), None);
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn try_lock_records_the_holder_label_and_refuses_a_held_lock() {
        let mutex = tokio::sync::Mutex::new(());
        let key = addr(&mutex);
        let guard = try_lock(&mutex, "test::try_holder").expect("free lock");
        assert_eq!(
            holder_of(key).map(|holder| holder.label),
            Some("test::try_holder")
        );
        assert!(try_lock(&mutex, "test::second").is_none());
        drop(guard);
        assert_eq!(holder_of(key), None);
    }

    /// #3021: the manager coordinator (10 s) and the manager action backstop
    /// (10 s) hold the shared Store through these files. An unlabelled holder
    /// shows as `unlabelled` in a lock-wait warning and hides which pass
    /// kept the Store; every acquire in them names itself.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn periodic_manager_paths_label_every_store_acquire() {
        let files: [(&str, &str); 8] = [
            (
                "manager_actions",
                include_str!("session/manager_actions.rs"),
            ),
            (
                "manager_actions/recovery",
                include_str!("session/manager_actions/recovery.rs"),
            ),
            (
                "manager_coordinator",
                include_str!("session/manager_coordinator.rs"),
            ),
            (
                "manager_issue_worker",
                include_str!("session/manager_issue_worker.rs"),
            ),
            (
                "manager_succession",
                include_str!("session/manager_succession.rs"),
            ),
            ("manager_reseat", include_str!("session/manager_reseat.rs")),
            ("remote_answers", include_str!("session/remote_answers.rs")),
            (
                "pending_approvals",
                include_str!("session/pending_approvals.rs"),
            ),
        ];
        let mut unlabelled = Vec::new();
        for (name, source) in files {
            let squeezed: String = source.chars().filter(|c| !c.is_whitespace()).collect();
            for needle in [
                "store.lock().await",
                "store.try_lock()",
                "store.blocking_lock()",
            ] {
                let count = squeezed.matches(needle).count();
                if count > 0 {
                    unlabelled.push(format!("{name}: {count} x {needle}"));
                }
            }
        }
        assert!(
            unlabelled.is_empty(),
            "use crate::store_lock::lock(&store, \"<module>::<fn>\"): {unlabelled:?}"
        );
        assert!(
            include_str!("session/lifecycle.rs")
                .contains("\"lifecycle::check_manager_notice_resume\""),
            "the manager notice-resume Store acquire lost its label"
        );
    }
}
