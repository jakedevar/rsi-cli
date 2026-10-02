//! Monotonic live-call tracking for admitted model invocations (#1080).
//!
//! Every [`AdmissionPermit`](super::AdmissionPermit) carries an `Arc` lease
//! registered here. While any clone of the permit is alive the call has a live
//! in-process handle; when the last clone drops the lease unregisters. The
//! stale-helper sweeper queries this registry instead of trusting wall-clock
//! ledger timestamps, which a clock jump or a long healthy stream would
//! otherwise turn into a live helper being failed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use uuid::Uuid;

fn registry() -> &'static Mutex<HashMap<Uuid, Weak<CallLease>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<Uuid, Weak<CallLease>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn lock_registry() -> std::sync::MutexGuard<'static, HashMap<Uuid, Weak<CallLease>>> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Liveness lease of one admitted invocation. Activity is tracked with the
/// monotonic clock only.
#[derive(Debug)]
pub(crate) struct CallLease {
    invocation_id: Uuid,
    last_activity: Mutex<Instant>,
}

impl CallLease {
    pub(crate) fn register(invocation_id: Uuid) -> Arc<Self> {
        let lease = Arc::new(Self {
            invocation_id,
            last_activity: Mutex::new(Instant::now()),
        });
        lock_registry().insert(invocation_id, Arc::downgrade(&lease));
        lease
    }

    pub(crate) fn touch(&self) {
        *self
            .last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
    }
}

impl Drop for CallLease {
    fn drop(&mut self) {
        let mut registry = lock_registry();
        // A newer lease for the same id (a test re-registering) must survive.
        if registry
            .get(&self.invocation_id)
            .is_some_and(|weak| weak.strong_count() == 0)
        {
            registry.remove(&self.invocation_id);
        }
    }
}

/// What the sweeper may conclude about one still-`running` ledger row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallLiveness {
    /// A permit handle is alive; the call was last active `idle` ago.
    Live { idle: Duration },
    /// No live in-process handle exists for the invocation.
    Orphaned,
}

pub(crate) fn liveness(invocation_id: Uuid) -> CallLiveness {
    let lease = lock_registry().get(&invocation_id).and_then(Weak::upgrade);
    match lease {
        Some(lease) => CallLiveness::Live {
            idle: lease.idle_for(),
        },
        None => CallLiveness::Orphaned,
    }
}
