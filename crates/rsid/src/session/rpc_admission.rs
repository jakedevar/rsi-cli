//! Copied transport identity for one RPC, with admission at the owning seam.
//!
//! A task-local scope carries only identity and the registry Arc, never an A6
//! guard. Tokio does not inherit it into spawned tasks: admitted durable work
//! retains its existing claim/generation fences, not reusable RPC authority.

use super::{AgentTokenRegistry, BoundAgentCaller};
use crate::error::{DaemonError, Result};
use std::sync::Arc;
use tokio::sync::{OwnedRwLockReadGuard, RwLock};
use uuid::Uuid;

tokio::task_local! {
    pub(crate) static RPC_ADMISSION: Option<RpcAdmission>;
}

#[derive(Clone)]
pub(crate) struct RpcAdmission {
    pub(crate) caller: BoundAgentCaller,
    pub(crate) registry: Arc<RwLock<AgentTokenRegistry>>,
}

pub(crate) fn unknown_caller() -> DaemonError {
    DaemonError::InvalidParam("agent_verb_unknown_session_token".into())
}

pub(crate) fn current() -> Option<RpcAdmission> {
    RPC_ADMISSION.try_with(Clone::clone).ok().flatten()
}

impl RpcAdmission {
    fn check(&self, registry: &AgentTokenRegistry, caller: Uuid) -> Result<()> {
        if self.caller.session_id() != caller
            || self.caller.legacy_session_id().is_none()
            || !registry.caller_matches(self.caller)
        {
            return Err(unknown_caller());
        }
        Ok(())
    }

    /// Only call after acquiring owning locks. The returned guard must remain
    /// in the bounded synchronous snapshot/transaction block; acquire no other
    /// locks and perform no await, I/O, dispatch or serialization within it.
    pub(crate) async fn admit(
        &self,
        caller: Uuid,
    ) -> Result<OwnedRwLockReadGuard<AgentTokenRegistry>> {
        let registry = Arc::clone(&self.registry).read_owned().await;
        self.check(&registry, caller)?;
        Ok(registry)
    }

    /// Blocking counterpart for a synchronous ledger: acquire that ledger
    /// before this call, in a spawn_blocking task, never on a runtime thread.
    pub(crate) fn with_admission<T>(&self, caller: Uuid, effect: impl FnOnce() -> T) -> Result<T> {
        let registry = self.registry.blocking_read();
        self.check(&registry, caller)?;
        Ok(effect())
    }
}

/// Internal/native calls retain their existing authority paths. An RPC must
/// keep its original resolution all the way to the effect, including remints
/// that happen to reinstall the same synthetic bearer.
pub(crate) async fn admit_current_rpc(
    caller: Uuid,
) -> Result<Option<OwnedRwLockReadGuard<AgentTokenRegistry>>> {
    match current() {
        Some(admission) => admission.admit(caller).await.map(Some),
        None => Ok(None),
    }
}
