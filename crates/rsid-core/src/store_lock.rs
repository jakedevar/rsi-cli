//! Tokio mutex with a holder label for every acquire. Plain acquires capture
//! the caller in a synchronous entry point, before a future can be polled on
//! another thread. Explicit labels override the default caller location.

use std::panic::Location;
use std::sync::Arc;

/// A static label or the original acquire site; formatting allocates only at
/// the logging boundary, never on the acquire/release hot path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    Explicit(&'static str),
    Caller(&'static Location<'static>),
}

impl std::fmt::Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Explicit(label) => f.write_str(label),
            Self::Caller(location) => write!(f, "{}:{}", location.file(), location.line()),
        }
    }
}

/// The shared Store's mutex. The raw mutex is private so no acquire bypasses
/// holder recording. This type deliberately does not dereference to Tokio's
/// mutex. The store and daemon share it through this leaf crate.
#[derive(Debug)]
pub struct Mutex<T> {
    inner: Arc<tokio::sync::Mutex<T>>,
}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(value)),
        }
    }

    #[track_caller]
    pub fn lock(&self) -> impl std::future::Future<Output = LabeledGuard<'_, T>> {
        self.lock_at(Location::caller())
    }

    /// Preserve a synchronous wrapper's caller through its async retry loop.
    pub fn lock_at(
        &self,
        caller: &'static Location<'static>,
    ) -> impl std::future::Future<Output = LabeledGuard<'_, T>> {
        let label = Label::Caller(caller);
        async move {
            if let Ok(guard) = self.inner.try_lock() {
                return LabeledGuard::record(self, label, None, guard);
            }
            let holder = note_contention(addr(self));
            let guard = self.inner.lock().await;
            LabeledGuard::record(self, label, Some(holder), guard)
        }
    }

    #[track_caller]
    pub fn try_lock(&self) -> Result<LabeledGuard<'_, T>, tokio::sync::TryLockError> {
        let guard = self.inner.try_lock()?;
        Ok(LabeledGuard::record(
            self,
            Label::Caller(Location::caller()),
            None,
            guard,
        ))
    }

    #[track_caller]
    pub fn blocking_lock(&self) -> LabeledGuard<'_, T> {
        let label = Label::Caller(Location::caller());
        if let Ok(guard) = self.inner.try_lock() {
            return LabeledGuard::record(self, label, None, guard);
        }
        let holder = note_contention(addr(self));
        let guard = self.inner.blocking_lock();
        LabeledGuard::record(self, label, Some(holder), guard)
    }

    #[track_caller]
    pub fn lock_owned(self: Arc<Self>) -> impl std::future::Future<Output = OwnedMutexGuard<T>> {
        let label = Label::Caller(Location::caller());
        async move {
            let key = addr(&self);
            let (guard, waited_behind) = match Arc::clone(&self.inner).try_lock_owned() {
                Ok(guard) => (guard, None),
                Err(_) => {
                    let holder = note_contention(key);
                    (Arc::clone(&self.inner).lock_owned().await, Some(holder))
                }
            };
            table().push(Held {
                mutex: key,
                label,
                since: Instant::now(),
            });
            OwnedMutexGuard {
                mutex: key,
                waited_behind,
                guard,
                _owner: self,
            }
        }
    }

    #[track_caller]
    pub fn try_lock_owned(
        self: Arc<Self>,
    ) -> Result<OwnedMutexGuard<T>, tokio::sync::TryLockError> {
        let guard = Arc::clone(&self.inner).try_lock_owned()?;
        let key = addr(&self);
        table().push(Held {
            mutex: key,
            label: Label::Caller(Location::caller()),
            since: Instant::now(),
        });
        Ok(OwnedMutexGuard {
            mutex: key,
            waited_behind: None,
            guard,
            _owner: self,
        })
    }

    pub fn into_inner(self) -> T {
        // The inner Arc is private; borrowed guards prevent moving self and
        // owned guards retain the outer Arc, so unwrap always succeeds here.
        Arc::try_unwrap(self.inner)
            .ok()
            .expect("mutex has no outstanding guard")
            .into_inner()
    }
}

/// Owned Store guard used by the Remote read budget.
pub struct OwnedMutexGuard<T> {
    mutex: usize,
    waited_behind: Option<Holder>,
    guard: tokio::sync::OwnedMutexGuard<T>,
    _owner: Arc<Mutex<T>>,
}

impl<T> OwnedMutexGuard<T> {
    pub fn waited_behind(&self) -> Option<Holder> {
        self.waited_behind
    }
}

impl<T> Deref for OwnedMutexGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}
impl<T> DerefMut for OwnedMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}
impl<T> Drop for OwnedMutexGuard<T> {
    fn drop(&mut self) {
        forget_holder(self.mutex);
    }
}

fn forget_holder(mutex: usize) {
    let mut held = table();
    if let Some(index) = held.iter().position(|entry| entry.mutex == mutex) {
        held.swap_remove(index);
    }
}

use std::ops::{Deref, DerefMut};
use std::sync::Mutex as StdMutex;
use std::time::Instant;

/// Lock-wait or operation time that earns a warning (matches the persistence
/// worker's slow-command threshold).
const WARN_AFTER_MS: u64 = 100;

struct Held {
    mutex: usize,
    label: Label,
    since: Instant,
}

/// A contended acquire: who held the lock when this acquirer started waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Holder {
    pub label: Label,
    pub held_ms: u64,
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

fn addr<T>(mutex: &Mutex<T>) -> usize {
    mutex as *const Mutex<T> as usize
}

fn holder_of(mutex: usize) -> Option<Holder> {
    let held = table();
    let entry = held.iter().find(|entry| entry.mutex == mutex)?;
    Some(Holder {
        label: entry.label,
        held_ms: entry.since.elapsed().as_millis() as u64,
    })
}

/// Current labelled holder and its live age for watchdog trip evidence.
/// Never waits for either the Store or the bookkeeping table: the watchdog
/// must still reach its restart when an executor thread is stuck.
pub fn holder_snapshot<T>(mutex: &Mutex<T>) -> String {
    snapshot_of(addr(mutex), &HELD)
}

fn snapshot_of(mutex: usize, table: &StdMutex<Vec<Held>>) -> String {
    let held = match table.try_lock() {
        Ok(held) => held,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return "<holder table busy>".into(),
    };
    match held.iter().find(|entry| entry.mutex == mutex) {
        Some(entry) => format!(
            "label={} held_ms={}",
            entry.label,
            entry.since.elapsed().as_millis()
        ),
        None => "<no labelled holder: unlocked or unlabelled>".into(),
    }
}

fn note_contention(mutex: usize) -> Holder {
    let holder = holder_of(mutex).unwrap_or(Holder {
        label: Label::Explicit("acquire/release transition"),
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
pub fn contention_since(since: Instant) -> Option<Holder> {
    let last = LAST_CONTENTION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let record = last.as_ref()?;
    (record.at >= since).then_some(record.holder)
}

/// A held lock that records its label for the guard's lifetime.
pub struct LabeledGuard<'a, T> {
    mutex: usize,
    waited_behind: Option<Holder>,
    // Declared after the table entry is removed in `Drop`, so the label is
    // gone before the lock is released.
    guard: tokio::sync::MutexGuard<'a, T>,
}

impl<T> LabeledGuard<'_, T> {
    /// Who held the lock when this acquire started waiting; `None` when it was
    /// free.
    pub fn waited_behind(&self) -> Option<Holder> {
        self.waited_behind
    }
}

impl<'a, T> LabeledGuard<'a, T> {
    fn record(
        mutex: &'a Mutex<T>,
        label: Label,
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
        forget_holder(self.mutex);
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
#[track_caller]
pub fn lock<'a, T>(
    mutex: &'a Mutex<T>,
    label: &'static str,
) -> impl std::future::Future<Output = LabeledGuard<'a, T>> {
    async move {
        if let Ok(guard) = mutex.inner.try_lock() {
            return LabeledGuard::record(mutex, Label::Explicit(label), None, guard);
        }
        let waited_behind = note_contention(addr(mutex));
        let guard = mutex.inner.lock().await;
        LabeledGuard::record(mutex, Label::Explicit(label), Some(waited_behind), guard)
    }
}

/// `mutex.try_lock()`, recording `label` as the holder; `None` when the lock
/// is held (nothing waits, so no contention is noted).
#[track_caller]
pub fn try_lock<'a, T>(mutex: &'a Mutex<T>, label: &'static str) -> Option<LabeledGuard<'a, T>> {
    let guard = mutex.inner.try_lock().ok()?;
    Some(LabeledGuard::record(
        mutex,
        Label::Explicit(label),
        None,
        guard,
    ))
}

/// `mutex.blocking_lock()`, recording `label` as the holder. Call from a
/// blocking thread, as `blocking_lock` itself requires.
#[track_caller]
pub fn blocking_lock<'a, T>(mutex: &'a Mutex<T>, label: &'static str) -> LabeledGuard<'a, T> {
    if let Ok(guard) = mutex.inner.try_lock() {
        return LabeledGuard::record(mutex, Label::Explicit(label), None, guard);
    }
    let waited_behind = note_contention(addr(mutex));
    let guard = mutex.inner.blocking_lock();
    LabeledGuard::record(mutex, Label::Explicit(label), Some(waited_behind), guard)
}

/// Run `op` under the labelled lock and warn, keeping the persistence worker's
/// existing `store_lock_wait_ms` / `store_operation_ms` fields, when either
/// phase is slow. The warning adds the holder this call waited behind.
#[track_caller]
pub fn run_timed_blocking<T, R>(
    mutex: &Mutex<T>,
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
            lock_holder = holder
                .map_or(Label::Explicit("none"), |holder| holder.label)
                .to_string()
                .as_str(),
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

    #[test]
    fn lock_wait_warning_names_the_holder_label() {
        let mutex = Arc::new(Mutex::new(0u32));
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

    #[test]
    fn released_lock_leaves_no_holder_entry() {
        let mutex = Mutex::new(());
        let key = addr(&mutex);
        {
            let _guard = blocking_lock(&mutex, "test::scoped");
            assert_eq!(
                holder_of(key).map(|holder| holder.label),
                Some(Label::Explicit("test::scoped"))
            );
        }
        assert_eq!(holder_of(key), None);
    }
    #[test]
    fn try_lock_records_the_holder_label_and_refuses_a_held_lock() {
        let mutex = Mutex::new(());
        let key = addr(&mutex);
        let guard = try_lock(&mutex, "test::try_holder").expect("free lock");
        assert_eq!(
            holder_of(key).map(|holder| holder.label),
            Some(Label::Explicit("test::try_holder"))
        );
        assert!(try_lock(&mutex, "test::second").is_none());
        drop(guard);
        assert_eq!(holder_of(key), None);
    }

    #[test]
    fn watchdog_snapshot_names_current_holder_and_live_age() {
        // A private table keeps concurrent lock users from making this test
        // observe the intentionally nonblocking "table busy" fallback.
        let held = StdMutex::new(vec![Held {
            mutex: 1,
            label: Label::Explicit("test::watchdog_holder"),
            since: Instant::now() - Duration::from_secs(104),
        }]);
        let snapshot = snapshot_of(1, &held);
        assert!(
            snapshot.starts_with("label=test::watchdog_holder held_ms="),
            "{snapshot}"
        );
        let age: u64 = snapshot.split("held_ms=").nth(1).unwrap().parse().unwrap();
        assert!(age >= 104_000, "{snapshot}");
        held.lock().unwrap().clear();
        assert_eq!(
            snapshot_of(1, &held),
            "<no labelled holder: unlocked or unlabelled>"
        );
    }

    #[test]
    fn watchdog_snapshot_does_not_wait_for_holder_table() {
        let held = StdMutex::new(Vec::new());
        let _locked = held.lock().unwrap();
        assert_eq!(snapshot_of(1, &held), "<holder table busy>");
    }
}

#[cfg(test)]
mod caller_tests {
    use super::*;

    #[track_caller]
    fn wrapper<T>(mutex: &Mutex<T>) -> impl std::future::Future<Output = LabeledGuard<'_, T>> {
        mutex.lock()
    }

    fn assert_site<T>(mutex: &Mutex<T>, line: u32) {
        let label = holder_of(addr(mutex))
            .expect("acquire records holder")
            .label;
        assert_eq!(label.to_string(), format!("{}:{line}", file!()));
    }

    #[tokio::test]
    async fn plain_async_acquire_preserves_caller_through_wrapper_and_deferred_poll() {
        let mutex = Mutex::new(());
        let line = line!() + 1;
        let future = wrapper(&mutex);
        tokio::task::yield_now().await;
        let guard = future.await;
        assert_site(&mutex, line);
        assert!(mutex.try_lock().is_err());
        drop(guard);
        assert_eq!(holder_of(addr(&mutex)), None);
    }

    #[test]
    fn plain_try_and_blocking_acquires_report_original_caller() {
        let mutex = Mutex::new(());
        let line = line!() + 1;
        let guard = mutex.try_lock().unwrap();
        assert_site(&mutex, line);
        drop(guard);
        let line = line!() + 1;
        let guard = mutex.blocking_lock();
        assert_site(&mutex, line);
        drop(guard);
        assert_eq!(holder_of(addr(&mutex)), None);
    }

    #[tokio::test]
    async fn owned_acquire_keeps_holder_alive_until_release() {
        let mutex = Arc::new(Mutex::new(1));
        let line = line!() + 1;
        let guard = Arc::clone(&mutex).lock_owned().await;
        assert_site(&mutex, line);
        assert!(Arc::try_unwrap(Arc::clone(&mutex)).is_err());
        drop(guard);
        assert_eq!(holder_of(addr(&mutex)), None);
        let line = line!() + 1;
        let guard = Arc::clone(&mutex).try_lock_owned().unwrap();
        assert_site(&mutex, line);
        assert!(Arc::clone(&mutex).try_lock_owned().is_err());
        drop(guard);
        assert_eq!(Arc::try_unwrap(mutex).unwrap().into_inner(), 1);
    }
}
