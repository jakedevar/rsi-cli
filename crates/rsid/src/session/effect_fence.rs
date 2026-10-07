//! #1277-#1279: the effect-time authority recheck shared by the verbs that
//! admit under the store lock, release it for an async step (a Git probe, a
//! directory probe, staging binaries), and then create a durable row.
//!
//! The verb resolves its authority and the fence
//! ([`crate::store::Store::agent_authority_fence`]); after the await it
//! re-runs the same admission under the store lock it keeps through the
//! insert, and refuses with the verb's existing typed code unless the fence is
//! unchanged.

use crate::error::{DaemonError, Result};

/// Refuse `denied` when the authority resolved at the effect differs from the
/// one admitted.
pub(crate) fn require_unchanged(admitted: &str, current: &str, denied: &'static str) -> Result<()> {
    if admitted == current {
        Ok(())
    } else {
        Err(DaemonError::PolicyDenied(denied.into()))
    }
}

/// A test hook at the await point between admission and effect, so a test can
/// revoke or replace the grant in exactly that window.
#[cfg(test)]
pub(crate) mod seam {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use uuid::Uuid;

    type Hook = Box<dyn FnOnce(&crate::store::Store) + Send>;

    fn hooks() -> &'static Mutex<HashMap<(Uuid, &'static str), Hook>> {
        static HOOKS: OnceLock<Mutex<HashMap<(Uuid, &'static str), Hook>>> = OnceLock::new();
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Install a hook for the next `point` of `caller`.
    pub(crate) fn install(
        caller: Uuid,
        point: &'static str,
        hook: impl FnOnce(&crate::store::Store) + Send + 'static,
    ) {
        hooks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((caller, point), Box::new(hook));
    }

    pub(crate) async fn run(
        store: &tokio::sync::Mutex<crate::store::Store>,
        caller: Uuid,
        point: &'static str,
    ) {
        let hook = hooks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(caller, point));
        if let Some(hook) = hook {
            hook(&*store.lock().await);
        }
    }
}
